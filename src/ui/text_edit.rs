// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! A reusable single-line editable text model — the state machine behind every entry
//! in the shell (overview search, run dialog, polkit password, lock screen, folder
//! rename).
//!
//! **Why this exists.** Until now every entry hand-rolled `String::push` / `String::pop`,
//! so all of them shared the same limitation: the caret was pinned to the end, there was
//! no selection, and a modified key (Ctrl/Alt/Super) was refused outright rather than
//! caret-handled. [`TextEdit`] owns the string, the caret and the selection bound, and
//! [`TextEdit::handle_key`] implements the bindings GNOME's entries actually have. The
//! view side stays in [`crate::ui::widget::Entry`]; this module never touches a renderer.
//!
//! **Where the bindings come from.** GNOME splits them across two files:
//!
//! * `clutter/clutter/pango/clutter-text.c:4338-4430` registers the motion/deletion pool —
//!   Left/Right (+Ctrl for word motion, `:3150-3228`), Up/Down, Home/End (`:3340-3382`), `Ctrl-a` =
//!   **select all** (`:3384-3394`, *not* beginning-of-line), Delete and `Ctrl-Delete`
//!   (`:3396-3452`), BackSpace and `Ctrl-BackSpace` (`:3454-3534`), and Return/KP_Enter/ISO_Enter =
//!   activate.
//! * `st/st-entry.c:656-765` adds what St layers on top for events ClutterText left unhandled:
//!   `Ctrl-u` deletes to the start of the line (`:743-750`) and `Ctrl-k` to the end (`:754-762`) —
//!   the two Emacs bindings GNOME ships unconditionally — plus the `Ctrl-c`/`Ctrl-v`/`Ctrl-x`
//!   clipboard set, which is **not** ported here (it needs the Wayland selection, so it belongs to
//!   the caller, not to a plain-data model).
//!
//! Shift-selection is `clutter_text_add_move_binding` (`clutter-text.c:3545-3577`): every
//! motion is registered plain, with Shift, with Ctrl, and with both, and each handler only
//! collapses the selection when Shift is *absent*. That is exactly [`Motion::apply`] here.
//!
//! **Divergence — the Emacs key theme.** GNOME does *not* implement
//! `org.gnome.desktop.interface gtk-key-theme` for shell entries: it is a pure GTK
//! mechanism (GTK loads `Emacs.css` with `gtk-key-bindings`), and greps for `key-theme`
//! over both `gnome-shell/src`+`js` and all of mutter return nothing. We honor it anyway,
//! because a user who set that key means it for every text field on their desktop and the
//! shell's silence reads as a bug rather than a decision. It is opt-in and off by default,
//! so the shipped behavior is still GNOME's; [`KeyTheme::Emacs`] only *adds* bindings, with
//! one deliberate override (`Ctrl-a` becomes beginning-of-line, as Emacs users expect, and
//! select-all is then only on `Ctrl-slash`-free `Ctrl-a`'s replacement — see
//! [`KeyTheme::Emacs`]).
//!
//! **Word boundaries.** ClutterText scans Pango log attrs for `is_word_start` /
//! `is_word_end` (`clutter-text.c:2239-2286`), i.e. UAX #29 word breaks. We use
//! `unicode-segmentation`, which implements the same annex, rather than an
//! `is_alphanumeric` approximation that would disagree on punctuation runs and CJK.

use std::ops::Range;

use smithay::input::keyboard::Keysym;
use unicode_segmentation::UnicodeSegmentation;

/// The modifier state a key arrived with. A plain-data mirror of the input backend's
/// modifiers, so this module (and its tests) need no seat.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EditMods {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub logo: bool,
}

impl EditMods {
    /// No modifier that changes a binding's meaning. (Shift is excluded: it selects
    /// rather than rebinding, and it is how you type a capital letter.)
    pub fn is_plain(self) -> bool {
        !self.ctrl && !self.alt && !self.logo
    }

    pub fn ctrl() -> Self {
        Self {
            ctrl: true,
            ..Self::default()
        }
    }

    pub fn shift() -> Self {
        Self {
            shift: true,
            ..Self::default()
        }
    }

    pub fn ctrl_shift() -> Self {
        Self {
            ctrl: true,
            shift: true,
            ..Self::default()
        }
    }
}

/// Which set of editing bindings is live — `org.gnome.desktop.interface gtk-key-theme`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum KeyTheme {
    /// What GNOME Shell entries do today: the ClutterText pool plus St's `Ctrl-u`/`Ctrl-k`.
    #[default]
    Default,
    /// `gtk-key-theme = "Emacs"`. Adds `Ctrl-b`/`Ctrl-f` (char motion), `Ctrl-e`
    /// (end of line), `Ctrl-d` (delete forward), `Ctrl-w` (kill previous word),
    /// `Ctrl-y` (yank the kill buffer) and `Ctrl-n`/`Ctrl-p` (inert on one line, but
    /// swallowed rather than typed). It also **overrides** `Ctrl-a` to
    /// beginning-of-line — the one place the two themes disagree instead of stacking,
    /// and the reason this is a theme rather than a flag. `Ctrl-u`/`Ctrl-k` already
    /// mean the Emacs thing in the default theme, so they do not move.
    Emacs,
}

