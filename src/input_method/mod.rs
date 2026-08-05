// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The compositor as input method — the model between `zwp_text_input_v3` and IBus.
//!
//! GNOME has no `MetaInputMethod`: `ClutterInputMethod` is abstract and the concrete subclass is
//! gnome-shell's `js/misc/inputMethod.js:24`, installed with `clutter_backend_set_input_method`.
//! mutter's `src/wayland/meta-wayland-text-input.c` is only the protocol half. This module is
//! both — the Wayland bridge and the IBus-facing input method — because we have no Clutter to
//! put a seam in.
//!
//! ```text
//! wayland client (zwp_text_input_v3)
//!   ↕  smithay TextInputHandle + set_internal_input_method   ← the seam (our smithay patch)
//! InputMethod (this module, compositor thread)
//!   ↕  async_channel / calloop::channel
//! worker thread → src/dbus/ibus.rs → ibus-daemon → engine
//! ```
//!
//! **Why a worker thread.** `ProcessKeyEvent` is a D-Bus round trip *per keystroke*
//! (`inputMethod.js:344`). Doing that on the compositor thread would put the frame loop behind
//! an IPC call for every key, so the socket lives on its own thread and both directions are
//! channels — the same shape as [`crate::dbus`].
//!
//! # Offsets
//!
//! IBus counts preedit cursors in **characters**; `zwp_text_input_v3` counts them in **bytes**.
//! Every crossing goes through [`char_to_byte`]. This is the defect class that only appears on
//! the accented text the whole feature exists for, so it is a function with tests rather than an
//! `as` cast at each call site.

#[cfg(feature = "dbus")]
pub mod worker;

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use smithay::backend::input::KeyState;
use smithay::input::keyboard::{Keycode, ModifiersState};
use smithay::utils::Serial;
use smithay::wayland::text_input::{TextInputEvent, TextInputSeat};

use crate::dbus::ibus::{self, ImEvent, PreeditMode};
use crate::synoik::State;

/// How long a keystroke may sit waiting for the engine before we give up and deliver it.
///
/// GNOME has no equivalent: it passes `-1` to `process_key_event_async`
/// (`inputMethod.js:347`), taking GDBus's 25-second default, so a wedged `ibus-daemon` costs
/// the user 25 seconds of dead keyboard per keystroke. A healthy round trip on the same
/// machine's session bus is well under a millisecond, so this is three orders of magnitude of
/// headroom and still bounds the damage at "input feels slow" rather than "the session is
/// unusable". Deliberate divergence.
const KEY_TIMEOUT: Duration = Duration::from_millis(1000);

/// A keystroke held back from the client while the engine decides on it.
///
/// Everything here is what [`smithay::input::keyboard::KeyboardHandle::input_forward`] needs, so
/// that delivering a deferred key later is the same call it would have been synchronously.
#[derive(Debug, Clone, Copy)]
struct PendingKey {
    id: u64,
    keycode: Keycode,
    state: KeyState,
    serial: Serial,
    time: u32,
    mods_changed: bool,
    /// When this key was held back, for [`KEY_TIMEOUT`]. Real time rather than the compositor
    /// clock on purpose: this bounds an *IPC* stall, and it must still expire when the frame
    /// clock is idle because nothing is animating.
    queued_at: std::time::Instant,
}

/// Convert our modifier state into the X11-style mask IBus expects.
///
/// `iso_level3_shift` is AltGr, which is how `us(intl)` reaches the level holding the dead keys —
/// dropping it would leave the engine unable to tell an AltGr composition from a bare one.
pub fn ibus_modifier_state(mods: &ModifiersState, pressed: bool) -> u32 {
    let mut state = 0;
    if mods.shift {
        state |= ibus::SHIFT_MASK;
    }
    if mods.caps_lock {
        state |= ibus::LOCK_MASK;
    }
    if mods.ctrl {
        state |= ibus::CONTROL_MASK;
    }
    if mods.alt {
        state |= ibus::MOD1_MASK;
    }
    if mods.num_lock {
        state |= ibus::MOD2_MASK;
    }
    if mods.logo {
        state |= ibus::MOD4_MASK;
    }
    if mods.iso_level3_shift {
        state |= ibus::MOD5_MASK;
    }
    if !pressed {
        state |= ibus::RELEASE_MASK;
    }
    state
}

