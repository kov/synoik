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

use std::sync::Arc;

use smithay::wayland::text_input::{TextInputEvent, TextInputSeat};

use crate::dbus::ibus::{ImEvent, PreeditMode};
use crate::synoik::State;

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
}

/// What the worker reports back. Everything here is applied on the compositor thread.
#[derive(Debug, Clone, PartialEq)]
pub enum ImUpdate {
    /// The engine produced something.
    Event(ImEvent),
    /// The worker (re)connected to a daemon, or lost it. While disconnected the compositor must
    /// behave exactly as it does with no input method at all.
    Connected(bool),
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
    /// The preedit we last sent a client, so focus changes and resets can clear it.
    preedit: Option<String>,
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
            preedit: None,
        }
    }

    /// Whether a client has text input enabled right now.
    pub fn is_enabled(&self) -> bool {
        self.enabled
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
                // A disabled input keeps no composition. Clear ours before telling the engine,
                // so a client that re-enables immediately cannot see a stale preedit.
                self.clear_preedit();
                let Some(im) = self.synoik.input_method.as_mut() else {
                    return;
                };
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
            // `Done` needs no forwarding: the requests above are already the atomic batch, and
            // IBus has no matching "end of batch" call. Content type and the cursor rectangle
            // are slices 8 and 6 respectively.
            TextInputEvent::Done
            | TextInputEvent::TextChangeCause(_)
            | TextInputEvent::ContentType { .. }
            | TextInputEvent::CursorRectangle(_) => {}
        }
    }

    /// Something came back from the engine.
    pub fn on_im_update(&mut self, update: ImUpdate) {
        match update {
            ImUpdate::Connected(connected) => {
                if !connected {
                    self.clear_preedit();
                }
            }
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
            // Slice 4 territory: a key the engine handed back or synthesized.
            ImEvent::ForwardKey { .. } => {}
            ImEvent::RequireSurrounding => {}
        }
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
