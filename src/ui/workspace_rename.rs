// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Renaming a workspace from its thumbnail, the third row `docs/fork/multi-display.md` §6 asks
//! for.
//!
//! **Divergence.** gnome-shell has no equivalent: its workspaces have no names. The editing
//! model is the app-folder rename's ([`crate::ui::folder_dialog`]) — the shared [`TextEdit`],
//! opened with the old name selected so typing replaces it, Enter commits, Escape abandons —
//! because that is the shell's one in-place rename and the two should not diverge.
//!
//! The entry takes the place of the name pill on the thumbnail, so what is being edited is
//! where the result will appear.

use smithay::input::keyboard::Keysym;
use smithay::utils::{Logical, Rectangle, Size};

use crate::layout::workspace::WorkspaceId;
use crate::ui::text_edit::{EditMods, EditOutcome, KeyTheme, TextEdit};
use crate::ui::widget::{Entry, EntryStyle};

/// What the rename entry did with a key — the folder rename's outcome, for the same reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameKey {
    /// Not ours: the caller keeps looking.
    Ignored,
    /// Consumed, nothing else to do.
    Took,
    /// Enter: the caller should apply the name.
    Commit,
    /// Escape: the caller should drop the rename, leaving the name as it was.
    Cancel,
}

/// A rename in progress.
#[derive(Debug)]
pub struct WorkspaceRename {
    /// Which workspace is being renamed. The entry outlives strip re-layouts, so the workspace
    /// is held by id rather than by position.
    pub workspace: WorkspaceId,
    edit: TextEdit,
}

impl WorkspaceRename {
    /// Start renaming `workspace`, whose current name is `name` (empty when it has none).
    ///
    /// The old name comes up selected, so typing replaces it and Backspace clears it — the
    /// folder rename's `select_all`.
    pub fn new(workspace: WorkspaceId, name: &str) -> Self {
        let mut edit = TextEdit::with_text(name.to_owned());
        edit.select_all();
        Self { workspace, edit }
    }

    /// The editing model, for the clipboard bindings and the input method.
    pub fn edit(&self) -> &TextEdit {
        &self.edit
    }

    pub fn edit_mut(&mut self) -> &mut TextEdit {
        &mut self.edit
    }

    /// What is currently typed, trimmed. Empty means "no name" — a workspace may have none, so
    /// clearing the entry is how a name is taken away.
    pub fn name(&self) -> &str {
        self.edit.text().trim()
    }

    /// Feed a key to the entry. Enter commits, Escape cancels, everything else is the shared
    /// editing surface's.
    pub fn key(
        &mut self,
        keysym: Option<Keysym>,
        ch: Option<char>,
        mods: EditMods,
        theme: KeyTheme,
    ) -> RenameKey {
        // Return commits whatever the modifiers are, like the folder rename: `TextEdit`'s
        // Activate is plain-only, and a Ctrl+Enter that quietly did nothing is not a behavior
        // anyone asked for.
        if matches!(
            keysym,
            Some(Keysym::Return | Keysym::KP_Enter | Keysym::ISO_Enter)
        ) {
            return RenameKey::Commit;
        }
        // Escape belongs to the rename while it is up — it abandons the edit rather than
        // closing the overview underneath it, which is the innermost-thing-first rule every
        // modal surface in the shell follows.
        if keysym == Some(Keysym::Escape) {
            return RenameKey::Cancel;
        }
        match self.edit.handle_key(keysym, ch, mods, theme) {
            EditOutcome::Changed | EditOutcome::Moved => RenameKey::Took,
            EditOutcome::Activate | EditOutcome::Cancel | EditOutcome::Ignored => {
                RenameKey::Ignored
            }
        }
    }
}

/// The entry pill's box on the thumbnail drawn at `thumb`: where the name label would be, as
/// wide as the thumbnail allows.
pub fn entry_rect(thumb: Rectangle<f64, Logical>) -> Rectangle<f64, Logical> {
    let inset = crate::layout::thumbnails::NAME_INSET;
    let width = (thumb.size.w - 2. * inset).max(0.);
    // Never taller than the thumbnail it is drawn on: a small strip on a short display would
    // otherwise hang the field off both edges.
    let height = Entry::HEIGHT.min(thumb.size.h - 2. * inset);
    let layout = Entry::layout(
        thumb.loc.x + thumb.size.w / 2.,
        thumb.loc.y + thumb.size.h - inset - height,
        width,
        height,
        STYLE,
    );
    layout.pill
}

/// Over a wallpaper, left-aligned, no icons — the family `%lockscreen_entry` exists for, and
/// the thumbnail is a wallpaper.
pub const STYLE: EntryStyle = EntryStyle::Lockscreen;

/// The size an [`entry_rect`] asks the bake for.
pub fn entry_size(thumb: Rectangle<f64, Logical>) -> Size<f64, Logical> {
    entry_rect(thumb).size
}
