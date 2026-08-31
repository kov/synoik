// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The emoji picker: a search entry over a scrolling grid, anchored at the caret.
//!
//! Unlike every other shell surface that takes keys, the picker **must not appear in
//! `KeyboardFocus`**. `text-input-v3` enter/leave rides `wl_keyboard` focus and every shell-owned
//! focus variant has no surface, so becoming the focus would take the client's text input away —
//! the very thing the picker exists to commit into. The client keeps `wl_keyboard` focus
//! throughout and the picker reads keys out of the input filter instead.
//!
//! The search entry is therefore a plain [`TextEdit`], not a `ShellEntry`: routing it through the
//! input method would move `ImFocus` off the client and reset the engine. No dead keys in the
//! search box, which costs nothing for ASCII emoji names.
//!
//! See `docs/fork/emoji-picker.md`.

use std::cell::RefCell;

use smithay::backend::renderer::element::Kind;
use smithay::input::keyboard::Keysym;
use smithay::output::Output;
use smithay::utils::{Logical, Physical, Point, Rectangle, Size, Transform};

use crate::emoji::{self, Emoji};
use crate::gnome::MAX_EMOJI_RECENTS;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanFrame, VulkanRenderer};
use crate::synoik_render_elements;
use crate::ui::text_edit::{EditMods, EditOutcome, KeyTheme, TextEdit};
use crate::ui::widget::{self, Align, ContentCache, Painter, ShapedText, TextShaper};
use crate::utils::to_physical_precise_round;

/// One grid cell, logical px — the hit box, not the glyph.
const CELL: f64 = 40.;
/// The emoji's em size inside a cell. Smaller than the cell so neighbours do not touch.
const GLYPH_PX: f64 = 26.;
const COLS: usize = 9;
/// Visible rows. The grid scrolls; the panel does not resize.
const ROWS: usize = 6;
/// Padding inside the panel, logical px.
const PAD: f64 = 12.;
/// Gap between the entry and the grid, and between the panel and its anchor.
const GAP: f64 = 8.;
/// Panel background — the same card as the dialogs.
const BG: widget::Rgba = widget::style::DIALOG_BG;
/// Height of the category rail along the panel's bottom edge.
const RAIL_H: f64 = 36.;
/// A rail tab's width: the rail spans the grid, so ten tabs are narrower than nine cells.
const TAB_W: f64 = CELL * COLS as f64 / RAIL_LABELS.len() as f64;

/// The rail label's em size — smaller than a grid cell's, it is a tab not a choice.
const RAIL_PX: f64 = 20.;
/// Columns in the skin-tone popover. A one-person emoji has five spellings and fits one row; a
/// two-person one has up to 25, which wrap into five.
const TONE_COLS: usize = 5;
/// Padding inside the tone popover.
const TONE_PAD: f64 = 6.;

/// The rail's tabs: the recents first, then one emoji per Unicode group in group order.
///
/// GNOME picks the same nine group labels for its on-screen keyboard's section keys
/// (`EmojiSelection._sections`, `js/ui/keyboard.js:884-894`); we take its labels rather than
/// inventing our own, and index them by position because our table's groups are Unicode's own
/// order, which is the order that list is in. The recents tab is GTK's chooser's, which opens on
/// it (`emoji-recent-symbolic`, `gtkemojichooser.c`); GNOME's keyboard has no history to show.
const RAIL_LABELS: [&str; 10] = [
    "\u{1f552}",
    "\u{1f642}",
    "\u{1f44d}",
    "\u{1f337}",
    "\u{1f374}",
    "\u{2708}\u{fe0f}",
    "\u{1f3c3}",
    "\u{1f514}",
    "\u{2764}\u{fe0f}",
    "\u{1f6a9}",
];

synoik_render_elements! {
    EmojiPickerRenderElement => {
        Texture = TextureRenderElement<VkTexture>,
    }
}

/// What a key on the open picker asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyOutcome {
    /// The picker used it, and it goes no further.
    Handled,
    /// The picker is closing; the key still goes no further.
    Close,
    /// Insert this text and close.
    Insert(String),
}

/// Where an open picker lives. Resolved once, at open: the anchor is the caret *as it was when
/// the user asked*, and a client that keeps editing underneath must not drag the picker around.
#[derive(Debug, Clone)]
struct Open {
    anchor: Rectangle<f64, Logical>,
    output: Output,
    /// The output's geometry in global coordinates, so [`EmojiPicker::geometry`] is a pure
    /// function of stored state and can be asked from the input path and the render path alike.
    output_geo: Rectangle<f64, Logical>,
}

/// One place in the grid: an entry in the table, and which of its skin-tone spellings this place
/// stands for.
///
/// The tone has to ride alongside the index because a tone spelling is not an entry of its own —
/// `tools/emoji-table` folds it into its base's `tones` — and a recent is remembered as the text
/// it inserted, tone included.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Slot {
    at: usize,
    tone: Option<usize>,
}

impl Slot {
    fn plain(at: usize) -> Self {
        Self { at, tone: None }
    }

    fn emoji(&self) -> &'static Emoji {
        &emoji::table().entries()[self.at]
    }

    /// What this place draws and inserts. An explicit tone wins; otherwise the remembered one
    /// applies to any emoji that takes tones.
    fn text(&self, default_tone: Option<usize>) -> &'static str {
        let emoji = self.emoji();
        self.tone
            .or(default_tone)
            .and_then(|t| emoji.tones.get(t))
            .copied()
            .unwrap_or(emoji.ch)
    }
}

#[derive(Default)]
pub struct EmojiPicker {
    open: Option<Open>,
    search: TextEdit,
    /// The grid's places, in display order.
    view: Vec<Slot>,
    /// The picker's history, newest first, as the strings it inserted — the shape
    /// `org.synoik.emoji recently-used-emoji` stores and GTK's chooser history imports into.
    recents: Vec<String>,
    /// The recents tab is showing.
    ///
    /// The nine group tabs are positions *within* the table's order, so the rail derives them
    /// from the selection; the recents tab is a list of its own and has to be state.
    on_recents: bool,
    /// Position in `view`; meaningless when `view` is empty.
    selected: usize,
    hovered: Option<usize>,
    /// The rail tab under the pointer. Its own field rather than part of `hovered`: a tab is not
    /// a cell, and the two can never be hovered at once.
    hovered_tab: Option<usize>,
    /// First visible row of the grid.
    first_row: usize,
    /// The cell whose skin-tone popover is open, and the position selected inside it.
    tone: Option<(usize, usize)>,
    /// The tone last picked, applied to any toned emoji picked without opening the popover.
    ///
    /// GNOME's on-screen keyboard has no such memory, but every other picker does, and picking a
    /// tone once and then having to pick it again for every emoji is the alternative. In memory
    /// only for now: persisting it belongs with the recents, whose GTK records carry a modifier
    /// field of their own.
    default_tone: Option<usize>,
    revision: u64,
    cache: RefCell<ContentCache>,
    tone_cache: RefCell<ContentCache>,
    entry_cache: RefCell<widget::BakeCache>,
}