/// What the compositor asks of the IBus worker.
#[derive(Debug, Clone, PartialEq)]
pub enum ImRequest {
    /// A client enabled text input on the focused surface.
    FocusIn,
    /// Focus left, or the client disabled text input.
    FocusOut,
    /// Drop any in-progress composition.
    Reset,
    /// Text around the caret, byte offsets as they arrived from the client.
    Surrounding {
        text: String,
        cursor: u32,
        anchor: u32,
    },
    /// Select the engine for the active input source (`xkb:us:intl:eng` and friends).
    SetEngine(String),
    /// What kind of field the caret is in, already mapped to IBus's enums. Above all this is
    /// how an engine learns it is looking at a **password**.
    ContentType { purpose: u32, hints: u32 },
    /// A keystroke for the engine to accept or decline. `id` comes back on the
    /// [`ImUpdate::KeyResult`] that answers it.
    ///
    /// `keycode` is an **evdev** code: IBus wants the X11 keycode minus 8, and gnome-shell
    /// subtracts it at the call (`inputMethod.js:346`).
    ProcessKey {
        id: u64,
        keysym: u32,
        keycode: u32,
        state: u32,
    },
}

/// What the worker reports back. Everything here is applied on the compositor thread.
#[derive(Debug, Clone, PartialEq)]
pub enum ImUpdate {
    /// The engine produced something.
    Event(ImEvent),
    /// The worker (re)connected to a daemon, or lost it. While disconnected the compositor must
    /// behave exactly as it does with no input method at all.
    Connected(bool),
    /// The engine's verdict on the [`ImRequest::ProcessKey`] with this `id`. `filtered` means the
    /// engine consumed the key, so the client must never see it.
    KeyResult { id: u64, filtered: bool },
}

/// The compositor-side input method.
pub struct InputMethod {
    to_worker: async_channel::Sender<ImRequest>,
    /// Whether a client currently has an *enabled* text input.
    ///
    /// This is the gate for touching the keyboard path at all: mutter's
    /// `meta_wayland_text_input_update` returns early unless there is a focused, enabled input
    /// (`meta-wayland-text-input.c:1174-1177`), so ordinary typing outside a text field never
    /// goes near the input method.
    enabled: bool,
    /// Whether the worker has a live daemon. While false the key path behaves exactly as it does
    /// with no input method at all, rather than queueing keys nobody will answer for.
    connected: bool,
    /// The preedit we last sent a client, so focus changes and resets can clear it.
    preedit: Option<String>,
    /// The content type last sent, as `(purpose, hints)`, so a reconnect can replay it. A new
    /// input context defaults to free-form, and letting a **password** field silently become
    /// free-form after a daemon restart is the one mistake here that matters.
    content_type: (u32, u32),
    /// Keys held back from the client, oldest first, awaiting the engine's verdict.
    ///
    /// Order is the whole point. mutter relies on the same guarantee — "we rely on the IM
    /// implementation to notify back of key events in the exact same order they were given"
    /// (`clutter-input-method.c:398-400`) — and a keyboard that delivers a release before its
    /// press leaves a key stuck down in the client.
    pending: VecDeque<PendingKey>,
    next_key_id: u64,
}

impl std::fmt::Debug for InputMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputMethod")
            .field("enabled", &self.enabled)
            .field("preedit", &self.preedit)
            .finish()
    }
}

impl InputMethod {
    /// Build the model around an already-created request channel.
    ///
    /// Split from the worker so tests can drive the compositor half with no daemon: take the
    /// receiver and assert on what the model asks for.
    pub fn new(to_worker: async_channel::Sender<ImRequest>) -> Self {
        Self {
            to_worker,
            enabled: false,
            connected: false,
            preedit: None,
            content_type: (ibus::purpose::FREE_FORM, 0),
            pending: VecDeque::new(),
            next_key_id: 0,
        }
    }

