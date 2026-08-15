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

pub mod worker;

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use smithay::backend::input::KeyState;
use smithay::input::keyboard::{Keycode, Keysym, ModifiersState};
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
pub(crate) const KEY_TIMEOUT: Duration = Duration::from_millis(1000);

/// How many consecutive keys may expire unanswered before we stop holding keys at all.
///
/// A wedged engine used to cost a full [`KEY_TIMEOUT`] on *every* keystroke, indefinitely, while
/// emitting one identical warning per key — 579 of them across 18 minutes on a live session
/// (2026-08-15) before anyone worked out it was the input method. Three strikes is enough to tell
/// "the engine is thinking" from "the engine is gone".
const UNRESPONSIVE_AFTER: u32 = 3;

/// One of the compositor's own text entries — the surfaces that are *not* Wayland clients and so
/// have no `zwp_text_input_v3` of their own.
///
/// GNOME reaches these through the same `ClutterInputFocus` a client gets, because there every
/// entry is a `ClutterText` (`clutter-text.c:292-531`) and the shell is just another actor. We
/// have no such common substrate, so the entries are enumerated instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellEntry {
    /// The lock screen's password field.
    Shield,
    /// Alt-F2.
    RunDialog,
    /// The polkit authentication dialog's password field.
    Polkit,
    /// Renaming an app folder in the overview.
    FolderRename,
    /// The overview's search entry.
    OverviewSearch,
}

impl ShellEntry {
    /// Whether this entry holds a secret, and must be announced to the engine as such.
    pub fn is_password(self) -> bool {
        matches!(self, Self::Shield | Self::Polkit)
    }

    /// The content type to announce for it, already in IBus's enums.
    fn content_type(self) -> (u32, u32) {
        if self.is_password() {
            // What GNOME's `StPasswordEntry` amounts to: `PURPOSE_PASSWORD`
            // (`st-password-entry.c:241`) plus the hints a hidden, sensitive field implies.
            (
                ibus::purpose::PASSWORD,
                ibus::hints::PRIVATE | ibus::hints::HIDDEN_TEXT,
            )
        } else {
            (ibus::purpose::FREE_FORM, 0)
        }
    }
}

/// A keystroke bound for one of the compositor's own entries, carrying everything the entry's
/// handler will need once the engine has had its say.
#[derive(Debug, Clone, Copy)]
pub struct ShellKey {
    pub entry: ShellEntry,
    pub raw: Option<Keysym>,
    /// The character the key produces, already filtered for ctrl/alt/logo by the caller.
    pub text: Option<char>,
    pub mods: crate::ui::text_edit::EditMods,
    pub theme: crate::ui::text_edit::KeyTheme,
    pub pressed: bool,
}

/// Where a held-back key goes once the engine declines it.
#[derive(Debug, Clone, Copy)]
enum KeyDest {
    /// A Wayland client. The fields are what
    /// [`smithay::input::keyboard::KeyboardHandle::input_forward`] needs, so that delivering the
    /// key later is the same call it would have been synchronously.
    Client {
        keycode: Keycode,
        state: KeyState,
        serial: Serial,
        time: u32,
        mods_changed: bool,
    },
    /// One of our own entries.
    Shell(ShellKey),
}

/// A keystroke held back while the engine decides on it.
#[derive(Debug, Clone, Copy)]
struct PendingKey {
    id: u64,
    dest: KeyDest,
    /// When this key was held back, for [`KEY_TIMEOUT`]. Real time rather than the compositor
    /// clock on purpose: this bounds an *IPC* stall, and it must still expire when the frame
    /// clock is idle because nothing is animating.
    queued_at: std::time::Instant,
}

