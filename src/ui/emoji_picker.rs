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

#[derive(Default)]
pub struct EmojiPicker {
    open: Option<Open>,
    search: TextEdit,
    /// Indices into [`emoji::table`]'s entries, in display order.
    view: Vec<usize>,
    /// Position in `view`; meaningless when `view` is empty.
    selected: usize,
    hovered: Option<usize>,
    /// First visible row of the grid.
    first_row: usize,
    revision: u64,
    cache: RefCell<ContentCache>,
    entry_cache: RefCell<widget::BakeCache>,
}

impl EmojiPicker {
    /// The panel's size, logical px. Fixed: a grid that resized as the search narrowed would move
    /// the cell under the pointer on every keystroke.
    pub const WIDTH: f64 = PAD * 2. + CELL * COLS as f64;
    pub const HEIGHT: f64 = PAD * 2. + widget::Entry::HEIGHT + GAP + CELL * ROWS as f64;

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
        self.selected = 0;
        self.first_row = 0;
        self.rebuild_view();
        self.revision += 1;
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

    /// Which cell the pointer is over, if any — what the render lights.
    pub fn hovered_index(&self) -> Option<usize> {
        self.hovered
    }

    pub fn contains(&self, pos: Point<f64, Logical>) -> bool {
        self.geometry().is_some_and(|geo| geo.contains(pos))
    }

    /// The emoji at a position in `view`.
    fn at(&self, index: usize) -> Option<&'static Emoji> {
        Some(&emoji::table().entries()[*self.view.get(index)?])
    }

    fn rows(&self) -> usize {
        self.view.len().div_ceil(COLS)
    }

    /// Rebuild the visible list from the search text.
    ///
    /// An empty search shows the whole table in Unicode order, which groups related emoji
    /// together. Nothing is filtered out: Unicode's `Component` group — the bare skin-tone
    /// swatches and hair components, which are modifiers rather than emoji anyone picks — carries
    /// the status `component` rather than `fully-qualified`, so `tools/emoji-table` never wrote
    /// them and the table has no such group at all.
    fn rebuild_view(&mut self) {
        let table = emoji::table();
        let query = self.search.text().trim();
        self.view = if query.is_empty() {
            (0..table.entries().len()).collect()
        } else {
            let hits = table.search(query);
            let base = table.entries().as_ptr();
            // `search` returns borrows into the same static table, so their offsets are indices.
            hits.into_iter()
                .map(|e| (e as *const Emoji as usize - base as usize) / size_of::<Emoji>())
                .collect()
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

        match raw {
            Some(Keysym::Escape) => return KeyOutcome::Close,
            Some(Keysym::Return | Keysym::KP_Enter | Keysym::ISO_Enter) => {
                return match self.at(self.selected) {
                    Some(emoji) => KeyOutcome::Insert(emoji.ch.to_owned()),
                    // Nothing matched the search; Enter has nothing to insert and is not a close.
                    None => KeyOutcome::Handled,
                };
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

    /// Track the pointer over the grid. Returns whether anything changed.
    pub fn pointer_motion(&mut self, pos: Point<f64, Logical>) -> bool {
        let hovered = self.cell_at(pos);
        if hovered == self.hovered {
            return false;
        }
        self.hovered = hovered;
        self.revision += 1;
        true
    }

    /// A click inside the picker. Returns the text to insert, if it landed on an emoji.
    pub fn pointer_click(&mut self, pos: Point<f64, Logical>) -> Option<String> {
        let index = self.cell_at(pos)?;
        self.selected = index;
        Some(self.at(index)?.ch.to_owned())
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
            widget::bake_content(
                renderer,
                &mut cache,
                scale,
                self.revision,
                |renderer| prepare_panel(renderer, scale, &cells),
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
    }

    /// The cells the grid currently shows, with their state — the render's whole input.
    fn visible_cells(&self) -> Vec<Cell> {
        let start = self.first_row * COLS;
        let end = (start + COLS * ROWS).min(self.view.len());
        (start..end)
            .map(|index| Cell {
                ch: emoji::table().entries()[self.view[index]].ch,
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
}

fn prepare_panel(
    renderer: &mut VulkanRenderer,
    scale: f64,
    cells: &[Cell],
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
        },
    ))
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
            widget::style::over(BG, [1., 1., 1., 0.18])
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
            let entry = &table.entries()[*index];
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