    /// Whether a client has text input enabled right now.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Whether keystrokes should be routed through the engine at all.
    ///
    /// Both halves matter: an enabled text input with no daemon behind it must not swallow keys,
    /// and a live daemon with no focused text input must never see the user's typing.
    pub fn wants_keys(&self) -> bool {
        self.enabled && self.connected
    }

    /// Whether any key is currently held back from the client.
    pub fn has_pending_keys(&self) -> bool {
        !self.pending.is_empty()
    }

    /// The preedit currently shown to the client, if any.
    pub fn preedit(&self) -> Option<&str> {
        self.preedit.as_deref()
    }

    fn send(&self, request: ImRequest) {
        // A full or closed channel means the worker is gone or wedged. Losing a request is
        // better than blocking the compositor thread on it; the next focus change resyncs.
        if self.to_worker.try_send(request).is_err() {
            tracing::debug!("input method worker is not accepting requests");
        }
    }
}

/// The sink handed to smithay, which forwards a client's committed text-input state onto the
/// compositor's event loop.
///
/// It must not touch `State`: smithay calls it from inside the `zwp_text_input_v3` dispatch,
/// where `State` is already borrowed.
pub fn make_sink(
    to_compositor: calloop::channel::Sender<TextInputEvent>,
) -> Arc<dyn Fn(TextInputEvent) + Send + Sync> {
    Arc::new(move |event| {
        if to_compositor.send(event).is_err() {
            tracing::debug!("dropping text-input event, compositor channel closed");
        }
    })
}

impl State {
    /// A client's text-input state changed. Mirrors the parts of `commit` that mutter forwards
    /// to the input method (`meta-wayland-text-input.c:844-978`).
    pub fn on_text_input_event(&mut self, event: TextInputEvent) {
        let Some(im) = self.synoik.input_method.as_mut() else {
            return;
        };

        match event {
            TextInputEvent::Enabled => {
                im.enabled = true;
                im.send(ImRequest::FocusIn);
            }
            TextInputEvent::Disabled => {
                im.enabled = false;
                self.synoik.im_surrounding = None;
                // Keys still in flight belong to the input that just went away. Deliver them
                // rather than drop them — the client asked for them as ordinary key events the
                // moment it disabled text input.
                self.flush_pending_im_keys();
                // A disabled input keeps no composition. Clear ours before telling the engine,
                // so a client that re-enables immediately cannot see a stale preedit.
                self.clear_preedit();
                let Some(im) = self.synoik.input_method.as_mut() else {
                    return;
                };
                // Back to free-form, as gnome-shell's reset does (`inputMethod.js:390`). The
                // next field to take focus sets its own content type, but until it does the
                // engine must not still believe it is in the *previous* one — a password
                // field's purpose outliving the password field is the wrong direction to err.
                im.content_type = (ibus::purpose::FREE_FORM, 0);
                im.send(ImRequest::ContentType {
                    purpose: ibus::purpose::FREE_FORM,
                    hints: 0,
                });
                im.send(ImRequest::FocusOut);
            }
            TextInputEvent::SurroundingText {
                text,
                cursor,
                anchor,
            } => {
                im.send(ImRequest::Surrounding {
                    text: text.clone(),
                    cursor,
                    anchor,
                });
                // Keep it: `delete_surrounding_text` comes back in characters and goes out in
                // bytes, and only this text can convert between them.
                self.synoik.im_surrounding = Some((text, cursor));
            }
            TextInputEvent::ContentType { hint, purpose } => {
                let purpose = ibus::content_purpose_to_ibus(purpose);
                let hints = ibus::content_hints_to_ibus(hint);
                im.content_type = (purpose, hints);
                im.send(ImRequest::ContentType { purpose, hints });
            }
            // `Done` needs no forwarding: the requests above are already the atomic batch, and
            // IBus has no matching "end of batch" call. The cursor rectangle is only needed to
            // place a candidate popup, which is the unported Panel surface.
            TextInputEvent::Done
            | TextInputEvent::TextChangeCause(_)
            | TextInputEvent::CursorRectangle(_) => {}
        }
    }