impl EmojiPicker {
    /// The panel's size, logical px. Fixed: a grid that resized as the search narrowed would move
    /// the cell under the pointer on every keystroke.
    pub const WIDTH: f64 = PAD * 2. + CELL * COLS as f64;
    pub const HEIGHT: f64 =
        PAD * 2. + widget::Entry::HEIGHT + GAP + CELL * ROWS as f64 + GAP + RAIL_H;

    fn size() -> Size<f64, Logical> {
        Size::from((Self::WIDTH, Self::HEIGHT))
    }

    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// Where the picker was opened, in global coordinates — the caret, or the pointer.
    pub fn anchor(&self) -> Option<Rectangle<f64, Logical>> {
        self.open.as_ref().map(|open| open.anchor)
    }

    /// The output the picker was opened on: the one owning the anchor.
    pub fn output(&self) -> Option<&Output> {
        self.open.as_ref().map(|open| &open.output)
    }

    pub fn open(
        &mut self,
        anchor: Rectangle<f64, Logical>,
        output: Output,
        output_geo: Rectangle<f64, Logical>,
    ) {
        self.open = Some(Open {
            anchor,
            output,
            output_geo,
        });
        self.search.clear();
        self.hovered = None;
        self.hovered_tab = None;
        self.selected = 0;
        self.first_row = 0;
        self.tone = None;
        // On the recents when there are any, like GTK's chooser: what you picked last is what you
        // are most likely to want, and it is one row rather than a scroll.
        self.on_recents = !self.recents.is_empty();
        self.rebuild_view();
        self.revision += 1;
    }

    /// Seed the history from settings. Takes effect at the next [`open`](Self::open).
    pub fn set_recents(&mut self, recents: Vec<String>) {
        self.recents = recents;
    }

    /// Record a pick and return the history to persist: newest first, no repeats, capped where
    /// GTK caps its own (`MAX_RECENT`, `gtkemojichooser.c`).
    pub fn record_pick(&mut self, text: &str) -> Vec<String> {
        self.recents.retain(|e| e != text);
        self.recents.insert(0, text.to_owned());
        self.recents.truncate(MAX_EMOJI_RECENTS);
        self.recents.clone()
    }

    /// Closes, and says whether it was open — so a caller can skip the redraw when it was not.
    pub fn close(&mut self) -> bool {
        self.open.take().is_some()
    }

    /// The panel's rectangle in global coordinates: below the anchor when it fits, above it when
    /// it does not, clamped to the output either way.
    pub fn geometry(&self) -> Option<Rectangle<f64, Logical>> {
        let open = self.open.as_ref()?;
        let size = Self::size();
        let out = open.output_geo;

        let below = open.anchor.loc.y + open.anchor.size.h + GAP;
        let y = if below + size.h <= out.loc.y + out.size.h {
            below
        } else {
            // Above the caret, so the text being edited stays visible.
            open.anchor.loc.y - GAP - size.h
        };

        let clamp = |v: f64, lo: f64, span: f64, len: f64| v.min(lo + span - len).max(lo);
        let loc = Point::from((
            clamp(open.anchor.loc.x, out.loc.x, out.size.w, size.w),
            clamp(y, out.loc.y, out.size.h, size.h),
        ));
        Some(Rectangle::new(loc, size))
    }

    /// The entry the selection is on, as an index into the table.
    pub fn selected_entry(&self) -> usize {
        self.view.get(self.selected).map_or(0, |slot| slot.at)
    }

    /// Whether the skin-tone popover is up.
    pub fn tone_is_open(&self) -> bool {
        self.tone.is_some()
    }

    /// The tone popover's rectangle and column count, for hit-testing from a test.
    pub fn tone_geometry(&self) -> Option<(Rectangle<f64, Logical>, usize)> {
        self.tone_rect()
    }

    /// Which cell the pointer is over, if any — what the render lights.
    pub fn hovered_index(&self) -> Option<usize> {
        self.hovered
    }

    pub fn contains(&self, pos: Point<f64, Logical>) -> bool {
        if self.tone_rect().is_some_and(|(rect, _)| rect.contains(pos)) {
            return true;
        }
        self.geometry().is_some_and(|geo| geo.contains(pos))
    }