impl KeyTheme {
    /// Parse the gsettings value. Anything but `"Emacs"` is the default theme, which is
    /// also what GTK does with an unknown theme name.
    pub fn from_setting(value: &str) -> Self {
        if value.eq_ignore_ascii_case("emacs") {
            KeyTheme::Emacs
        } else {
            KeyTheme::Default
        }
    }
}

/// What [`TextEdit::handle_key`] did with a key, for the caller to act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditOutcome {
    /// The text changed — re-run whatever the text drives (a search, a completion).
    Changed,
    /// Only the caret or selection moved. Redraw, but the text is the same.
    Moved,
    /// Return/KP_Enter/ISO_Enter: the caller should submit.
    Activate,
    /// Escape: the caller should cancel (clear, or close — its policy, not ours).
    Cancel,
    /// Not a key this entry handles; the caller may fall through to its own bindings.
    Ignored,
}

/// Whether a motion collapses the selection or extends it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Motion {
    /// No Shift — the caret moves and the selection is dropped.
    Move,
    /// Shift held — the caret moves and the selection bound stays put, so the
    /// selection grows or shrinks (`clutter-text.c:3185-3186` and friends).
    Extend,
}

impl Motion {
    fn of(mods: EditMods) -> Self {
        if mods.shift {
            Motion::Extend
        } else {
            Motion::Move
        }
    }
}

/// A single-line editable string with a caret and a selection.
///
/// Byte offsets throughout: `cursor` and `anchor` are always on `char` boundaries of
/// `text`, which every mutator here maintains (and `debug_assert`s). `anchor == cursor`
/// means no selection.
#[derive(Debug, Clone, Default)]
pub struct TextEdit {
    text: String,
    /// Caret position, a byte offset into `text`.
    cursor: usize,
    /// The other end of the selection. Equal to `cursor` when nothing is selected.
    anchor: usize,
    /// The Emacs kill buffer that `Ctrl-k`/`Ctrl-w` fill and `Ctrl-y` yanks.
    ///
    /// Not a clipboard: a real kill ring would need the Wayland selection. This is per-entry
    /// and dies with it.
    ///
    /// **Only ever written under [`KeyTheme::Emacs`].** In the default theme GNOME's
    /// `Ctrl-u`/`Ctrl-k` simply *discard* what they delete (`st_entry_key_press_event` calls
    /// `clutter_text_delete_text` and nothing else, `st-entry.c:743-762`) and there is no
    /// `Ctrl-y` to paste it back — so filling this would buy nothing and cost everything: it
    /// is a second heap copy of whatever was deleted, and both password entries route
    /// `Ctrl-u` here. Typing a password and hitting `Ctrl-u` to start over would have left
    /// the whole secret sitting in this buffer for the life of the dialog.
    ///
    /// It is zeroed by [`Self::secure_clear`] and before every overwrite, for the same
    /// reason: a `String` that is merely reassigned drops its old allocation unzeroed.
    kill: String,
    /// The engine's in-progress composition. Shown at the caret, never part of `text` — see
    /// [`Self::preedit`].
    preedit: Option<String>,
}