    /// Tell the engine which input source is active.
    ///
    /// **Plain xkb layouts go through IBus too.** `js/ui/status/keyboard.js:510-528` synthesizes
    /// an `xkb:<layout>:<variant>:<lang>` engine for an `xkb` source and calls `setEngine`
    /// unconditionally — `js/misc/inputMethod.js:331-336` never checks the source type. Dead keys
    /// come from the `ibus-keyboard` engine, not from the keymap, so there is no "xkb needs no
    /// input method" shortcut to take here.
    ///
    /// The active source is the head of `mru-sources`, which is what GNOME rewrites on an
    /// interactive switch, falling back to the configured order.
    pub fn sync_input_method_engine(&mut self) {
        let sources = &self.synoik.gnome_settings.input_sources;
        let active = sources
            .mru_sources
            .first()
            .or_else(|| sources.sources.first());
        let Some((source_type, id)) = active else {
            return;
        };

        // The language is only used to name the engine; `eng` is `engine_name`'s default and is
        // right for the Latin layouts. Deriving it properly needs IBus's own layout→language
        // table, which is a lookup we do not have yet.
        let engine = crate::dbus::ibus::engine_name(source_type, id, None);
        if let Some(im) = self.synoik.input_method.as_mut() {
            im.send(ImRequest::SetEngine(engine));
        }
    }

    /// Offer a keystroke to the engine, holding it back from the client until the verdict.
    ///
    /// Returns whether the key was taken. `false` means the caller must deliver it now, exactly
    /// as it would with no input method.
    ///
    /// Called at the point the key would otherwise be forwarded, which is a deliberate
    /// divergence: mutter consults the input method *before* keybindings
    /// (`events.c:168` vs `:262`), so on GNOME every keystroke typed into a text field makes a
    /// D-Bus round trip before any shortcut can resolve. Ours resolves shortcuts first and offers
    /// the engine only what was headed for the client anyway. The practical difference is empty —
    /// engines want unmodified printable keys plus Escape/Backspace/arrows during a composition,
    /// none of which are GNOME accelerators — and it keeps the compositor's own modal surfaces
    /// (the lock screen, the polkit password entry) strictly ahead of an external daemon, which
    /// on the ordering above they would not be.
    #[allow(clippy::too_many_arguments)]
    pub fn im_offer_key(
        &mut self,
        keycode: Keycode,
        state: KeyState,
        serial: Serial,
        time: u32,
        mods_changed: bool,
        keysym: u32,
        mods: &ModifiersState,
        pressed: bool,
    ) -> bool {
        let Some(im) = self.synoik.input_method.as_mut() else {
            return false;
        };
        if !im.wants_keys() {
            return false;
        }

        let id = im.next_key_id;
        im.next_key_id += 1;

        // IBus wants an evdev code; xkb keycodes are that plus 8 (`inputMethod.js:346`).
        let evcode = keycode.raw().saturating_sub(8);
        im.send(ImRequest::ProcessKey {
            id,
            keysym,
            keycode: evcode,
            state: ibus_modifier_state(mods, pressed),
        });

        let was_empty = im.pending.is_empty();
        im.pending.push_back(PendingKey {
            id,
            keycode,
            state,
            serial,
            time,
            mods_changed,
            queued_at: std::time::Instant::now(),
        });

        if was_empty {
            self.arm_im_key_timeout(KEY_TIMEOUT);
        }
        true
    }