    /// The emoji at a position in `view` — the base entry, whatever tone the place stands for.
    fn at(&self, index: usize) -> Option<&'static Emoji> {
        Some(self.view.get(index)?.emoji())
    }

    fn rows(&self) -> usize {
        self.view.len().div_ceil(COLS)
    }

    /// Rebuild the visible list from the search text and the latched tab.
    ///
    /// A search outranks the tab — typing is how you leave one. An empty search on a group tab
    /// shows the whole table in Unicode order, which groups related emoji together; the rail then
    /// scrolls within it. Nothing is filtered out: Unicode's `Component` group — the bare
    /// skin-tone swatches and hair components, which are modifiers rather than emoji anyone
    /// picks — carries the status `component` rather than `fully-qualified`, so
    /// `tools/emoji-table` never wrote them and the table has no such group at all.
    ///
    /// A recent the table cannot spell drops out of the grid rather than the history: it is an
    /// emoji from a newer Unicode than the vendored table, and it comes back when the table does.
    fn rebuild_view(&mut self) {
        let table = emoji::table();
        let query = self.search.text().trim();
        self.view = if !query.is_empty() {
            table
                .search_indices(query)
                .into_iter()
                .map(Slot::plain)
                .collect()
        } else if self.on_recents {
            self.recents
                .iter()
                .filter_map(|text| table.resolve(text))
                .map(|(at, tone)| Slot { at, tone })
                .collect()
        } else {
            (0..table.entries().len()).map(Slot::plain).collect()
        };
        self.selected = 0;
        self.first_row = 0;
    }

    /// Keep the selected cell on screen after a move.
    fn scroll_to_selected(&mut self) {
        let row = self.selected / COLS;
        if row < self.first_row {
            self.first_row = row;
        } else if row >= self.first_row + ROWS {
            self.first_row = row + 1 - ROWS;
        }
    }

    /// Move the selection by whole cells, clamped to the view.
    fn move_selection(&mut self, delta: isize) {
        if self.view.is_empty() {
            return;
        }
        let last = self.view.len() as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
        self.scroll_to_selected();
        self.revision += 1;
    }

    /// The spellings the tone popover offers for a cell: the base first, then its variants, so
    /// the popover can always put the plain form back.
    fn tone_choices(&self, index: usize) -> Option<Vec<&'static str>> {
        let emoji = self.at(index)?;
        if !emoji.has_tones() {
            return None;
        }
        Some(
            std::iter::once(emoji.ch)
                .chain(emoji.tones.iter().copied())
                .collect(),
        )
    }

    /// What `index` draws and inserts: the place's own tone if it has one (a recent carries the
    /// tone it was picked with), else the remembered tone, else the base.
    fn text_of(&self, index: usize) -> Option<&'static str> {
        Some(self.view.get(index)?.text(self.default_tone))
    }

    fn spelling_of(&self, index: usize) -> Option<String> {
        Some(self.text_of(index)?.to_owned())
    }

    /// Open the tone popover on a cell, if it has tones. Returns whether it opened.
    fn open_tones(&mut self, index: usize) -> bool {
        if self.tone_choices(index).is_none() {
            return false;
        }
        // Start on the remembered tone, so the popover opens showing what a plain pick would do.
        let at = self.default_tone.map_or(0, |t| t + 1);
        self.tone = Some((index, at));
        self.revision += 1;
        true
    }

    /// The tone popover's rectangle in global coordinates, and how many columns it has.
    fn tone_rect(&self) -> Option<(Rectangle<f64, Logical>, usize)> {
        let (index, _) = self.tone?;
        let choices = self.tone_choices(index)?;
        let geo = self.geometry()?;
        let cols = choices.len().min(TONE_COLS);
        let rows = choices.len().div_ceil(TONE_COLS);
        let size = Size::from((
            cols as f64 * CELL + TONE_PAD * 2.,
            rows as f64 * CELL + TONE_PAD * 2.,
        ));

        let cell = self.cell_global_rect(index)?;
        // Centred over the cell, above it when there is room — the row being varied stays visible.
        let mut loc = Point::from((
            cell.loc.x + cell.size.w / 2. - size.w / 2.,
            cell.loc.y - GAP - size.h,
        ));
        if loc.y < geo.loc.y {
            loc.y = cell.loc.y + cell.size.h + GAP;
        }
        // Clamped to the panel horizontally: a popover hanging off the card reads as a glitch.
        loc.x = loc.x.clamp(geo.loc.x, geo.loc.x + geo.size.w - size.w);
        Some((Rectangle::new(loc, size), cols))
    }

    /// A visible cell's rectangle in global coordinates.
    fn cell_global_rect(&self, index: usize) -> Option<Rectangle<f64, Logical>> {
        let geo = self.geometry()?;
        let slot = index.checked_sub(self.first_row * COLS)?;
        if slot >= COLS * ROWS {
            return None;
        }
        Some(Rectangle::new(
            geo.loc + cell_rect(slot).loc,
            (CELL, CELL).into(),
        ))
    }

    /// The rail's rectangle in global coordinates.
    fn rail_rect(&self, geo: Rectangle<f64, Logical>) -> Rectangle<f64, Logical> {
        Rectangle::new(
            geo.loc + Point::from((PAD, Self::HEIGHT - PAD - RAIL_H)),
            Size::from((CELL * COLS as f64, RAIL_H)),
        )
    }

    /// Jump the rail to a tab: 0 is the recents, the rest are the Unicode groups in order.
    ///
    /// Clears the search, because the tabs index lists a search result is not in.
    fn go_to_tab(&mut self, tab: usize) {
        if tab >= RAIL_LABELS.len() {
            return;
        }
        let on_recents = tab == 0;
        let stale = on_recents != self.on_recents || !self.search.text().is_empty();
        self.on_recents = on_recents;
        if stale {
            self.search.clear();
            self.rebuild_view();
        }
        let start = match emoji::table().groups().get(tab.wrapping_sub(1)) {
            Some(group) => group.entries.start,
            // The recents tab, and any group the table does not have.
            None => 0,
        };
        self.selected = start.min(self.view.len().saturating_sub(1));
        let max = self.rows().saturating_sub(ROWS);
        self.first_row = (self.selected / COLS).min(max);
        self.tone = None;
        self.revision += 1;
    }

    /// Which group the rail latches: the one the selection is in.
    ///
    /// The selection rather than the first visible row, because a row straddles two groups
    /// whenever a group's length is not a multiple of the column count — which is nearly always —
    /// and a tab that lights up for the group you are *leaving* reads as a bug.
    ///
    /// `None` while searching: the view is not the table's order, so no tab describes it.
    fn current_group(&self) -> Option<usize> {
        if !self.search.text().is_empty() || self.on_recents {
            return None;
        }
        let at = self.view.get(self.selected)?.at;
        emoji::table()
            .groups()
            .iter()
            .position(|g| g.entries.contains(&at))
    }

    /// Which tab the rail latches — the recents when it is showing, else the selection's group.
    fn current_tab(&self) -> Option<usize> {
        // A search outranks the latched tab in the view, so it does in the rail too.
        if !self.search.text().is_empty() {
            return None;
        }
        if self.on_recents {
            return Some(0);
        }
        self.current_group().map(|group| group + 1)
    }

    /// Feed a key to the open picker.
    pub fn handle_key(
        &mut self,
        raw: Option<Keysym>,
        text: Option<char>,
        mods: EditMods,
        theme: KeyTheme,
        pressed: bool,
    ) -> KeyOutcome {
        if !pressed {
            return KeyOutcome::Handled;
        }

        // The tone popover is modal over the grid while it is up: it is a choice about one cell,
        // so nothing behind it moves until it is answered or dismissed.
        if let Some((index, at)) = self.tone {
            let choices = self.tone_choices(index).unwrap_or_default();
            let step = |at: usize, delta: isize| {
                (at as isize + delta).clamp(0, choices.len() as isize - 1) as usize
            };
            match raw {
                Some(Keysym::Escape) => {
                    self.tone = None;
                    self.revision += 1;
                }
                Some(Keysym::Return | Keysym::KP_Enter | Keysym::ISO_Enter) => {
                    // Position 0 is the plain form, so picking it *forgets* the tone rather than
                    // remembering one — otherwise there would be no way back to the base.
                    self.default_tone = at.checked_sub(1);
                    self.tone = None;
                    return match choices.get(at) {
                        Some(ch) => KeyOutcome::Insert((*ch).to_owned()),
                        None => KeyOutcome::Handled,
                    };
                }
                Some(Keysym::Left) => self.tone = Some((index, step(at, -1))),
                Some(Keysym::Right) => self.tone = Some((index, step(at, 1))),
                Some(Keysym::Up) => self.tone = Some((index, step(at, -(TONE_COLS as isize)))),
                Some(Keysym::Down) => self.tone = Some((index, step(at, TONE_COLS as isize))),
                _ => return KeyOutcome::Handled,
            }
            self.revision += 1;
            return KeyOutcome::Handled;
        }

        match raw {
            Some(Keysym::Escape) => return KeyOutcome::Close,
            Some(Keysym::Return | Keysym::KP_Enter | Keysym::ISO_Enter) => {
                // Shift opens the variants instead of picking, the keyboard's answer to the
                // secondary click — "the same thing, varied".
                if mods.shift && self.open_tones(self.selected) {
                    return KeyOutcome::Handled;
                }
                return match self.spelling_of(self.selected) {
                    Some(ch) => KeyOutcome::Insert(ch),
                    // Nothing matched the search; Enter has nothing to insert and is not a close.
                    None => KeyOutcome::Handled,
                };
            }
            Some(Keysym::Tab) => {
                // With no tab latched — mid-search — Shift+Tab walks back into the recents and
                // Tab forward into the first group, from the notional place between them.
                let tab = self.current_tab().unwrap_or(0);
                let next = if mods.shift {
                    tab.saturating_sub(1)
                } else {
                    (tab + 1).min(RAIL_LABELS.len() - 1)
                };
                self.go_to_tab(next);
            }
            Some(Keysym::Left) => self.move_selection(-1),
            Some(Keysym::Right) => self.move_selection(1),
            Some(Keysym::Up) => self.move_selection(-(COLS as isize)),
            Some(Keysym::Down) => self.move_selection(COLS as isize),
            Some(Keysym::Page_Up) => self.move_selection(-((COLS * ROWS) as isize)),
            Some(Keysym::Page_Down) => self.move_selection((COLS * ROWS) as isize),
            _ => {
                // Everything else is the search entry's: caret motion, word deletion, selection,
                // `Ctrl-u`/`Ctrl-k`, the Emacs theme.
                match self.search.handle_key(raw, text, mods, theme) {
                    EditOutcome::Changed => {
                        self.rebuild_view();
                        self.revision += 1;
                    }
                    // Escape and Return are claimed above, so neither reaches the entry.
                    EditOutcome::Activate
                    | EditOutcome::Moved
                    | EditOutcome::Cancel
                    | EditOutcome::Ignored => self.revision += 1,
                }
            }
        }

        KeyOutcome::Handled
    }

    /// Which cell a global position falls in, if any.
    fn cell_at(&self, pos: Point<f64, Logical>) -> Option<usize> {
        let geo = self.geometry()?;
        let grid = self.grid_rect(geo);
        if !grid.contains(pos) {
            return None;
        }
        let col = ((pos.x - grid.loc.x) / CELL) as usize;
        let row = ((pos.y - grid.loc.y) / CELL) as usize;
        if col >= COLS || row >= ROWS {
            return None;
        }
        let index = (self.first_row + row) * COLS + col;
        (index < self.view.len()).then_some(index)
    }

    fn grid_rect(&self, geo: Rectangle<f64, Logical>) -> Rectangle<f64, Logical> {
        Rectangle::new(
            geo.loc + Point::from((PAD, PAD + widget::Entry::HEIGHT + GAP)),
            Size::from((CELL * COLS as f64, CELL * ROWS as f64)),
        )
    }

    /// Track the pointer over the grid and the rail. Returns whether anything changed.
    pub fn pointer_motion(&mut self, pos: Point<f64, Logical>) -> bool {
        let hovered = self.cell_at(pos);
        let hovered_tab = self.rail_at(pos);
        if hovered == self.hovered && hovered_tab == self.hovered_tab {
            return false;
        }
        self.hovered = hovered;
        self.hovered_tab = hovered_tab;
        self.revision += 1;
        true
    }

    /// Which rail tab the pointer is over — what the render lights.
    pub fn hovered_tab(&self) -> Option<usize> {
        self.hovered_tab
    }

    /// Which position in the open tone popover a point falls on.
    fn tone_at(&self, pos: Point<f64, Logical>) -> Option<usize> {
        let (rect, cols) = self.tone_rect()?;
        let inner = Rectangle::new(
            rect.loc + Point::from((TONE_PAD, TONE_PAD)),
            rect.size - Size::from((TONE_PAD * 2., TONE_PAD * 2.)),
        );
        if !inner.contains(pos) {
            return None;
        }
        let col = ((pos.x - inner.loc.x) / CELL) as usize;
        let row = ((pos.y - inner.loc.y) / CELL) as usize;
        if col >= cols {
            return None;
        }
        let at = row * TONE_COLS + col;
        let (index, _) = self.tone?;
        (at < self.tone_choices(index)?.len()).then_some(at)
    }

    /// A click inside the picker. Returns the text to insert, if it landed on an emoji.
    ///
    /// `secondary` is the right button, which opens the skin-tone variants instead of picking —
    /// the pointer's answer to Shift+Return, and what GNOME's on-screen keyboard reaches by a long
    /// press (`Key._showSubkeys`, `js/ui/keyboard.js`). A long press is deferred.
    pub fn pointer_click(&mut self, pos: Point<f64, Logical>, secondary: bool) -> Option<String> {
        if self.tone.is_some() {
            let at = self.tone_at(pos);
            let (index, _) = self.tone?;
            let Some(at) = at else {
                // A click anywhere else dismisses the popover without picking.
                self.tone = None;
                self.revision += 1;
                return None;
            };
            self.default_tone = at.checked_sub(1);
            let ch = self.tone_choices(index)?.get(at).copied()?;
            self.tone = None;
            return Some(ch.to_owned());
        }

        if let Some(tab) = self.rail_at(pos) {
            self.go_to_tab(tab);
            return None;
        }

        let index = self.cell_at(pos)?;
        self.selected = index;
        self.revision += 1;
        if secondary && self.open_tones(index) {
            return None;
        }
        self.spelling_of(index)
    }

    /// Which rail tab a point falls on.
    fn rail_at(&self, pos: Point<f64, Logical>) -> Option<usize> {
        let geo = self.geometry()?;
        let rail = self.rail_rect(geo);
        if !rail.contains(pos) {
            return None;
        }
        let tab = ((pos.x - rail.loc.x) / TAB_W) as usize;
        (tab < RAIL_LABELS.len()).then_some(tab)
    }

    /// Scroll the grid by whole rows. Returns whether anything moved.
    pub fn scroll_rows(&mut self, delta: isize) -> bool {
        let max = self.rows().saturating_sub(ROWS);
        let first = (self.first_row as isize + delta).clamp(0, max as isize) as usize;
        if first == self.first_row {
            return false;
        }
        self.first_row = first;
        // The cell under the pointer changed even though the pointer did not move.
        self.hovered = None;
        self.revision += 1;
        true
    }

    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        output: &Output,
        accent: [u8; 3],
        push: &mut dyn FnMut(EmojiPickerRenderElement),
    ) {
        let Some(open) = self.open.as_ref() else {
            return;
        };
        if open.output != *output {
            return;
        }
        let Some(geo) = self.geometry() else {
            return;
        };
        let _span = tracy_client::span!("EmojiPicker::render");

        let scale = output.current_scale().fractional_scale();
        // Panel-local, and pinned to a whole physical pixel so the entry composited on top lands
        // on the same grid the panel was baked on.
        let origin = (geo.loc - open.output_geo.loc)
            .to_physical_precise_round(scale)
            .to_logical(scale);

        let panel = {
            let mut cache = self.cache.borrow_mut();
            let cells = self.visible_cells();
            let latched = self.current_tab();
            let rail_hovered = self.hovered_tab;
            widget::bake_content(
                renderer,
                &mut cache,
                scale,
                self.revision,
                |renderer| prepare_panel(renderer, scale, &cells, latched, rail_hovered),
                |frame, phys, prepared| paint_panel(frame, phys, prepared, scale),
            )
        };

        let entry_rect = Rectangle::new(
            origin + Point::from((PAD, PAD)),
            Size::from((Self::WIDTH - PAD * 2., widget::Entry::HEIGHT)),
        );
        match widget::Entry::bake(
            renderer,
            &mut self.entry_cache.borrow_mut(),
            scale,
            entry_rect.size.w,
            entry_rect.size.h,
            widget::EntryContent::of(&self.search, "Search emoji", true),
            widget::EntryStyle::Dialog,
            true,
            false,
            widget::style::accent_rgba(accent),
            widget::Revision::new()
                .of(self.revision)
                .of(accent)
                .px(entry_rect.size.w)
                .px(entry_rect.size.h)
                .done(),
        ) {
            Ok(buffer) => push(EmojiPickerRenderElement::Texture(
                TextureRenderElement::from_texture_buffer(
                    buffer,
                    entry_rect.loc,
                    1.,
                    None,
                    None,
                    Kind::Unspecified,
                ),
            )),
            Err(err) => warn!("error drawing the emoji picker entry: {err:#}"),
        }

        match panel {
            Ok(texture) => {
                let buffer = TextureBuffer::from_texture(
                    renderer,
                    texture,
                    scale,
                    Transform::Normal,
                    Vec::new(),
                );
                push(EmojiPickerRenderElement::Texture(
                    TextureRenderElement::from_texture_buffer(
                        buffer,
                        origin,
                        1.,
                        None,
                        None,
                        Kind::Unspecified,
                    ),
                ));
            }
            Err(err) => warn!("error rendering the emoji picker: {err:#}"),
        }

        // The tone popover last, so it draws over the panel it hangs off.
        if let (Some((index, at)), Some((rect, cols))) = (self.tone, self.tone_rect()) {
            let Some(choices) = self.tone_choices(index) else {
                return;
            };
            let size = rect.size;
            let tones = widget::bake_content(
                renderer,
                &mut self.tone_cache.borrow_mut(),
                scale,
                self.revision,
                |renderer| prepare_tones(renderer, scale, &choices, at, size, cols),
                |frame, phys, prepared| paint_tones(frame, phys, prepared, scale),
            );
            match tones {
                Ok(texture) => {
                    let buffer = TextureBuffer::from_texture(
                        renderer,
                        texture,
                        scale,
                        Transform::Normal,
                        Vec::new(),
                    );
                    push(EmojiPickerRenderElement::Texture(
                        TextureRenderElement::from_texture_buffer(
                            buffer,
                            rect.loc - open.output_geo.loc,
                            1.,
                            None,
                            None,
                            Kind::Unspecified,
                        ),
                    ));
                }
                Err(err) => warn!("error rendering the emoji picker tones: {err:#}"),
            }
        }
    }

    /// The cells the grid currently shows, with their state — the render's whole input.
    fn visible_cells(&self) -> Vec<Cell> {
        let start = self.first_row * COLS;
        let end = (start + COLS * ROWS).min(self.view.len());
        (start..end)
            .map(|index| Cell {
                ch: self.view[index].text(self.default_tone),
                slot: index - start,
                selected: index == self.selected,
                hovered: self.hovered == Some(index),
            })
            .collect()
    }
}