/// Who the input method is currently focused on.
///
/// One at a time, exactly as in GNOME: `ClutterInputMethod` has a single `focus`
/// (`clutter-input-method.c:544`), and a shell entry taking key focus focuses *out* whatever had
/// it. That matters here because a modal dialog can open over a client whose text input is still
/// enabled — the client must not keep the engine while the user types into the dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ImFocus {
    #[default]
    None,
    Client,
    Shell(ShellEntry),
}

/// Whether a keysym can *begin* a composition while producing no character of its own.
///
/// This is the whole point of the feature and the easiest thing to miss: `dead_acute.key_char()`
/// is `None`. Gating what reaches the engine on "carries text" therefore drops precisely the key
/// that starts a dead-key sequence, and composition never begins. It cost a working overview
/// search and a broken lock screen to notice, because the overview accidentally routed dead keys
/// to the engine down the *client* fall-through path while the lock screen — which intercepts
/// unconditionally — had no such accident to rely on.
///
/// The range is xkb's contiguous block of `dead_*` keysyms (`XKB_KEY_dead_grave` through
/// `XKB_KEY_dead_greek` and the later additions), plus `Multi_key`, which opens a Compose
/// sequence the same way.
pub fn begins_composition(sym: Option<Keysym>) -> bool {
    let Some(sym) = sym else {
        return false;
    };
    let raw = sym.raw();
    (0xfe50..=0xfe93).contains(&raw) || sym == Keysym::Multi_key
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
    /// This is one input to [`ImFocus`], not the gate itself: a client can have text input
    /// enabled while a modal dialog of ours holds the keyboard, and then the *dialog* owns the
    /// engine.
    client_enabled: bool,
    /// The content type the focused client declared, kept separate from the effective one so
    /// that focus moving to a shell entry and back restores it.
    client_content_type: (u32, u32),
    /// Who the engine is focused on. Nothing touches the key path while this is
    /// [`ImFocus::None`]: mutter's `meta_wayland_text_input_update` likewise returns early
    /// without a focused, enabled input (`meta-wayland-text-input.c:1174-1177`), so ordinary
    /// typing outside a text field never goes near the input method.
    focus: ImFocus,
    /// Whether the worker has a live daemon. While false the key path behaves exactly as it does
    /// with no input method at all, rather than queueing keys nobody will answer for.
    connected: bool,
    /// The preedit we last showed, so focus changes and resets can clear it.
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
    /// Consecutive keys that expired without a verdict, and whether that has crossed
    /// [`UNRESPONSIVE_AFTER`]. See [`InputMethod::is_unresponsive`].
    unanswered: u32,
    unresponsive: bool,
}