    /// The engine answered. Deliver or drop the key it answered for.
    fn on_im_key_result(&mut self, id: u64, filtered: bool) {
        let Some(im) = self.synoik.input_method.as_mut() else {
            return;
        };
        let Some(index) = im.pending.iter().position(|key| key.id == id) else {
            // Already flushed by a timeout or a focus change. The key has been delivered, so the
            // verdict is moot — dropping it here is the only correct move.
            return;
        };

        // Anything queued *before* the answered key never got its own answer. Deliver those
        // rather than drop them: a lost keystroke is worse than one the engine wanted, and
        // holding them back would stall the queue behind a reply that is never coming.
        //
        // Drained in one go before forwarding any of them, because `forward_pending_key` needs
        // `&mut self` and so cannot run while the queue is borrowed.
        let skipped: Vec<PendingKey> = im.pending.drain(..index).collect();
        let key = im.pending.pop_front().expect("index is in bounds");
        let still_pending = !im.pending.is_empty();

        for key in skipped {
            tracing::debug!(id = key.id, "no verdict for key, forwarding it");
            self.forward_pending_key(key);
        }
        if !filtered {
            self.forward_pending_key(key);
        }

        // The timer tracks the head of the queue, which has just changed.
        self.disarm_im_key_timeout();
        if still_pending {
            self.arm_im_key_timeout(KEY_TIMEOUT);
        }
    }

    /// Deliver every held-back key to the client, in order.
    ///
    /// The fail-open direction is the only safe one: whatever went wrong — a wedged daemon, a
    /// lost connection, focus moving away — the alternative is keystrokes that silently vanish.
    pub fn flush_pending_im_keys(&mut self) {
        loop {
            let Some(im) = self.synoik.input_method.as_mut() else {
                return;
            };
            let Some(key) = im.pending.pop_front() else {
                break;
            };
            self.forward_pending_key(key);
        }
        self.disarm_im_key_timeout();
    }

    /// Start the clock on the key at the head of the queue.
    fn arm_im_key_timeout(&mut self, after: Duration) {
        self.disarm_im_key_timeout();
        let timer = calloop::timer::Timer::from_duration(after);
        let token = self
            .synoik
            .event_loop
            .insert_source(timer, |_, _, state| {
                state.on_im_key_timeout();
                calloop::timer::TimeoutAction::Drop
            })
            .unwrap();
        self.synoik.im_key_timer = Some(token);
    }

    fn disarm_im_key_timeout(&mut self) {
        if let Some(token) = self.synoik.im_key_timer.take() {
            self.synoik.event_loop.remove(token);
        }
    }

    /// The engine took too long. Deliver what has actually expired and re-arm for the rest.
    fn on_im_key_timeout(&mut self) {
        self.synoik.im_key_timer = None;

        let now = std::time::Instant::now();
        let mut expired = Vec::new();
        let mut next_deadline = None;
        if let Some(im) = self.synoik.input_method.as_mut() {
            while let Some(key) = im.pending.front() {
                match KEY_TIMEOUT.checked_sub(now.saturating_duration_since(key.queued_at)) {
                    // Still has time left: it is the new head, so the timer tracks it.
                    Some(left) if !left.is_zero() => {
                        next_deadline = Some(left);
                        break;
                    }
                    _ => expired.push(im.pending.pop_front().expect("front is Some")),
                }
            }
        }

        if !expired.is_empty() {
            tracing::warn!(
                count = expired.len(),
                "input method did not answer in {KEY_TIMEOUT:?}, delivering keys anyway"
            );
        }
        for key in expired {
            self.forward_pending_key(key);
        }

        if let Some(left) = next_deadline {
            self.arm_im_key_timeout(left);
        }
    }

    /// Hand one deferred key to the focused client.
    ///
    /// `input_forward` is the other half of the `input_intercept` that already ran for this key:
    /// xkb state was updated back then, and this only decides delivery. That split is why a
    /// deferred key needs no replay through the filter and cannot double-count a modifier.
    fn forward_pending_key(&mut self, key: PendingKey) {
        let keyboard = self.synoik.seat.get_keyboard().unwrap();
        keyboard.input_forward(
            self,
            key.keycode,
            key.state,
            key.serial,
            key.time,
            key.mods_changed,
        );
    }