impl TextEdit {
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty edit with room for `capacity` bytes already reserved.
    ///
    /// For a **password** field: growing a `String` copies its bytes into a new allocation and
    /// leaves the old one on the heap, unzeroed, so a password typed one character at a time
    /// leaves a ladder of increasingly complete fragments behind it. Reserving up front means
    /// no realloc happens for any realistic secret. Pair with [`Self::secure_clear`].
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            text: String::with_capacity(capacity),
            ..Self::default()
        }
    }

    /// An edit already holding `text`, caret at the end (what recalling a history entry
    /// or seeding a rename wants).
    pub fn with_text(text: impl Into<String>) -> Self {
        let mut this = Self::new();
        this.set_text(text);
        this
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Caret byte offset.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The selected byte range, ordered, or `None` when nothing is selected.
    pub fn selection(&self) -> Option<Range<usize>> {
        let (lo, hi) = self.ordered();
        (lo != hi).then_some(lo..hi)
    }

    /// Replace the whole text, caret to the end, selection dropped.
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.text.len();
        self.anchor = self.cursor;
    }

    /// Empty it, caret and selection with it.
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.anchor = 0;
    }

    /// [`Self::clear`], but the bytes are overwritten first and the allocation is kept at
    /// `capacity` — the password path.
    ///
    /// `write_volatile` so the fill is not optimised away as a dead store into memory nothing
    /// reads again. Note this zeroes the *live* bytes only: anything beyond `len` inside the
    /// same allocation (the tail a mid-string deletion shifted down) survives until the next
    /// call, which is the same guarantee the hand-rolled `String` buffers gave before.
    pub fn secure_clear(&mut self, capacity: usize) {
        // SAFETY: every byte is overwritten with 0 and the vec is then truncated to nothing,
        // so no invalid UTF-8 is ever observable.
        zero(&mut self.text);
        if self.text.capacity() < capacity {
            self.text.reserve(capacity - self.text.capacity());
        }
        self.cursor = 0;
        self.anchor = 0;
        zero(&mut self.kill);
        // A composition abandoned mid-password is as much of the secret as the rest.
        if let Some(preedit) = self.preedit.as_mut() {
            zero(preedit);
        }
        self.preedit = None;
    }

    /// This edit's caret and selection **mapped onto a masked rendering** of it — the offsets
    /// that index `mask.to_string().repeat(char_count)` rather than the text itself.
    ///
    /// For a password field whose view is deliberately never handed the secret (the lock
    /// screen's `PromptContent` carries the bullets, not the password). Masking is per
    /// character, so an offset maps by counting characters and multiplying by the mask's own
    /// encoded length — reusing the byte offset would land mid-glyph for any non-ASCII secret.
    pub fn masked_positions(
        &self,
        mask: char,
        focused: bool,
    ) -> (Option<usize>, Option<Range<usize>>) {
        if !focused {
            return (None, None);
        }
        let at = |i: usize| self.masked_offset(i, mask);
        // A shown composition sits between the caret's text position and where it is drawn, so
        // the caret goes after it — otherwise it lands *inside* the characters being composed.
        let preedit = self.preedit.as_deref().map_or(0, str::len);
        (
            Some(at(self.cursor) + preedit),
            self.selection().map(|s| at(s.start)..at(s.end)),
        )
    }

    /// The caret's byte offset in the *unmasked* rendering, composition included.
    ///
    /// The plain-text counterpart of [`Self::masked_positions`]' first element, for the entries
    /// that draw their text as-is.
    pub fn display_cursor(&self) -> usize {
        self.cursor + self.preedit.as_deref().map_or(0, str::len)
    }

    /// The in-progress composition, shown at the caret but **not** part of [`Self::text`].
    ///
    /// It is deliberately not inserted: it is not yet input, and committing it is the engine's
    /// call. An entry that read it as text would submit a half-composed password, search for a
    /// dangling accent, or rename a folder to one.
    pub fn preedit(&self) -> Option<&str> {
        self.preedit.as_deref()
    }

    /// Set (or clear) the composition. Returns whether anything changed, so callers can skip a
    /// redraw.
    pub fn set_preedit(&mut self, preedit: Option<String>) -> bool {
        if self.preedit == preedit {
            return false;
        }
        // A composition that came and went must not be left on the heap of a password entry;
        // `String` assignment would drop the old allocation unzeroed. Same reasoning as `kill`.
        if let Some(old) = self.preedit.as_mut() {
            zero(old);
        }
        self.preedit = preedit;
        true
    }

    /// What to draw: the text — masked, if `mask` is given — with any composition spliced in at
    /// the caret.
    ///
    /// **A preedit is shown in the clear even in a password field.** That is what GNOME does:
    /// `ClutterText` builds its layout from `clutter_text_get_display_text`, which applies
    /// `password_char`, and then inserts the raw preedit into the result
    /// (`clutter-text.c:848-877`). Visible is the point — nobody can steer a composition they
    /// cannot see, and it lasts only until the engine commits, at which point the committed
    /// characters are masked like everything else.
    pub fn display(&self, mask: Option<char>) -> String {
        let (base, caret) = match mask {
            Some(m) => (
                m.to_string().repeat(self.text.chars().count()),
                self.masked_offset(self.cursor, m),
            ),
            None => (self.text.clone(), self.cursor),
        };

        let Some(preedit) = self.preedit.as_deref() else {
            return base;
        };
        let caret = caret.min(base.len());
        let mut out = String::with_capacity(base.len() + preedit.len());
        out.push_str(&base[..caret]);
        out.push_str(preedit);
        out.push_str(&base[caret..]);
        out
    }

    /// A byte offset into `text`, mapped onto the masked rendering of it.
    fn masked_offset(&self, i: usize, mask: char) -> usize {
        self.text[..i.min(self.text.len())].chars().count() * mask.len_utf8()
    }

    /// Select everything, caret at the end (`clutter_text_real_select_all`,
    /// `clutter-text.c:3384-3394`).
    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.cursor = self.text.len();
    }

    /// Insert `s` at the caret, replacing the selection first — what typing a character
    /// and what a paste both do.
    pub fn insert_str(&mut self, s: &str) {
        self.delete_selection();
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
        self.anchor = self.cursor;
        self.debug_check();
    }

    /// Handle one key press.
    ///
    /// `sym` is the raw keysym (unmodified, so `Ctrl-a` arrives as `a` however the seat
    /// mapped it), `ch` the character the key would type or `None` for a non-typing key,
    /// and `mods` the modifier state. Returns [`EditOutcome::Ignored`] for anything this
    /// entry does not claim, so the caller can fall through to its own bindings.
    pub fn handle_key(
        &mut self,
        sym: Option<Keysym>,
        ch: Option<char>,
        mods: EditMods,
        theme: KeyTheme,
    ) -> EditOutcome {
        if mods.logo {
            // A Super binding is the compositor's, never an entry's.
            return EditOutcome::Ignored;
        }
        if let Some(sym) = sym {
            if let Some(outcome) = self.handle_sym(sym, mods, theme) {
                return outcome;
            }
        }
        // Typing. `ch` is only ever `Some` for an unmodified printable (the caller masks
        // it under Ctrl/Alt), which matches ClutterText refusing to insert while Control
        // is down (`clutter-text.c:2456`).
        match ch {
            Some(c) if mods.is_plain() && !c.is_control() => {
                let mut buf = [0u8; 4];
                self.insert_str(c.encode_utf8(&mut buf));
                EditOutcome::Changed
            }
            _ => EditOutcome::Ignored,
        }
    }

    /// The keysym half of [`Self::handle_key`]. `None` = not ours, try typing.
    fn handle_sym(&mut self, sym: Keysym, mods: EditMods, theme: KeyTheme) -> Option<EditOutcome> {
        let emacs = theme == KeyTheme::Emacs;
        let motion = Motion::of(mods);

        // --- Activation / cancellation. Unmodified only: Ctrl-Enter is the caller's
        // (GNOME's open-in-new-window), and this model must not swallow it.
        match sym {
            Keysym::Return | Keysym::KP_Enter | Keysym::ISO_Enter if mods.is_plain() => {
                return Some(EditOutcome::Activate);
            }
            Keysym::Escape if mods.is_plain() => return Some(EditOutcome::Cancel),
            _ => {}
        }

        // --- Motion (`clutter-text.c:4340-4379`). Ctrl upgrades char motion to word
        // motion; Shift extends rather than collapsing.
        let moved = match sym {
            Keysym::Left | Keysym::KP_Left if !mods.alt => {
                let to = if mods.ctrl {
                    self.word_start_before(self.cursor)
                } else {
                    self.prev_grapheme(self.cursor)
                };
                Some(to)
            }
            Keysym::Right | Keysym::KP_Right if !mods.alt => {
                let to = if mods.ctrl {
                    self.word_end_after(self.cursor)
                } else {
                    self.next_grapheme(self.cursor)
                };
                Some(to)
            }
            // Home/End land on the two ends (`clutter_text_move_line_start/end`,
            // `clutter-text.c:2288,2323`). **Up/Down are deliberately absent**: on line 0 of a
            // one-line entry `clutter_text_real_move_up` returns FALSE (`:3258-3259`), so the
            // binding is unhandled in GNOME too and the key falls through to whatever is
            // above. The overview search relies on exactly that to steer its results.
            Keysym::Home | Keysym::KP_Home | Keysym::Begin if !mods.alt => Some(0),
            Keysym::End | Keysym::KP_End if !mods.alt => Some(self.text.len()),
            // Emacs motion, only when the theme is on.
            Keysym::b | Keysym::B if emacs && mods.ctrl => Some(self.prev_grapheme(self.cursor)),
            Keysym::f | Keysym::F if emacs && mods.ctrl => Some(self.next_grapheme(self.cursor)),
            Keysym::a | Keysym::A if emacs && mods.ctrl => Some(0),
            Keysym::e | Keysym::E if emacs && mods.ctrl => Some(self.text.len()),
            _ => None,
        };
        if let Some(to) = moved {
            self.move_caret(to, motion);
            return Some(EditOutcome::Moved);
        }

        // --- Select all (`clutter-text.c:4381-4388`). In the Emacs theme `Ctrl-a` is
        // beginning-of-line above, so this arm never sees it there.
        if matches!(sym, Keysym::a | Keysym::A) && mods.ctrl && !mods.shift && !emacs {
            self.select_all();
            return Some(EditOutcome::Moved);
        }

        // --- Deletion.
        let deleted = match sym {
            // BackSpace and Shift-BackSpace are the same binding (`:4406-4413`).
            //
            // Divergence: with a selection up, the Ctrl arms below delete the *selection*
            // rather than a word. GNOME's `clutter_text_real_del_word_prev/next`
            // (`:3419-3452`, `:3490-3534`) do not — only the unmodified variants clear the
            // selection first. Deleting visibly-selected text is what every other toolkit
            // (and GTK) does, and "Ctrl-BackSpace ate a word *next to* what I had selected"
            // is a surprise with no upside.
            Keysym::BackSpace if !mods.alt => {
                if self.delete_selection() {
                    true
                } else if mods.ctrl {
                    // `Ctrl-BackSpace` = delete previous word (`:4414-4417`).
                    self.delete_range(self.word_start_before(self.cursor)..self.cursor)
                } else {
                    self.delete_range(self.prev_grapheme(self.cursor)..self.cursor)
                }
            }
            Keysym::Delete | Keysym::KP_Delete if !mods.alt => {
                if self.delete_selection() {
                    true
                } else if mods.ctrl {
                    // `Ctrl-Delete` = delete next word (`:4394-4405`).
                    self.delete_range(self.cursor..self.word_end_after(self.cursor))
                } else {
                    self.delete_range(self.cursor..self.next_grapheme(self.cursor))
                }
            }
            // St's two unconditional Emacs bindings (`st-entry.c:743-762`). They kill to
            // the line end/start regardless of the selection, which is what St does — it
            // calls `clutter_text_delete_text` with explicit positions.
            Keysym::k | Keysym::K if mods.ctrl => {
                self.kill_range(self.cursor..self.text.len(), emacs)
            }
            Keysym::u | Keysym::U if mods.ctrl => self.kill_range(0..self.cursor, emacs),
            // Emacs-only deletions.
            Keysym::d | Keysym::D if emacs && mods.ctrl => {
                if self.delete_selection() {
                    true
                } else {
                    self.delete_range(self.cursor..self.next_grapheme(self.cursor))
                }
            }
            Keysym::w | Keysym::W if emacs && mods.ctrl => {
                let range = match self.selection() {
                    Some(sel) => sel,
                    None => self.word_start_before(self.cursor)..self.cursor,
                };
                self.kill_range(range, true)
            }
            _ => return self.handle_emacs_tail(sym, mods, emacs),
        };
        Some(if deleted {
            EditOutcome::Changed
        } else {
            // A BackSpace on an empty entry is still *ours* — the caller must not treat
            // it as a fall-through and fire an unrelated binding.
            EditOutcome::Moved
        })
    }

    /// The Emacs bindings that are neither motion nor deletion.
    fn handle_emacs_tail(
        &mut self,
        sym: Keysym,
        mods: EditMods,
        emacs: bool,
    ) -> Option<EditOutcome> {
        if !emacs || !mods.ctrl {
            return None;
        }
        match sym {
            Keysym::y | Keysym::Y => {
                if self.kill.is_empty() {
                    return Some(EditOutcome::Moved);
                }
                let kill = std::mem::take(&mut self.kill);
                self.insert_str(&kill);
                self.kill = kill;
                Some(EditOutcome::Changed)
            }
            // Line motion on a single-line entry: nothing to do, but Emacs users expect
            // these to be swallowed rather than fall through to a compositor bind.
            Keysym::n | Keysym::N | Keysym::p | Keysym::P => Some(EditOutcome::Moved),
            _ => None,
        }
    }

    /// Move the caret to `to`, collapsing or extending the selection per `motion`.
    fn move_caret(&mut self, to: usize, motion: Motion) {
        self.cursor = to;
        if motion == Motion::Move {
            self.anchor = to;
        }
        self.debug_check();
    }

    /// Delete the selection if there is one. Returns whether anything went.
    fn delete_selection(&mut self) -> bool {
        match self.selection() {
            Some(sel) => self.delete_range(sel),
            None => false,
        }
    }

    /// Delete `range`, moving the caret to its start. Returns whether anything went.
    fn delete_range(&mut self, range: Range<usize>) -> bool {
        if range.start >= range.end {
            return false;
        }
        self.text.replace_range(range.clone(), "");
        self.cursor = range.start;
        self.anchor = range.start;
        self.debug_check();
        true
    }

    /// [`Self::delete_range`], but when `remember` the removed text lands in the kill buffer
    /// for `Ctrl-y`. See [`Self::kill`] for why `remember` is false in the default theme.
    fn kill_range(&mut self, range: Range<usize>, remember: bool) -> bool {
        if range.start >= range.end {
            return false;
        }
        if remember {
            // Overwrite the previous kill in place where it fits, so a reassignment cannot
            // drop an allocation still holding the last thing that was deleted.
            let text = self.text[range.clone()].to_owned();
            zero(&mut self.kill);
            self.kill.push_str(&text);
        }
        self.delete_range(range)
    }

    /// The selection bounds, low first.
    fn ordered(&self) -> (usize, usize) {
        if self.cursor <= self.anchor {
            (self.cursor, self.anchor)
        } else {
            (self.anchor, self.cursor)
        }
    }

    /// The grapheme boundary before `at` (so a flag emoji or a combining accent goes as
    /// one BackSpace, which is what a user means by "a character").
    fn prev_grapheme(&self, at: usize) -> usize {
        self.text[..at]
            .grapheme_indices(true)
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0)
    }

    /// The grapheme boundary after `at`.
    fn next_grapheme(&self, at: usize) -> usize {
        self.text[at..]
            .grapheme_indices(true)
            .next()
            .map(|(_, g)| at + g.len())
            .unwrap_or(self.text.len())
    }

    /// The start of the word at or before `at` — Pango's `is_word_start` scan
    /// (`clutter_text_move_word_backward`, `clutter-text.c:2239-2260`).
    fn word_start_before(&self, at: usize) -> usize {
        self.text
            .split_word_bound_indices()
            .rfind(|(i, w)| *i < at && is_word(w))
            .map(|(i, _)| i)
            .unwrap_or(0)
    }

    /// The end of the word at or after `at` — Pango's `is_word_end` scan
    /// (`clutter_text_move_word_forward`, `clutter-text.c:2262-2286`).
    fn word_end_after(&self, at: usize) -> usize {
        self.text
            .split_word_bound_indices()
            .map(|(i, w)| i + w.len())
            .zip(self.text.split_word_bound_indices().map(|(_, w)| w))
            .find(|(end, w)| *end > at && is_word(w))
            .map(|(end, _)| end)
            .unwrap_or(self.text.len())
    }

    fn debug_check(&self) {
        debug_assert!(self.text.is_char_boundary(self.cursor));
        debug_assert!(self.text.is_char_boundary(self.anchor));
    }
}