/// One drawn cell: what it shows, where in the visible grid, and how it is lit.
struct Cell {
    ch: &'static str,
    slot: usize,
    selected: bool,
    hovered: bool,
}

/// The cell's rectangle, panel-local logical px.
fn cell_rect(slot: usize) -> Rectangle<f64, Logical> {
    let col = (slot % COLS) as f64;
    let row = (slot / COLS) as f64;
    Rectangle::new(
        Point::from((
            PAD + col * CELL,
            PAD + widget::Entry::HEIGHT + GAP + row * CELL,
        )),
        Size::from((CELL, CELL)),
    )
}

struct Prepared {
    glyphs: Vec<(usize, ShapedText)>,
    cells: Vec<(usize, bool, bool)>,
    rail: Vec<(usize, ShapedText)>,
    rail_latched: Option<usize>,
    rail_hovered: Option<usize>,
}

fn prepare_panel(
    renderer: &mut VulkanRenderer,
    scale: f64,
    cells: &[Cell],
    rail_latched: Option<usize>,
    rail_hovered: Option<usize>,
) -> anyhow::Result<(Size<i32, Physical>, Prepared)> {
    let _span = tracy_client::span!("emoji_picker::prepare_panel");

    let mut shaper = TextShaper::new(renderer, scale);
    let mut glyphs = Vec::with_capacity(cells.len());
    for cell in cells {
        // A glyph that will not shape must not take the whole panel down with it.
        match shaper.shape_emoji(cell.ch, GLYPH_PX) {
            Ok(shaped) => glyphs.push((cell.slot, shaped)),
            Err(err) => warn!("emoji {:?} did not shape: {err:#}", cell.ch),
        }
    }

    let mut rail = Vec::with_capacity(RAIL_LABELS.len());
    for (tab, label) in RAIL_LABELS.iter().enumerate() {
        match shaper.shape_emoji(label, RAIL_PX) {
            Ok(shaped) => rail.push((tab, shaped)),
            Err(err) => warn!("rail label {label:?} did not shape: {err:#}"),
        }
    }

    let px = |v: f64| to_physical_precise_round::<i32>(scale, v);
    let size = Size::<i32, Physical>::from((
        px(EmojiPicker::WIDTH).max(1),
        px(EmojiPicker::HEIGHT).max(1),
    ));
    Ok((
        size,
        Prepared {
            glyphs,
            cells: cells
                .iter()
                .map(|c| (c.slot, c.selected, c.hovered))
                .collect(),
            rail,
            rail_latched,
            rail_hovered,
        },
    ))
}