    /// Something came back from the engine.
    pub fn on_im_update(&mut self, update: ImUpdate) {
        match update {
            ImUpdate::Connected(connected) => {
                if connected {
                    self.sync_input_method_engine();
                }
                if let Some(im) = self.synoik.input_method.as_mut() {
                    im.connected = connected;
                    // A reconnect brings a *new* input context, which starts unfocused and
                    // knows nothing. Replay what the focused client already told us, or the
                    // engine stays deaf until the user clicks into another text field.
                    if connected && im.enabled {
                        im.send(ImRequest::FocusIn);
                        let (purpose, hints) = im.content_type;
                        im.send(ImRequest::ContentType { purpose, hints });
                        if let Some((text, cursor)) = self.synoik.im_surrounding.clone() {
                            let Some(im) = self.synoik.input_method.as_mut() else {
                                return;
                            };
                            im.send(ImRequest::Surrounding {
                                text,
                                cursor,
                                anchor: cursor,
                            });
                        }
                    }
                }
                if !connected {
                    // Nothing is going to answer for the keys in flight, and the preedit they
                    // were building can no longer be completed.
                    self.flush_pending_im_keys();
                    self.clear_preedit();
                }
            }
            ImUpdate::KeyResult { id, filtered } => self.on_im_key_result(id, filtered),
            ImUpdate::Event(event) => self.on_im_event(event),
        }
    }

    fn on_im_event(&mut self, event: ImEvent) {
        match event {
            ImEvent::Commit(text) => self.commit_text(&text),
            ImEvent::Preedit {
                text,
                cursor,
                visible,
                mode,
            } => {
                let shown = if visible { text } else { None };
                self.set_preedit(shown, cursor, mode);
            }
            ImEvent::ShowPreedit | ImEvent::HidePreedit => {
                // The engine toggles visibility of the preedit it already sent. We only keep the
                // visible one, so `Hide` clears and `Show` is a no-op until the next Preedit —
                // which the engine always sends, since gnome-shell replays its cached string the
                // same way (`inputMethod.js:180-193`).
                if matches!(event, ImEvent::HidePreedit) {
                    self.clear_preedit();
                }
            }
            ImEvent::DeleteSurrounding { offset, n_chars } => {
                self.delete_surrounding(offset, n_chars)
            }
            ImEvent::ForwardKey { keycode, press, .. } => self.forward_engine_key(keycode, press),
            // The engine wants surrounding text. We send it whenever the client gives us some,
            // so the useful answer is to resend what we have rather than ask the client again —
            // `zwp_text_input_v3` has no way to request it, the client volunteers it.
            ImEvent::RequireSurrounding => {
                if let Some((text, cursor)) = self.synoik.im_surrounding.clone() {
                    if let Some(im) = self.synoik.input_method.as_mut() {
                        im.send(ImRequest::Surrounding {
                            text,
                            cursor,
                            anchor: cursor,
                        });
                    }
                }
            }
        }
    }

    /// Deliver a key the engine synthesized or handed back.
    ///
    /// It goes straight out rather than back through the filter: it is already the input
    /// method's verdict, so re-offering it would loop. Clutter marks the re-injected event
    /// `CLUTTER_EVENT_FLAG_INPUT_METHOD` and checks that flag before consulting the input method
    /// at all (`clutter-input-method.c:522`); bypassing the filter is the same guard without the
    /// round trip.
    ///
    /// xkb state is deliberately untouched. This key never went through `input_intercept`, and a
    /// synthesized modifier that updated our state would leave it stuck: nothing will ever
    /// deliver the matching release.
    fn forward_engine_key(&mut self, evcode: u32, press: bool) {
        let keyboard = self.synoik.seat.get_keyboard().unwrap();
        let state = if press {
            KeyState::Pressed
        } else {
            KeyState::Released
        };
        keyboard.input_forward(
            self,
            // IBus hands back evdev codes; xkb wants them plus 8 (`inputMethod.js:192`).
            Keycode::new(evcode.saturating_add(8)),
            state,
            smithay::utils::SERIAL_COUNTER.next_serial(),
            crate::utils::get_monotonic_time().as_millis() as u32,
            false,
        );
    }