impl std::fmt::Debug for InputMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputMethod")
            .field("focus", &self.focus)
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
            unanswered: 0,
            unresponsive: false,
            client_enabled: false,
            client_content_type: (ibus::purpose::FREE_FORM, 0),
            focus: ImFocus::None,
            connected: false,
            preedit: None,
            content_type: (ibus::purpose::FREE_FORM, 0),
            pending: VecDeque::new(),
            next_key_id: 0,
        }
    }

    /// Who the engine is focused on.
    pub fn focus(&self) -> ImFocus {
        self.focus
    }

    /// Whether the focused *client* has text input enabled.
    pub fn is_enabled(&self) -> bool {
        self.client_enabled
    }

    /// Whether keystrokes should be routed through the engine at all.
    ///
    /// Both halves matter: a focused entry with no daemon behind it must not swallow keys, and a
    /// live daemon with nothing focused must never see the user's typing.
    pub fn wants_keys(&self) -> bool {
        self.focus != ImFocus::None && self.connected
    }

    /// Whether the engine has stopped answering and keys are going straight through.
    ///
    /// Holding every keystroke for [`KEY_TIMEOUT`] behind an engine that will never reply is a
    /// second of latency *per key*, which is worse for the user than losing the input method —
    /// and it is indistinguishable from a wedged compositor. After [`UNRESPONSIVE_AFTER`]
    /// consecutive unanswered keys we stop holding them. We keep *asking*, so a single late
    /// verdict is enough to switch back (see `on_im_key_result`); recovery needs no reconnect.
    pub fn is_unresponsive(&self) -> bool {
        self.unresponsive
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
            // Enable and disable no longer focus the engine themselves: they feed `sync_im_focus`,
            // which weighs them against a modal entry of ours that may hold the keyboard.
            TextInputEvent::Enabled => {
                im.client_enabled = true;
                self.sync_im_focus();
            }
            TextInputEvent::Disabled => {
                im.client_enabled = false;
                // Back to free-form, as gnome-shell's reset does (`inputMethod.js:390`), so the
                // client's declared type cannot outlive the input that declared it.
                im.client_content_type = (ibus::purpose::FREE_FORM, 0);
                self.synoik.im_surrounding = None;
                self.sync_im_focus();
            }
            TextInputEvent::SurroundingText {
                text,
                cursor,
                anchor,
            } => {
                // Only meaningful while the client actually holds the engine; a modal entry of
                // ours owning it means this text is not what the user is editing.
                if im.focus == ImFocus::Client {
                    im.send(ImRequest::Surrounding {
                        text: text.clone(),
                        cursor,
                        anchor,
                    });
                }
                // Keep it regardless: `delete_surrounding_text` comes back in characters and goes
                // out in bytes, and only this text can convert between them.
                self.synoik.im_surrounding = Some((text, cursor));
            }
            TextInputEvent::ContentType { hint, purpose } => {
                let purpose = ibus::content_purpose_to_ibus(purpose);
                let hints = ibus::content_hints_to_ibus(hint);
                im.client_content_type = (purpose, hints);
                if im.focus == ImFocus::Client {
                    im.content_type = (purpose, hints);
                    im.send(ImRequest::ContentType { purpose, hints });
                }
            }
            // `Done` needs no forwarding: the requests above are already the atomic batch, and
            // IBus has no matching "end of batch" call. The cursor rectangle is only needed to
            // place a candidate popup, which is the unported Panel surface.
            TextInputEvent::Done
            | TextInputEvent::TextChangeCause(_)
            | TextInputEvent::CursorRectangle(_) => {}
        }
    }

    /// Which of the compositor's own entries currently owns typed text, if any.
    ///
    /// The order mirrors the key filter's, because that is what actually decides where a
    /// keystroke lands — deriving it from anything else would let the engine be focused on one
    /// entry while the keys went to another.
    fn shell_im_entry(&self) -> Option<ShellEntry> {
        if self.synoik.screen_shield.is_active() {
            return Some(ShellEntry::Shield);
        }
        if self.synoik.run_dialog.is_open() {
            return Some(ShellEntry::RunDialog);
        }
        if self.synoik.polkit_is_open() {
            return Some(ShellEntry::Polkit);
        }
        if self.synoik.keyboard_focus.is_overview() {
            if self.synoik.folder_dialog.is_renaming() {
                return Some(ShellEntry::FolderRename);
            }
            // The search entry counts as focused even before it is open: the keystroke that
            // *starts* a search has to be composable too.
            return Some(ShellEntry::OverviewSearch);
        }
        None
    }

    /// Move the engine's focus to whatever owns text right now.
    ///
    /// Call it after anything that can change that: keyboard focus, a dialog opening or closing,
    /// a client enabling or disabling its text input.
    pub fn sync_im_focus(&mut self) {
        let Some(im) = self.synoik.input_method.as_ref() else {
            return;
        };

        let desired = match self.shell_im_entry() {
            Some(entry) => ImFocus::Shell(entry),
            None if im.client_enabled => ImFocus::Client,
            None => ImFocus::None,
        };
        if desired == im.focus {
            return;
        }

        // Whatever was in flight belonged to the old focus. Deliver those keys and drop the
        // composition before anything hears about the new one, or a half-finished character
        // lands in the wrong entry — including, at worst, a password field.
        self.flush_pending_im_keys();
        self.clear_preedit();

        let Some(im) = self.synoik.input_method.as_mut() else {
            return;
        };
        let had_focus = im.focus != ImFocus::None;
        im.focus = desired;

        if had_focus {
            im.send(ImRequest::FocusOut);
            // Reset before anything else can happen on this context. A password purpose that
            // outlives the field it described is the one stale value here that matters, and
            // focusing out is not by itself documented to clear it. gnome-shell's reset does the
            // same (`inputMethod.js:390`).
            im.content_type = (ibus::purpose::FREE_FORM, 0);
            im.send(ImRequest::ContentType {
                purpose: ibus::purpose::FREE_FORM,
                hints: 0,
            });
        }
        if desired == ImFocus::None {
            return;
        }

        let (purpose, hints) = match desired {
            ImFocus::Shell(entry) => entry.content_type(),
            // Restore what the client declared; it does not re-send on every focus change.
            ImFocus::Client => im.client_content_type,
            ImFocus::None => unreachable!("returned above"),
        };
        im.content_type = (purpose, hints);
        im.send(ImRequest::FocusIn);
        im.send(ImRequest::ContentType { purpose, hints });
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

        // IBus wants an evdev code; xkb keycodes are that plus 8 (`inputMethod.js:346`).
        let evcode = keycode.raw().saturating_sub(8);
        let state_mask = ibus_modifier_state(mods, pressed);
        self.queue_im_key(
            keysym,
            evcode,
            state_mask,
            KeyDest::Client {
                keycode,
                state,
                serial,
                time,
                mods_changed,
            },
        )
    }

    /// Offer a keystroke bound for one of *our* entries to the engine.
    ///
    /// Returns whether it was taken; `false` means deliver it now.
    ///
    /// Narrower than the client path on purpose. Only keys that carry text — or any key at all
    /// once a composition is in progress — are offered. The compositor's entries sit at the
    /// bottom of ladders that fall through to other handlers (the overview's search gives way to
    /// grid navigation, then to hardcoded binds), and those fall-throughs have to produce a
    /// `FilterResult` synchronously. Deferring a key that might fall through would mean
    /// reimplementing each ladder in the delivery path; deferring only what the entry is certain
    /// to consume costs nothing, because an engine has no use for the rest.
    pub fn im_offer_shell_key(&mut self, key: ShellKey, keysym: u32, keycode: Keycode) -> bool {
        // Sync here rather than hunting down every state change that can open or close an entry:
        // a folder rename or a search entry engages without keyboard focus moving at all, and a
        // stale focus would silently mean no input method for that entry. Cheap — a handful of
        // predicate reads — and it runs only for keys already bound to one of our entries.
        self.sync_im_focus();

        let Some(im) = self.synoik.input_method.as_ref() else {
            return false;
        };
        if !im.wants_keys() || im.focus != ImFocus::Shell(key.entry) {
            return false;
        }
        // Presses only. Three of the five entries act on presses alone, and deferring a release
        // would mean the entry never seeing one the engine happened to consume — for a text
        // entry a no-op, but not worth the asymmetry. Releases keep their original path.
        if !key.pressed {
            return false;
        }
        // Text, a key that starts a composition, or any key at all once one is in flight — so
        // Backspace and Escape can cancel it. `begins_composition` is not optional: a dead key
        // produces no character, so a "carries text" test alone drops the very key the sequence
        // starts with.
        if key.text.is_none() && !begins_composition(key.raw) && im.preedit.is_none() {
            return false;
        }

        let mods = self.synoik.seat.get_keyboard().unwrap().modifier_state();
        let state_mask = ibus_modifier_state(&mods, key.pressed);
        self.queue_im_key(
            keysym,
            keycode.raw().saturating_sub(8),
            state_mask,
            KeyDest::Shell(key),
        )
    }

    /// Ask the engine about a key and hold it until the verdict comes back.
    ///
    /// Returns whether the key was actually held. While the engine is
    /// [unresponsive](InputMethod::is_unresponsive) we still **ask** — that is what lets a single
    /// late verdict switch holding back on — but we do not hold, so keys reach the client at once
    /// instead of one second late, forever.
    fn queue_im_key(&mut self, keysym: u32, keycode: u32, state: u32, dest: KeyDest) -> bool {
        let Some(im) = self.synoik.input_method.as_mut() else {
            return false;
        };
        let id = im.next_key_id;
        im.next_key_id += 1;

        im.send(ImRequest::ProcessKey {
            id,
            keysym,
            keycode,
            state,
        });

        if im.unresponsive {
            return false;
        }

        let was_empty = im.pending.is_empty();
        im.pending.push_back(PendingKey {
            id,
            dest,
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

        // Any verdict at all means the engine is answering again — including one for a key we no
        // longer hold, which is exactly what arrives while we are passing keys through. This is
        // the whole recovery path; without it the strike count could only ever rise.
        im.unanswered = 0;
        if im.unresponsive {
            im.unresponsive = false;
            tracing::warn!("input method is answering again; holding keys for it once more");
        }

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
        // The source returns `TimeoutAction::Drop`, so the token it left behind names nothing.
        self.synoik.im_key_timer = None;
        self.expire_im_keys_at(std::time::Instant::now());
    }

    /// The body of [`Self::on_im_key_timeout`], against a caller-supplied clock.
    ///
    /// The seam exists for the corpus. A held key expires against **real** time on purpose (it
    /// bounds an IPC stall, so it must fire with the frame clock idle), which leaves a test no way
    /// to reach this path except by sleeping a real [`KEY_TIMEOUT`] per key — and the behaviour
    /// worth pinning here takes three of them.
    pub(crate) fn expire_im_keys_at(&mut self, now: std::time::Instant) {
        // Remove rather than just forget: from the callback the source has already dropped itself
        // and the token is gone, so this is a no-op — but reaching the seam any other way would
        // otherwise leave a live timer armed behind us.
        self.disarm_im_key_timeout();

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
            let newly_unresponsive = if let Some(im) = self.synoik.input_method.as_mut() {
                im.unanswered = im.unanswered.saturating_add(expired.len() as u32);
                let crossed = !im.unresponsive && im.unanswered >= UNRESPONSIVE_AFTER;
                if crossed {
                    im.unresponsive = true;
                }
                crossed
            } else {
                false
            };
            // One line when it starts, one when it recovers — not one per keystroke forever.
            if newly_unresponsive {
                tracing::warn!(
                    "input method has not answered {UNRESPONSIVE_AFTER} keys in a row; \
                     passing keys straight through until it replies again"
                );
            } else if !self
                .synoik
                .input_method
                .as_ref()
                .is_some_and(|im| im.unresponsive)
            {
                tracing::warn!(
                    count = expired.len(),
                    "input method did not answer in {KEY_TIMEOUT:?}, delivering keys anyway"
                );
            }
        }
        for key in expired {
            self.forward_pending_key(key);
        }

        if let Some(left) = next_deadline {
            self.arm_im_key_timeout(left);
        }
    }

    /// Hand one deferred key to wherever it was bound.
    ///
    /// For a client, `input_forward` is the other half of the `input_intercept` that already ran:
    /// xkb state was updated back then, and this only decides delivery. That split is why a
    /// deferred key needs no replay through the filter and cannot double-count a modifier.
    fn forward_pending_key(&mut self, key: PendingKey) {
        match key.dest {
            KeyDest::Client {
                keycode,
                state,
                serial,
                time,
                mods_changed,
            } => {
                let keyboard = self.synoik.seat.get_keyboard().unwrap();
                keyboard.input_forward(self, keycode, state, serial, time, mods_changed);
            }
            KeyDest::Shell(key) => self.deliver_shell_key(key),
        }
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
                    // A reconnect brings a *new* input context, which starts unfocused and knows
                    // nothing. Replay whatever holds the focus, or the engine stays deaf until
                    // the user moves to another entry.
                    if connected && im.focus != ImFocus::None {
                        let (purpose, hints) = im.content_type;
                        let replay_surrounding = im.focus == ImFocus::Client;
                        im.send(ImRequest::FocusIn);
                        im.send(ImRequest::ContentType { purpose, hints });

                        if replay_surrounding {
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
        let focus = self
            .synoik
            .input_method
            .as_ref()
            .map_or(ImFocus::None, |im| im.focus);
        if let ImFocus::Shell(entry) = focus {
            self.commit_into_shell_entry(entry, text);
            return;
        }

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

    /// Hand the in-progress composition to the entry that will draw it.
    ///
    /// It goes into the entry's own [`crate::ui::text_edit::TextEdit`] rather than being tracked
    /// beside it, so every entry renders a preedit the same way and none can forget to.
    fn set_shell_preedit(&mut self, entry: ShellEntry, preedit: Option<String>) {
        let changed = match entry {
            ShellEntry::Shield => self.synoik.unlock_dialog.set_preedit(preedit),
            ShellEntry::RunDialog => self.synoik.run_dialog.set_preedit(preedit),
            ShellEntry::Polkit => self.synoik.polkit_dialog.set_preedit(preedit),
            ShellEntry::FolderRename => self.synoik.folder_dialog.set_preedit(preedit),
            ShellEntry::OverviewSearch => self.synoik.overview_search.set_preedit(preedit),
        };
        if changed {
            self.synoik.queue_redraw_all();
        }
    }

    /// Put finished text into one of the compositor's own entries.
    ///
    /// Delivered as one synthetic keystroke per character (see
    /// [`State::type_into_shell_entry`], which a paste shares) rather than through a new
    /// insert-a-string API on each entry.
    fn commit_into_shell_entry(&mut self, entry: ShellEntry, text: &str) {
        self.clear_preedit();

        self.type_into_shell_entry(entry, text);

        if let Some(im) = self.synoik.input_method.as_mut() {
            im.preedit = None;
        }
        self.synoik.queue_redraw_all();
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

        // A compositor entry has no `zwp_text_input_v3` to send this over: the string is kept
        // here and drawn by whichever entry is focused (see `preedit()`).
        let focus = self
            .synoik
            .input_method
            .as_ref()
            .map_or(ImFocus::None, |im| im.focus);
        if let ImFocus::Shell(entry) = focus {
            self.set_shell_preedit(entry, text.clone());
            if let Some(im) = self.synoik.input_method.as_mut() {
                im.preedit = text;
            }
            self.synoik.queue_redraw_all();
            return;
        }

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
    fn a_dead_key_counts_as_composing_though_it_types_nothing() {
        // The premise the whole feature rests on, and the one that is easy to assume away.
        assert_eq!(Keysym::dead_acute.key_char(), None);
        assert_eq!(Keysym::dead_tilde.key_char(), None);

        assert!(begins_composition(Some(Keysym::dead_acute)));
        assert!(begins_composition(Some(Keysym::dead_tilde)));
        assert!(begins_composition(Some(Keysym::dead_diaeresis)));
        assert!(begins_composition(Some(Keysym::dead_cedilla)));
        // Compose opens a sequence the same way.
        assert!(begins_composition(Some(Keysym::Multi_key)));

        // Ordinary keys carry their own text and need no special case.
        assert!(!begins_composition(Some(Keysym::a)));
        assert!(!begins_composition(Some(Keysym::apostrophe)));
        assert!(!begins_composition(Some(Keysym::BackSpace)));
        assert!(!begins_composition(Some(Keysym::Shift_L)));
        assert!(!begins_composition(None));
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