/// A rail tab's rectangle, panel-local logical px.
fn rail_tab_rect(tab: usize) -> Rectangle<f64, Logical> {
    Rectangle::new(
        Point::from((PAD + tab as f64 * TAB_W, EmojiPicker::HEIGHT - PAD - RAIL_H)),
        Size::from((TAB_W, RAIL_H)),
    )
}

fn paint_panel(
    frame: &mut VulkanFrame,
    phys: Size<i32, Physical>,
    prepared: &Prepared,
    scale: f64,
) -> anyhow::Result<()> {
    let mut p = Painter::new(frame, scale, phys);
    // Rounded borderless card: transparent clear so the corners composite away, then the fill.
    p.clear(widget::style::TRANSPARENT)?;
    p.fill_rounded_full(widget::style::DIALOG_RADIUS, BG)?;

    for (slot, selected, hovered) in &prepared.cells {
        if !selected && !hovered {
            continue;
        }
        let rect = cell_rect(*slot);
        // Selection reads stronger than hover, and one cell can be both.
        let wash = if *selected {
            widget::style::over(BG, widget::style::SELECTED_WASH)
        } else {
            widget::style::over(BG, widget::style::HOVER_WASH)
        };
        p.fill_rounded(rect, 8., wash)?;
    }

    for (slot, shaped) in &prepared.glyphs {
        let rect = cell_rect(*slot);
        p.text(
            shaped,
            rect.loc + rect.size.to_point().downscale(2.),
            Align::CENTER,
            widget::style::TEXT,
        )?;
    }

    // The rail, along the bottom edge: a hairline separating it from the grid, then the tabs.
    let rail_top = EmojiPicker::HEIGHT - PAD - RAIL_H - GAP / 2.;
    p.hairline(
        Rectangle::new(
            Point::from((PAD, rail_top)),
            Size::from((CELL * COLS as f64, 1.)),
        ),
        widget::style::BORDERS,
    )?;
    for (tab, shaped) in &prepared.rail {
        let rect = rail_tab_rect(*tab);
        // The latched tab reads stronger than a hovered one, and one tab can be both — the same
        // rule the grid cells follow.
        let wash = if prepared.rail_latched == Some(*tab) {
            Some(widget::style::SELECTED_WASH)
        } else if prepared.rail_hovered == Some(*tab) {
            Some(widget::style::HOVER_WASH)
        } else {
            None
        };
        if let Some(wash) = wash {
            p.fill_rounded(rect, 8., widget::style::over(BG, wash))?;
        }
        p.text(
            shaped,
            rect.loc + rect.size.to_point().downscale(2.),
            Align::CENTER,
            widget::style::TEXT,
        )?;
    }
    Ok(())
}