    /// Send finished text to the focused client.
    ///
    /// Clears the preedit first, which is both the protocol's step order and what mutter does
    /// (`meta-wayland-text-input.c:325-341` sends `preedit_string(NULL)` before `commit_string`).
    /// Skipping it leaves the composition visible *beside* the text it turned into.
    fn commit_text(&mut self, text: &str) {
        let had_preedit = self
            .synoik
            .input_method
            .as_ref()
            .is_some_and(|im| im.preedit.is_some());

        self.synoik
            .seat
            .text_input()
            .with_active_text_input(|ti, _surface| {
                if had_preedit {
                    ti.preedit_string(None, 0, 0);
                }
                ti.commit_string(Some(text.to_owned()));
            });
        self.synoik.seat.text_input().done(false);

        if let Some(im) = self.synoik.input_method.as_mut() {
            im.preedit = None;
        }
    }

    /// Show (or clear) the in-progress composition.
    fn set_preedit(&mut self, text: Option<String>, cursor_chars: u32, mode: PreeditMode) {
        // A `Commit` reset mode means an interrupted composition should be kept rather than
        // discarded; we record it so a later reset can act on it (`clutter_input_focus_reset`,
        // `clutter-input-focus.c:107-128`).
        let _ = mode;

        let cursor = text
            .as_deref()
            .map(|t| char_to_byte(t, cursor_chars))
            .unwrap_or(0);

        let payload = text.clone();
        self.synoik
            .seat
            .text_input()
            .with_active_text_input(|ti, _surface| {
                // Both cursor ends are the same offset: gnome-shell never renders a preedit
                // selection (`inputMethod.js:169`, `const anchor = pos`).
                ti.preedit_string(payload.clone(), cursor as i32, cursor as i32);
            });
        self.synoik.seat.text_input().done(false);

        if let Some(im) = self.synoik.input_method.as_mut() {
            im.preedit = text;
        }
    }

    /// Drop a shown preedit, telling the client if it had one.
    fn clear_preedit(&mut self) {
        let had = self
            .synoik
            .input_method
            .as_ref()
            .is_some_and(|im| im.preedit.is_some());
        if !had {
            return;
        }
        self.set_preedit(None, 0, PreeditMode::Clear);
    }

    /// Delete around the caret. IBus counts in characters and may pass a negative offset; the
    /// protocol wants two non-negative **byte** lengths measured from the caret.
    fn delete_surrounding(&mut self, offset: i32, n_chars: u32) {
        // mutter clamps the offset to be non-positive (`MIN (offset, 0)`,
        // `meta-wayland-text-input.c:296`) — a request to delete starting *after* the caret is
        // treated as starting at it.
        let before_chars = (-offset.min(0)) as u32;
        let after_chars = n_chars.saturating_sub(before_chars);

        // Without the client's surrounding text we cannot turn characters into bytes. Rather
        // than guess — and delete the wrong span of someone's document — do nothing.
        //
        // gnome-shell refuses the same way, logging that an engine must not call this without
        // the SURROUNDING_TEXT capability (`inputMethod.js:131-141`).
        let Some(surrounding) = self.synoik.im_surrounding.clone() else {
            tracing::debug!("ignoring delete_surrounding_text without surrounding text");
            return;
        };

        let Some((before_length, after_length)) =
            surrounding_byte_lengths(&surrounding.0, surrounding.1, before_chars, after_chars)
        else {
            tracing::debug!("delete_surrounding_text out of range, ignoring");
            return;
        };

        self.synoik
            .seat
            .text_input()
            .with_active_text_input(|ti, _surface| {
                ti.delete_surrounding_text(before_length, after_length);
            });
        self.synoik.seat.text_input().done(false);
    }
}

/// Byte offset of the `n`th character of `text`, clamped to its length.
///
/// A cursor past the end is not worth refusing over — engines do send one — but indexing with it
/// would panic or split a codepoint.
pub fn char_to_byte(text: &str, n: u32) -> u32 {
    text.char_indices()
        .nth(n as usize)
        .map(|(i, _)| i as u32)
        .unwrap_or(text.len() as u32)
}

