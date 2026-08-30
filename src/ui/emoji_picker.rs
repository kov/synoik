// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The emoji picker's grab: what it is while it has no UI yet.
//!
//! Unlike every other shell surface that takes keys, the picker **must not appear in
//! `KeyboardFocus`**. `text-input-v3` enter/leave rides `wl_keyboard` focus and every shell-owned
//! focus variant has no surface, so becoming the focus would take the client's text input away —
//! the very thing the picker exists to commit into. The client keeps `wl_keyboard` focus
//! throughout and the picker reads keys out of the input filter instead.
//!
//! See `docs/fork/emoji-picker.md`. The grid, the search entry and the tone popover are the next
//! slice; this module owns only the open/close state and the anchor it was opened at.

use smithay::input::keyboard::Keysym;
use smithay::utils::{Logical, Rectangle};

/// What the compositor should do with a key the picker was offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKey {
    /// The picker used it, and it goes no further.
    Handled,
    /// The picker is closing; the key still goes no further.
    Close,
}

#[derive(Debug, Default)]
pub struct EmojiPicker {
    open: Option<Rectangle<f64, Logical>>,
}

impl EmojiPicker {
    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// Where the picker was opened, in global coordinates — the caret, or the pointer.
    ///
    /// Captured once at open rather than read per frame: the anchor is the caret *as it was when
    /// the user asked*, and a client that keeps editing underneath must not drag the picker
    /// around.
    pub fn anchor(&self) -> Option<Rectangle<f64, Logical>> {
        self.open
    }

    pub fn open(&mut self, anchor: Rectangle<f64, Logical>) {
        self.open = Some(anchor);
    }

    /// Closes, and says whether it was open — so a caller can skip the redraw when it was not.
    pub fn close(&mut self) -> bool {
        self.open.take().is_some()
    }

    /// Offer a key press to the picker.
    pub fn handle_key(&mut self, raw: Option<Keysym>) -> PickerKey {
        if raw == Some(Keysym::Escape) {
            self.open = None;
            return PickerKey::Close;
        }
        PickerKey::Handled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_closes_and_everything_else_is_swallowed() {
        let mut picker = EmojiPicker::default();
        assert!(!picker.is_open());
        assert_eq!(picker.anchor(), None);

        let anchor = Rectangle::new((10., 20.).into(), (2., 18.).into());
        picker.open(anchor);
        assert_eq!(picker.anchor(), Some(anchor));

        assert_eq!(picker.handle_key(Some(Keysym::a)), PickerKey::Handled);
        assert!(picker.is_open(), "a plain key does not close the picker");

        assert_eq!(picker.handle_key(Some(Keysym::Escape)), PickerKey::Close);
        assert!(!picker.is_open());
        assert!(!picker.close(), "closing a closed picker is a no-op");
    }
}