/// The tone popover's shaped choices.
struct PreparedTones {
    glyphs: Vec<(usize, ShapedText)>,
    selected: usize,
    cols: usize,
}

fn prepare_tones(
    renderer: &mut VulkanRenderer,
    scale: f64,
    choices: &[&'static str],
    selected: usize,
    size: Size<f64, Logical>,
    cols: usize,
) -> anyhow::Result<(Size<i32, Physical>, PreparedTones)> {
    let mut shaper = TextShaper::new(renderer, scale);
    let mut glyphs = Vec::with_capacity(choices.len());
    for (at, ch) in choices.iter().enumerate() {
        match shaper.shape_emoji(ch, GLYPH_PX) {
            Ok(shaped) => glyphs.push((at, shaped)),
            Err(err) => warn!("tone {ch:?} did not shape: {err:#}"),
        }
    }
    let px = |v: f64| to_physical_precise_round::<i32>(scale, v);
    Ok((
        Size::<i32, Physical>::from((px(size.w).max(1), px(size.h).max(1))),
        PreparedTones {
            glyphs,
            selected,
            cols,
        },
    ))
}

fn paint_tones(
    frame: &mut VulkanFrame,
    phys: Size<i32, Physical>,
    prepared: &PreparedTones,
    scale: f64,
) -> anyhow::Result<()> {
    let mut p = Painter::new(frame, scale, phys);
    p.clear(widget::style::TRANSPARENT)?;
    // A menu rather than a card: it hangs off a cell, so it takes the menu fill and a smaller
    // radius than the panel it sits on.
    p.fill_rounded_full(12., widget::style::MENU_BG)?;

    for (at, shaped) in &prepared.glyphs {
        let col = (at % prepared.cols) as f64;
        let row = (at / prepared.cols) as f64;
        let rect = Rectangle::new(
            Point::from((TONE_PAD + col * CELL, TONE_PAD + row * CELL)),
            Size::from((CELL, CELL)),
        );
        if *at == prepared.selected {
            p.fill_rounded(
                rect,
                8.,
                widget::style::over(widget::style::MENU_BG, widget::style::SELECTED_WASH),
            )?;
        }
        p.text(
            shaped,
            rect.loc + rect.size.to_point().downscale(2.),
            Align::CENTER,
            widget::style::TEXT,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picker() -> EmojiPicker {
        let mut picker = EmojiPicker::default();
        picker.rebuild_view();
        picker
    }

    /// An open picker with a history, without an output to open on: `open` needs one, so the
    /// tests that only care about the view drive its two effects directly.
    fn picker_with_recents(recents: &[&str]) -> EmojiPicker {
        let mut picker = EmojiPicker::default();
        picker.set_recents(recents.iter().map(|s| (*s).to_owned()).collect());
        picker.on_recents = !picker.recents.is_empty();
        picker.rebuild_view();
        picker
    }

    /// The rail answers the pointer the way the grid does: the tab under it lights, and the
    /// highlight follows or clears as the pointer moves off.
    #[test]
    fn the_rail_tracks_the_pointer() {
        let mut picker = EmojiPicker::default();
        picker.open(
            Rectangle::new(Point::from((100., 100.)), Size::from((0., 0.))),
            Output::new(
                "rail".to_owned(),
                smithay::output::PhysicalProperties {
                    size: (0, 0).into(),
                    subpixel: smithay::output::Subpixel::Unknown,
                    make: String::new(),
                    model: String::new(),
                    serial_number: String::new(),
                },
            ),
            Rectangle::new(Point::from((0., 0.)), Size::from((1920., 1080.))),
        );
        let geo = picker.geometry().unwrap();
        let tab_centre = |tab: usize| {
            geo.loc
                + Point::from((
                    PAD + TAB_W * (tab as f64 + 0.5),
                    EmojiPicker::HEIGHT - PAD - RAIL_H / 2.,
                ))
        };

        assert!(picker.pointer_motion(tab_centre(3)));
        assert_eq!(picker.hovered_tab(), Some(3));
        assert_eq!(picker.hovered_index(), None, "a tab is not a cell");

        assert!(!picker.pointer_motion(tab_centre(3)), "no move, no redraw");

        assert!(picker.pointer_motion(tab_centre(4)));
        assert_eq!(picker.hovered_tab(), Some(4));

        // Into the grid: the tab lets go and a cell takes over.
        let cell = geo.loc
            + Point::from((
                PAD + CELL / 2.,
                PAD + widget::Entry::HEIGHT + GAP + CELL / 2.,
            ));
        assert!(picker.pointer_motion(cell));
        assert_eq!(picker.hovered_tab(), None);
        assert_eq!(picker.hovered_index(), Some(0));
    }

    /// The history is newest-first with no repeats and a cap, the shape GTK's own chooser keeps
    /// (`add_recent_item`, `gtkemojichooser.c`): re-picking something moves it to the front
    /// rather than adding a second copy.
    #[test]
    fn a_pick_leads_the_history_without_repeating() {
        let mut picker = picker_with_recents(&["\u{1f600}", "\u{1f601}"]);

        assert_eq!(
            picker.record_pick("\u{1f602}"),
            ["\u{1f602}", "\u{1f600}", "\u{1f601}"],
            "a new pick leads"
        );
        assert_eq!(
            picker.record_pick("\u{1f600}"),
            ["\u{1f600}", "\u{1f602}", "\u{1f601}"],
            "an old one moves up rather than repeating"
        );

        for entry in emoji::table().entries().iter().take(MAX_EMOJI_RECENTS + 5) {
            picker.record_pick(entry.ch);
        }
        assert_eq!(picker.recents.len(), MAX_EMOJI_RECENTS, "and it is capped");
    }

    /// A recent is remembered as the text it inserted, so a toned pick comes back toned — and a
    /// tone spelling is not an entry of its own, which is the whole reason a cell carries a tone
    /// beside its index.
    #[test]
    fn a_toned_recent_comes_back_with_its_tone() {
        let toned = emoji::table()
            .entries()
            .iter()
            .find(|e| e.has_tones())
            .expect("the table has toned emoji")
            .tones[2];

        let picker = picker_with_recents(&[toned]);
        assert_eq!(picker.view.len(), 1);
        assert_eq!(picker.text_of(0), Some(toned));
        assert_eq!(
            picker.current_tab(),
            Some(0),
            "and the rail latches the recents"
        );
    }

    /// A recent from a newer Unicode than the vendored table has no cell to draw, and drops out
    /// of the grid — not out of the history, which is still what gets written back.
    #[test]
    fn a_recent_the_table_cannot_spell_drops_out_of_the_grid() {
        let picker = picker_with_recents(&["\u{1f600}", "\u{10fffd}"]);
        assert_eq!(picker.view.len(), 1, "one of the two has a cell");
        assert_eq!(picker.text_of(0), Some("\u{1f600}"));
        assert_eq!(picker.recents.len(), 2, "both are still the history");
    }

    /// Searching leaves the recents tab: a search result is not in either list's order, and the
    /// rail latches nothing while one is up.
    #[test]
    fn a_search_leaves_the_recents_tab() {
        let mut picker = picker_with_recents(&["\u{1f600}"]);
        picker.search.insert_str("cat");
        picker.rebuild_view();

        assert!(picker.view.len() > 1, "the search is over the whole table");
        assert_eq!(picker.current_tab(), None, "no tab describes a search");

        // Tab out of a search walks from the notional place before the first tab, so it lands
        // on the first group and Shift+Tab on the recents — neither skips one.
        press(&mut picker, Keysym::Tab, false);
        assert_eq!(picker.current_tab(), Some(1));
        picker.search.insert_str("cat");
        picker.rebuild_view();
        press(&mut picker, Keysym::Tab, true);
        assert_eq!(picker.current_tab(), Some(0));

        picker.search.insert_str("cat");
        picker.rebuild_view();
        picker.go_to_tab(0);
        assert!(picker.search.text().is_empty(), "the tab clears the search");
        assert_eq!(picker.view.len(), 1, "and the recents are back");
    }

    /// A skin-tone swatch on its own is a modifier, not an emoji, and must never appear as a
    /// cell. It does not, because Unicode marks the whole `Component` group `component` rather
    /// than `fully-qualified` and the generator keeps only the latter — so this pins a property of
    /// the vendored table that the picker relies on and does not enforce itself.
    #[test]
    fn no_bare_skin_tone_swatch_is_pickable() {
        let picker = picker();
        let table = emoji::table();
        assert!(
            !table.groups().iter().any(|g| g.name == "Component"),
            "the generator dropped the component group; the picker assumes that"
        );
        assert_eq!(picker.view.len(), table.entries().len());
        for index in &picker.view {
            let entry = index.emoji();
            assert!(
                !entry.name.ends_with("skin tone"),
                "{:?} is a modifier, not an emoji",
                entry.name
            );
        }
    }

    #[test]
    fn the_search_view_maps_back_to_the_right_entries() {
        let mut picker = picker();
        picker.search.set_text("thumbs up".to_owned());
        picker.rebuild_view();

        let first = picker.at(0).expect("thumbs up matches something");
        assert_eq!(first.ch, "\u{1f44d}", "got {:?}", first.name);
    }

    #[test]
    fn the_selection_scrolls_the_grid_and_stops_at_the_ends() {
        let mut picker = picker();
        assert_eq!(picker.first_row, 0);

        picker.move_selection(COLS as isize * ROWS as isize);
        assert_eq!(picker.selected, COLS * ROWS);
        assert_eq!(picker.first_row, 1, "one row scrolled into view");

        picker.move_selection(-(COLS as isize) * 100);
        assert_eq!(picker.selected, 0);
        assert_eq!(picker.first_row, 0, "and back off again");

        picker.move_selection(isize::MAX / 2);
        assert_eq!(picker.selected, picker.view.len() - 1, "clamped to the end");
    }

    fn press(picker: &mut EmojiPicker, sym: Keysym, shift: bool) -> KeyOutcome {
        picker.handle_key(
            Some(sym),
            None,
            EditMods {
                shift,
                ..EditMods::default()
            },
            KeyTheme::Default,
            true,
        )
    }

    /// The first entry that takes skin tones, so the tone tests do not hardcode a table offset
    /// that a Unicode bump would move.
    fn first_toned(picker: &EmojiPicker) -> usize {
        picker
            .view
            .iter()
            .position(|slot| slot.emoji().has_tones())
            .expect("the table has toned emoji")
    }

    /// The rail indexes the table's own order, so a search result is not in it — clicking a tab
    /// has to clear the search rather than jump to an index that means nothing.
    #[test]
    fn the_rail_jumps_to_a_group_clearing_the_search() {
        let mut picker = picker();
        picker.search.set_text("hat".to_owned());
        picker.rebuild_view();
        assert_eq!(picker.current_group(), None, "no tab describes a search");

        picker.go_to_tab(4);
        assert_eq!(picker.search.text(), "");
        assert_eq!(
            picker.current_group(),
            Some(3),
            "and the tab it jumped to is the latched one"
        );
        let group = &emoji::table().groups()[3];
        assert_eq!(
            picker.selected, group.entries.start,
            "landing on the group's first emoji"
        );
    }

    /// The popover offers the plain form first, so there is a way back from a remembered tone.
    /// Picking position 0 is what forgets it.
    #[test]
    fn the_tone_popover_leads_with_the_plain_form() {
        let mut picker = picker();
        picker.selected = first_toned(&picker);
        let base = picker.at(picker.selected).unwrap();
        let choices = picker.tone_choices(picker.selected).unwrap();
        assert_eq!(choices[0], base.ch);
        assert_eq!(choices.len(), base.tones.len() + 1);

        assert_eq!(
            press(&mut picker, Keysym::Return, true),
            KeyOutcome::Handled
        );
        assert!(picker.tone.is_some(), "Shift+Return opens the variants");

        // One right, then pick: the first tone.
        press(&mut picker, Keysym::Right, false);
        let picked = press(&mut picker, Keysym::Return, false);
        assert_eq!(picked, KeyOutcome::Insert(base.tones[0].to_owned()));
        assert_eq!(picker.default_tone, Some(0), "and it is remembered");
        assert!(picker.tone.is_none());

        // Back to the plain form forgets it again.
        press(&mut picker, Keysym::Return, true);
        press(&mut picker, Keysym::Left, false);
        assert_eq!(
            press(&mut picker, Keysym::Return, false),
            KeyOutcome::Insert(base.ch.to_owned())
        );
        assert_eq!(picker.default_tone, None);
    }

    /// A remembered tone applies to the *next* emoji picked plainly — the whole reason to
    /// remember one — and leaves emoji that take no tones alone.
    #[test]
    fn a_remembered_tone_applies_to_the_next_plain_pick() {
        let mut picker = picker();
        picker.default_tone = Some(2);

        let toned = first_toned(&picker);
        let base = picker.at(toned).unwrap();
        picker.selected = toned;
        assert_eq!(
            press(&mut picker, Keysym::Return, false),
            KeyOutcome::Insert(base.tones[2].to_owned())
        );

        let plain = picker
            .view
            .iter()
            .position(|slot| !slot.emoji().has_tones())
            .unwrap();
        picker.selected = plain;
        let plain_ch = picker.at(plain).unwrap().ch.to_owned();
        assert_eq!(
            press(&mut picker, Keysym::Return, false),
            KeyOutcome::Insert(plain_ch),
            "an emoji with no tones is unaffected"
        );
    }

    /// The popover is modal over the grid: a choice about one cell, so nothing behind it moves
    /// until it is answered or dismissed.
    #[test]
    fn the_tone_popover_holds_the_grid_still() {
        let mut picker = picker();
        picker.selected = first_toned(&picker);
        let selected = picker.selected;
        press(&mut picker, Keysym::Return, true);

        press(&mut picker, Keysym::Down, false);
        assert_eq!(picker.selected, selected, "the grid did not move");

        // `Handled`, not `Close`: Escape dismisses the popover and leaves the picker up.
        assert_eq!(
            press(&mut picker, Keysym::Escape, false),
            KeyOutcome::Handled
        );
        assert!(picker.tone.is_none());
    }

    #[test]
    fn an_empty_search_result_has_nothing_to_insert() {
        let mut picker = picker();
        picker.search.set_text("zzzzznothing".to_owned());
        picker.rebuild_view();
        assert!(picker.view.is_empty());

        assert_eq!(
            picker.handle_key(
                Some(Keysym::Return),
                None,
                EditMods::default(),
                KeyTheme::Default,
                true
            ),
            KeyOutcome::Handled,
            "Enter on nothing is not a close and not an insert"
        );
    }
}