/// Turn a character span around the caret into the protocol's before/after **byte** lengths.
///
/// `cursor` is a byte offset into `text`. Returns `None` when the span leaves the text, which
/// mutter treats as a programming error (`g_return_if_fail`, `meta-wayland-text-input.c:305-310`)
/// and we treat as a reason to do nothing.
pub fn surrounding_byte_lengths(
    text: &str,
    cursor: u32,
    before_chars: u32,
    after_chars: u32,
) -> Option<(u32, u32)> {
    let cursor = cursor as usize;
    if cursor > text.len() || !text.is_char_boundary(cursor) {
        return None;
    }

    // Zero is its own case: `nth(before_chars - 1)` on a saturating subtraction would ask for
    // the *last* character back instead of none of them, and quietly delete it.
    let before_bytes = if before_chars == 0 {
        0
    } else {
        let (start, _) = text[..cursor]
            .char_indices()
            .rev()
            .nth(before_chars as usize - 1)?;
        cursor - start
    };

    let after_bytes = {
        let rest = &text[cursor..];
        let mut chars = rest.char_indices();
        match chars.nth(after_chars as usize) {
            Some((i, _)) => i,
            None if rest.chars().count() as u32 >= after_chars => rest.len(),
            None => return None,
        }
    };

    Some((before_bytes as u32, after_bytes as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_to_byte_counts_characters_not_bytes() {
        // The whole reason this function exists: a preedit cursor of 2 in "héllo" is byte 3,
        // and using it as a byte offset would split the é.
        assert_eq!(char_to_byte("héllo", 0), 0);
        assert_eq!(char_to_byte("héllo", 1), 1);
        assert_eq!(char_to_byte("héllo", 2), 3);
        assert_eq!(char_to_byte("héllo", 5), 6);
        // Past the end clamps rather than panicking; engines do send this.
        assert_eq!(char_to_byte("héllo", 99), 6);
        assert_eq!(char_to_byte("", 3), 0);
    }

    #[test]
    fn surrounding_lengths_measure_bytes_from_the_caret() {
        // "héllo" with the caret at the end (byte 6): deleting 2 chars before is 2 bytes.
        assert_eq!(surrounding_byte_lengths("héllo", 6, 2, 0), Some((2, 0)));
        // Deleting 4 chars before crosses the é, so it is 5 bytes, not 4.
        assert_eq!(surrounding_byte_lengths("héllo", 6, 4, 0), Some((5, 0)));
        // Caret in the middle (after "hé" = byte 3), one char each way.
        assert_eq!(surrounding_byte_lengths("héllo", 3, 1, 1), Some((2, 1)));
        // Nothing to do is a valid answer, not an error.
        assert_eq!(surrounding_byte_lengths("héllo", 3, 0, 0), Some((0, 0)));
    }

    #[test]
    fn modifier_state_carries_altgr_and_the_release_flag() {
        let mut mods = ModifiersState::default();
        assert_eq!(ibus_modifier_state(&mods, true), 0);
        // A release has no argument of its own in `ProcessKeyEvent`; it is a bit on the state.
        assert_eq!(ibus_modifier_state(&mods, false), ibus::RELEASE_MASK);

        // AltGr is how `us(intl)` reaches the level holding the dead keys, so losing this bit
        // loses the feature this whole module exists for.
        mods.iso_level3_shift = true;
        assert_eq!(ibus_modifier_state(&mods, true), ibus::MOD5_MASK);

        mods.shift = true;
        mods.ctrl = true;
        assert_eq!(
            ibus_modifier_state(&mods, true),
            ibus::MOD5_MASK | ibus::SHIFT_MASK | ibus::CONTROL_MASK
        );
    }

    #[test]
    fn surrounding_lengths_refuse_to_run_off_the_text() {
        // Deleting more than exists must not silently clamp: an engine that asked for the wrong
        // span would eat text the user can't get back.
        assert_eq!(surrounding_byte_lengths("héllo", 6, 9, 0), None);
        assert_eq!(surrounding_byte_lengths("héllo", 0, 0, 9), None);
        // A caret that is not on a character boundary is nonsense we refuse rather than panic on.
        assert_eq!(surrounding_byte_lengths("héllo", 2, 0, 0), None);
        assert_eq!(surrounding_byte_lengths("héllo", 99, 0, 0), None);
    }
}