/// Overwrite a string's bytes and empty it, keeping the allocation — the password path's
/// `write_volatile` fill, so the compiler cannot drop it as a store nothing reads back.
fn zero(s: &mut String) {
    // SAFETY: every byte is overwritten with 0 and the vec is then truncated to nothing, so no
    // invalid UTF-8 is ever observable.
    unsafe {
        let bytes = s.as_mut_vec();
        bytes
            .iter_mut()
            .for_each(|b| std::ptr::write_volatile(b, 0));
        bytes.clear();
    }
}

/// Whether a UAX #29 word-bound chunk is a *word* rather than whitespace or punctuation
/// — Pango sets `is_word_start`/`is_word_end` only on these.
fn is_word(chunk: &str) -> bool {
    chunk.chars().any(char::is_alphanumeric)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Type `s` one character at a time, the way the input path feeds it.
    fn type_str(e: &mut TextEdit, s: &str) {
        for c in s.chars() {
            assert_eq!(
                e.handle_key(None, Some(c), EditMods::default(), KeyTheme::Default),
                EditOutcome::Changed
            );
        }
    }

    fn press(e: &mut TextEdit, sym: Keysym, mods: EditMods) -> EditOutcome {
        e.handle_key(Some(sym), None, mods, KeyTheme::Default)
    }

    fn press_emacs(e: &mut TextEdit, sym: Keysym, mods: EditMods) -> EditOutcome {
        e.handle_key(Some(sym), None, mods, KeyTheme::Emacs)
    }

    #[test]
    fn typing_inserts_at_the_caret_not_the_end() {
        let mut e = TextEdit::new();
        type_str(&mut e, "helo");
        press(&mut e, Keysym::Left, EditMods::default());
        type_str(&mut e, "l");
        assert_eq!(e.text(), "hello");
        assert_eq!(e.cursor(), 4);
    }

    #[test]
    fn arrows_move_and_clamp_at_both_ends() {
        let mut e = TextEdit::with_text("ab");
        assert_eq!(e.cursor(), 2);
        press(&mut e, Keysym::Right, EditMods::default());
        assert_eq!(e.cursor(), 2, "right at the end stays put");
        for _ in 0..5 {
            press(&mut e, Keysym::Left, EditMods::default());
        }
        assert_eq!(e.cursor(), 0, "left clamps at the start");
    }

    #[test]
    fn home_and_end_jump_to_the_ends() {
        let mut e = TextEdit::with_text("hello");
        assert_eq!(
            press(&mut e, Keysym::Home, EditMods::default()),
            EditOutcome::Moved
        );
        assert_eq!(e.cursor(), 0);
        press(&mut e, Keysym::End, EditMods::default());
        assert_eq!(e.cursor(), 5);
    }

    #[test]
    fn ctrl_arrows_move_by_word() {
        let mut e = TextEdit::with_text("one two three");
        press(&mut e, Keysym::Left, EditMods::ctrl());
        assert_eq!(&e.text()[e.cursor()..], "three");
        press(&mut e, Keysym::Left, EditMods::ctrl());
        assert_eq!(&e.text()[e.cursor()..], "two three");
        press(&mut e, Keysym::Right, EditMods::ctrl());
        assert_eq!(&e.text()[..e.cursor()], "one two");
    }

    #[test]
    fn ctrl_backspace_deletes_the_previous_word() {
        let mut e = TextEdit::with_text("one two three");
        assert_eq!(
            press(&mut e, Keysym::BackSpace, EditMods::ctrl()),
            EditOutcome::Changed
        );
        assert_eq!(e.text(), "one two ");
        press(&mut e, Keysym::BackSpace, EditMods::ctrl());
        assert_eq!(e.text(), "one ");
    }

    #[test]
    fn ctrl_delete_deletes_the_next_word() {
        let mut e = TextEdit::with_text("one two three");
        press(&mut e, Keysym::Home, EditMods::default());
        press(&mut e, Keysym::Delete, EditMods::ctrl());
        assert_eq!(e.text(), " two three");
    }

    #[test]
    fn shift_arrow_selects_and_typing_replaces_it() {
        let mut e = TextEdit::with_text("hello");
        press(&mut e, Keysym::Left, EditMods::shift());
        press(&mut e, Keysym::Left, EditMods::shift());
        assert_eq!(e.selection(), Some(3..5));
        type_str(&mut e, "p");
        assert_eq!(e.text(), "help");
        assert_eq!(e.selection(), None);
    }

    #[test]
    fn ctrl_shift_arrow_selects_by_word() {
        let mut e = TextEdit::with_text("one two");
        press(&mut e, Keysym::Left, EditMods::ctrl_shift());
        assert_eq!(e.selection(), Some(4..7));
    }

    #[test]
    fn a_plain_arrow_collapses_the_selection() {
        let mut e = TextEdit::with_text("hello");
        e.select_all();
        assert_eq!(e.selection(), Some(0..5));
        press(&mut e, Keysym::Left, EditMods::default());
        assert_eq!(e.selection(), None);
    }

    #[test]
    fn ctrl_a_selects_all_and_backspace_wipes_it() {
        let mut e = TextEdit::with_text("hello");
        assert_eq!(
            press(&mut e, Keysym::a, EditMods::ctrl()),
            EditOutcome::Moved
        );
        assert_eq!(e.selection(), Some(0..5));
        press(&mut e, Keysym::BackSpace, EditMods::default());
        assert_eq!(e.text(), "");
    }

    #[test]
    fn ctrl_k_and_ctrl_u_kill_to_the_ends() {
        let mut e = TextEdit::with_text("one two");
        press(&mut e, Keysym::Home, EditMods::default());
        press(&mut e, Keysym::Right, EditMods::ctrl());
        assert_eq!(
            press(&mut e, Keysym::k, EditMods::ctrl()),
            EditOutcome::Changed
        );
        assert_eq!(e.text(), "one");
        press(&mut e, Keysym::u, EditMods::ctrl());
        assert_eq!(e.text(), "");
    }

    #[test]
    fn a_grapheme_goes_as_one_backspace() {
        let mut e = TextEdit::with_text("a🇧🇷");
        press(&mut e, Keysym::BackSpace, EditMods::default());
        assert_eq!(e.text(), "a", "a flag is one grapheme, not two code points");
    }

    #[test]
    fn escape_and_enter_are_reported_not_swallowed_silently() {
        let mut e = TextEdit::with_text("x");
        assert_eq!(
            press(&mut e, Keysym::Escape, EditMods::default()),
            EditOutcome::Cancel
        );
        assert_eq!(
            press(&mut e, Keysym::Return, EditMods::default()),
            EditOutcome::Activate
        );
        assert_eq!(
            press(&mut e, Keysym::Return, EditMods::ctrl()),
            EditOutcome::Ignored,
            "Ctrl-Enter belongs to the caller (open in new window)"
        );
    }

    #[test]
    fn a_super_binding_is_never_the_entrys() {
        let mut e = TextEdit::with_text("x");
        let mods = EditMods {
            logo: true,
            ..EditMods::default()
        };
        assert_eq!(
            e.handle_key(Some(Keysym::a), Some('a'), mods, KeyTheme::Default),
            EditOutcome::Ignored
        );
        assert_eq!(e.text(), "x");
    }

    #[test]
    fn unclaimed_keys_fall_through() {
        let mut e = TextEdit::new();
        assert_eq!(
            press(&mut e, Keysym::F5, EditMods::default()),
            EditOutcome::Ignored
        );
        assert_eq!(
            press(&mut e, Keysym::Tab, EditMods::default()),
            EditOutcome::Ignored
        );
        assert_eq!(
            press(&mut e, Keysym::b, EditMods::ctrl()),
            EditOutcome::Ignored,
            "Ctrl-b is an Emacs binding; the default theme must not claim it"
        );
    }

    #[test]
    fn backspace_on_an_empty_entry_is_still_claimed() {
        let mut e = TextEdit::new();
        assert_eq!(
            press(&mut e, Keysym::BackSpace, EditMods::default()),
            EditOutcome::Moved,
            "claimed but inert — falling through would fire an unrelated bind"
        );
    }

    // ---- Emacs key theme ----

    #[test]
    fn emacs_ctrl_b_and_f_move_by_character() {
        let mut e = TextEdit::with_text("abc");
        press_emacs(&mut e, Keysym::b, EditMods::ctrl());
        assert_eq!(e.cursor(), 2);
        press_emacs(&mut e, Keysym::f, EditMods::ctrl());
        assert_eq!(e.cursor(), 3);
    }

    #[test]
    fn emacs_ctrl_a_is_beginning_of_line_not_select_all() {
        let mut e = TextEdit::with_text("abc");
        press_emacs(&mut e, Keysym::a, EditMods::ctrl());
        assert_eq!(e.cursor(), 0);
        assert_eq!(e.selection(), None);
        press_emacs(&mut e, Keysym::e, EditMods::ctrl());
        assert_eq!(e.cursor(), 3);
    }

    #[test]
    fn emacs_ctrl_d_deletes_forward_and_ctrl_w_kills_a_word() {
        let mut e = TextEdit::with_text("one two");
        press_emacs(&mut e, Keysym::a, EditMods::ctrl());
        press_emacs(&mut e, Keysym::d, EditMods::ctrl());
        assert_eq!(e.text(), "ne two");
        press_emacs(&mut e, Keysym::e, EditMods::ctrl());
        press_emacs(&mut e, Keysym::w, EditMods::ctrl());
        assert_eq!(e.text(), "ne ");
    }

    #[test]
    fn emacs_ctrl_y_yanks_what_ctrl_k_killed() {
        let mut e = TextEdit::with_text("one two");
        press_emacs(&mut e, Keysym::a, EditMods::ctrl());
        press_emacs(&mut e, Keysym::k, EditMods::ctrl());
        assert_eq!(e.text(), "");
        press_emacs(&mut e, Keysym::y, EditMods::ctrl());
        assert_eq!(e.text(), "one two");
        assert_eq!(e.cursor(), 7);
    }

    #[test]
    fn the_default_theme_still_has_gnomes_own_emacs_pair() {
        // `Ctrl-k`/`Ctrl-u` are St's, unconditional (`st-entry.c:743-762`) — they must
        // work without the key theme, unlike `Ctrl-w`/`Ctrl-y`.
        let mut e = TextEdit::with_text("one two");
        assert_eq!(
            press(&mut e, Keysym::u, EditMods::ctrl()),
            EditOutcome::Changed
        );
        assert_eq!(e.text(), "");
        assert_eq!(
            press(&mut e, Keysym::w, EditMods::ctrl()),
            EditOutcome::Ignored,
            "Ctrl-w is Emacs-only"
        );
    }

    /// The kill buffer is an Emacs-only affordance, and a *password* copy anywhere else.
    /// `Ctrl-u` reaches both password entries in the default theme, where GNOME simply
    /// discards; remembering it there would leave the whole secret in a second allocation.
    #[test]
    fn the_default_theme_never_fills_the_kill_buffer() {
        let mut e = TextEdit::with_text("hunter2");
        press(&mut e, Keysym::u, EditMods::ctrl());
        assert_eq!(e.text(), "");
        // Nothing to yank back, because nothing was kept. (`Ctrl-y` is Emacs-only, so ask in
        // that theme — if the default theme had filled the buffer, this would paste.)
        press_emacs(&mut e, Keysym::y, EditMods::ctrl());
        assert_eq!(
            e.text(),
            "",
            "the default theme's Ctrl-u must not have kept a copy"
        );
        assert!(e.kill.is_empty());
    }

    /// `secure_clear` zeroes the kill buffer too — otherwise an Emacs user's `Ctrl-k` leaves
    /// the password behind after the dialog has "cleared" itself.
    #[test]
    fn a_preedit_is_shown_but_is_not_the_text() {
        let mut e = TextEdit::with_text("ab");
        // Caret is at the end after `with_text`.
        assert!(e.set_preedit(Some("ñ".to_owned())));
        assert_eq!(e.display(None), "abñ");
        // The composition is emphatically *not* input yet: submitting here must give "ab".
        assert_eq!(e.text(), "ab");
        // The caret sits after what is being composed, not inside it.
        assert_eq!(e.display_cursor(), "abñ".len());

        // Committing is the engine inserting it for real, and the preedit going away.
        assert!(e.set_preedit(None));
        e.insert_str("ñ");
        assert_eq!(e.text(), "abñ");
        assert_eq!(e.display(None), "abñ");
        // Setting the same value twice reports no change, so callers can skip a redraw.
        assert!(!e.set_preedit(None));
    }

    #[test]
    fn a_preedit_shows_in_the_clear_inside_a_password() {
        // GNOME's behavior: `ClutterText` masks its buffer and then splices the *raw* preedit
        // into the result (`clutter-text.c:848-877`). A composition nobody can see is one
        // nobody can steer, and it lasts only until the engine commits.
        let mut e = TextEdit::with_capacity(64);
        e.insert_str("hunter2");
        e.set_preedit(Some("ñ".to_owned()));

        let shown = e.display(Some('\u{25cf}'));
        assert_eq!(shown, "\u{25cf}".repeat(7) + "ñ");
        assert!(
            !shown.contains("hunter2"),
            "the secret itself must still be masked"
        );

        // And the caret maps past the composition in masked coordinates.
        let (cursor, _) = e.masked_positions('\u{25cf}', true);
        assert_eq!(cursor, Some(shown.len()));
    }

    #[test]
    fn secure_clear_drops_the_composition_too() {
        let mut e = TextEdit::with_capacity(64);
        e.insert_str("hunter2");
        e.set_preedit(Some("ñ".to_owned()));
        e.secure_clear(64);
        assert_eq!(e.preedit(), None, "a composition is as much of the secret");
        assert_eq!(e.display(Some('\u{25cf}')), "");
    }

    #[test]
    fn secure_clear_zeroes_the_kill_buffer_as_well() {
        let mut e = TextEdit::with_capacity(512);
        e.insert_str("hunter2");
        press_emacs(&mut e, Keysym::a, EditMods::ctrl());
        press_emacs(&mut e, Keysym::k, EditMods::ctrl());
        assert_eq!(e.kill, "hunter2", "the Emacs theme does keep it");
        e.secure_clear(512);
        assert!(e.kill.is_empty(), "and secure_clear takes it with the text");
        press_emacs(&mut e, Keysym::y, EditMods::ctrl());
        assert_eq!(e.text(), "", "nothing survives to be yanked back");
    }

    #[test]
    fn key_theme_parses_the_gsettings_value() {
        assert_eq!(KeyTheme::from_setting("Emacs"), KeyTheme::Emacs);
        assert_eq!(KeyTheme::from_setting("emacs"), KeyTheme::Emacs);
        assert_eq!(KeyTheme::from_setting("Default"), KeyTheme::Default);
        assert_eq!(KeyTheme::from_setting(""), KeyTheme::Default);
    }
}
