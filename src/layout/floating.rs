// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

use std::cmp::max;
use std::iter::zip;
use std::rc::Rc;

use smithay::utils::{Logical, Point, Rectangle, Scale, Serial, Size};
use synoik_config::utils::MergeWith as _;
use synoik_config::{PresetSize, RelativeTo, WindowingMode};
use synoik_ipc::{PositionChange, SizeChange, WindowLayout};

use super::closing_window::{ClosingWindow, ClosingWindowRenderElement};
use super::scrolling::ColumnWidth;
use super::tile::{Tile, TileRenderElement, TileUnmapSnapshot};
use super::workspace::{InteractiveResize, ResolvedSize};
use super::{
    ConfigureIntent, InteractiveResizeData, LayoutElement, Options, RemovedTile, SizeFrac,
    SizingMode,
};
use crate::animation::{Animation, Clock};
use crate::gnome::TileSide;
use crate::render_helpers::xray::XrayPos;
use crate::render_helpers::RenderCtx;
use crate::synoik_render_elements;
use crate::utils::transaction::TransactionBlocker;
use crate::utils::{
    center_preferring_top_left_in_area, clamp_preferring_top_left_in_area, ensure_min_max_size,
    ensure_min_max_size_maybe_zero, ResizeEdge,
};
use crate::window::ResolvedWindowRules;

/// By how many logical pixels the directional move commands move floating windows.
pub const DIRECTIONAL_MOVE_PX: f64 = 50.;

/// Space for floating windows.
#[derive(Debug)]
pub struct FloatingSpace<W: LayoutElement> {
    /// Tiles in top-to-bottom order.
    tiles: Vec<Tile<W>>,

    /// Extra per-tile data.
    data: Vec<Data>,

    /// Id of the active window.
    ///
    /// The active window is not necessarily the topmost window. Focus-follows-mouse should
    /// activate a window, but not bring it to the top, because that's very annoying.
    ///
    /// This is always set to `Some()` when `tiles` isn't empty.
    active_window_id: Option<W::Id>,

    /// The window a running window cycler is showing, drawn above every other tile without
    /// touching the stacking order — `CyclerHighlight` (`altTab.js:410-472`) does the same thing
    /// by cloning the window into `window_group` and raising the clone, so Escape leaves the
    /// stack exactly as it found it.
    ///
    /// A clone is what GNOME needs and we do not: we own the render loop, so drawing the tile
    /// out of order is both cheaper and free of the double-composite a clone over a still-visible
    /// original would give a translucent window.
    cycler_raised: Option<W::Id>,

    /// Ongoing interactive resize.
    interactive_resize: Option<InteractiveResize<W>>,

    /// Windows in the closing animation.
    closing_windows: Vec<ClosingWindow>,

    /// View size for this space.
    view_size: Size<f64, Logical>,

    /// Working area for this space.
    working_area: Rectangle<f64, Logical>,

    /// Scale of the output the space is on (and rounds its sizes to).
    scale: f64,

    /// Clock for driving animations.
    clock: Clock,

    /// Configurable properties of the layout.
    options: Rc<Options>,
}

synoik_render_elements! {
    FloatingSpaceRenderElement => {
        Tile = TileRenderElement,
        ClosingWindow = ClosingWindowRenderElement,
    }
}

/// Where a tile's position comes from.
///
/// Maximized and fullscreen windows do not have a position of their own: theirs is derived from
/// the work area / output, and must not be clamped like a free-floating one (a fullscreen tile is
/// larger than the work area, so the off-screen clamp does not even apply to it). Anchoring is a
/// *view* on the position — `pos` keeps holding the free-floating one underneath, so unmaximizing
/// a window that never had a saved rect still lands where it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Anchor {
    /// Free-floating: the stored size-fraction position, clamped to stay mostly on-screen.
    Free,
    /// Pinned to the work area origin (maximized).
    WorkArea,
    /// Pinned to the output origin (fullscreen).
    Output,
}

/// The anchor a tile's pending sizing mode calls for.
///
/// Keyed off the *pending* mode rather than the tile's committed one so the tile travels together
/// with the resize it was just asked for, instead of jumping once the client catches up.
fn anchor_for<W: LayoutElement>(tile: &Tile<W>) -> Anchor {
    match tile.window().pending_sizing_mode() {
        SizingMode::Fullscreen => Anchor::Output,
        SizingMode::Maximized => Anchor::WorkArea,
        SizingMode::Normal => Anchor::Free,
    }
}

/// Extra per-tile data.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Data {
    /// Position relative to the working area.
    ///
    /// Retained while the tile is anchored, as the position to fall back to when it unanchors.
    pos: Point<f64, SizeFrac>,

    /// What `logical_pos` is derived from.
    anchor: Anchor,

    /// Cached position in logical coordinates.
    ///
    /// Not rounded to physical pixels.
    logical_pos: Point<f64, Logical>,

    /// Cached actual size of the tile.
    size: Size<f64, Logical>,

    /// Working area used for conversions.
    working_area: Rectangle<f64, Logical>,
}

impl Data {
    pub fn new<W: LayoutElement>(
        working_area: Rectangle<f64, Logical>,
        tile: &Tile<W>,
        logical_pos: Point<f64, Logical>,
    ) -> Self {
        let mut rv = Self {
            pos: Point::default(),
            anchor: Anchor::Free,
            logical_pos: Point::default(),
            size: Size::default(),
            working_area,
        };
        rv.update(tile);
        rv.set_logical_pos(logical_pos);
        rv.set_anchor(anchor_for(tile));
        rv
    }

    pub fn scale_by_working_area(
        area: Rectangle<f64, Logical>,
        pos: Point<f64, SizeFrac>,
    ) -> Point<f64, Logical> {
        let mut logical_pos = Point::from((pos.x, pos.y));
        logical_pos.x *= area.size.w;
        logical_pos.y *= area.size.h;
        logical_pos += area.loc;
        logical_pos
    }

    pub fn logical_to_size_frac_in_working_area(
        area: Rectangle<f64, Logical>,
        logical_pos: Point<f64, Logical>,
    ) -> Point<f64, SizeFrac> {
        let pos = logical_pos - area.loc;
        let mut pos = Point::from((pos.x, pos.y));
        pos.x /= f64::max(area.size.w, 1.0);
        pos.y /= f64::max(area.size.h, 1.0);
        pos
    }

    fn recompute_logical_pos(&mut self) {
        match self.anchor {
            Anchor::WorkArea => {
                self.logical_pos = self.working_area.loc;
                return;
            }
            Anchor::Output => {
                self.logical_pos = Point::from((0., 0.));
                return;
            }
            Anchor::Free => (),
        }

        let mut logical_pos = Self::scale_by_working_area(self.working_area, self.pos);

        // Make sure the window doesn't go too much off-screen. Numbers taken from Mutter.
        let min_on_screen_hor = f64::clamp(self.size.w / 4., 10., 75.);
        let min_on_screen_ver = f64::clamp(self.size.h / 4., 10., 75.);
        let max_off_screen_hor = f64::max(0., self.size.w - min_on_screen_hor);
        let max_off_screen_ver = f64::max(0., self.size.h - min_on_screen_ver);

        logical_pos -= self.working_area.loc;
        logical_pos.x = f64::max(logical_pos.x, -max_off_screen_hor);
        logical_pos.y = f64::max(logical_pos.y, -max_off_screen_ver);
        logical_pos.x = f64::min(
            logical_pos.x,
            self.working_area.size.w - self.size.w + max_off_screen_hor,
        );
        logical_pos.y = f64::min(
            logical_pos.y,
            self.working_area.size.h - self.size.h + max_off_screen_ver,
        );
        logical_pos += self.working_area.loc;

        self.logical_pos = logical_pos;
    }

    pub fn update_config(&mut self, working_area: Rectangle<f64, Logical>) {
        if self.working_area == working_area {
            return;
        }

        self.working_area = working_area;
        self.recompute_logical_pos();
    }

    pub fn update<W: LayoutElement>(&mut self, tile: &Tile<W>) {
        let size = tile.tile_size();
        if self.size == size {
            return;
        }

        self.size = size;
        self.recompute_logical_pos();
    }

    pub fn set_logical_pos(&mut self, logical_pos: Point<f64, Logical>) {
        self.pos = Self::logical_to_size_frac_in_working_area(self.working_area, logical_pos);

        // This will clamp the logical position to the current working area.
        self.recompute_logical_pos();
    }

    pub fn set_anchor(&mut self, anchor: Anchor) {
        if self.anchor == anchor {
            return;
        }

        self.anchor = anchor;
        self.recompute_logical_pos();
    }

    pub fn center(&self) -> Point<f64, Logical> {
        self.logical_pos + self.size.downscale(2.)
    }

    #[cfg(test)]
    fn verify_invariants(&self) {
        let mut temp = *self;
        temp.recompute_logical_pos();
        assert_eq!(
            self.logical_pos, temp.logical_pos,
            "cached logical pos must be up to date"
        );
    }
}

impl<W: LayoutElement> FloatingSpace<W> {
    pub fn new(
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
        scale: f64,
        clock: Clock,
        options: Rc<Options>,
    ) -> Self {
        Self {
            tiles: Vec::new(),
            data: Vec::new(),
            active_window_id: None,
            cycler_raised: None,
            interactive_resize: None,
            closing_windows: Vec::new(),
            view_size,
            working_area,
            scale,
            clock,
            options,
        }
    }

    pub fn update_config(
        &mut self,
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
        scale: f64,
        options: Rc<Options>,
    ) {
        for (tile, data) in zip(&mut self.tiles, &mut self.data) {
            tile.update_config(view_size, scale, options.clone());
            data.update(tile);
            data.update_config(working_area);
        }

        self.view_size = view_size;
        self.working_area = working_area;
        self.scale = scale;
        self.options = options;
    }

    pub fn update_shaders(&mut self) {
        for tile in &mut self.tiles {
            tile.update_shaders();
        }
    }

    pub fn advance_animations(&mut self) {
        for tile in &mut self.tiles {
            tile.advance_animations();
        }

        self.closing_windows.retain_mut(|closing| {
            closing.advance_animations();
            closing.are_animations_ongoing()
        });
    }

    pub fn are_animations_ongoing(&self) -> bool {
        self.tiles.iter().any(Tile::are_animations_ongoing) || !self.closing_windows.is_empty()
    }

    pub fn are_transitions_ongoing(&self) -> bool {
        self.tiles.iter().any(Tile::are_transitions_ongoing) || !self.closing_windows.is_empty()
    }

    pub fn update_render_elements(&mut self, is_active: bool, view_rect: Rectangle<f64, Logical>) {
        let active = self.active_window_id.clone();
        for (tile, offset) in self.tiles_with_offsets_mut() {
            let id = tile.window().id();
            let is_active = is_active && Some(id) == active.as_ref();

            let mut tile_view_rect = view_rect;
            tile_view_rect.loc -= offset + tile.render_offset();
            tile.update_render_elements(is_active, tile_view_rect);
        }
    }

    pub fn tiles(&self) -> impl Iterator<Item = &Tile<W>> + '_ {
        self.tiles.iter()
    }

    pub fn tiles_mut(&mut self) -> impl Iterator<Item = &mut Tile<W>> + '_ {
        self.tiles.iter_mut()
    }

    pub fn tiles_with_offsets(&self) -> impl Iterator<Item = (&Tile<W>, Point<f64, Logical>)> + '_ {
        let offsets = self.data.iter().map(|d| d.logical_pos);
        zip(&self.tiles, offsets)
    }

    pub fn tiles_with_offsets_mut(
        &mut self,
    ) -> impl Iterator<Item = (&mut Tile<W>, Point<f64, Logical>)> + '_ {
        let offsets = self.data.iter().map(|d| d.logical_pos);
        zip(&mut self.tiles, offsets)
    }

    pub fn tiles_with_render_positions(
        &self,
    ) -> impl Iterator<Item = (&Tile<W>, Point<f64, Logical>)> {
        let scale = self.scale;
        self.tiles_with_offsets().map(move |(tile, offset)| {
            let pos = offset + tile.render_offset();
            // Round to physical pixels.
            let pos = pos.to_physical_precise_round(scale).to_logical(scale);
            (tile, pos)
        })
    }

    pub fn tiles_with_render_positions_mut(
        &mut self,
        round: bool,
    ) -> impl Iterator<Item = (&mut Tile<W>, Point<f64, Logical>)> {
        let scale = self.scale;
        self.tiles_with_offsets_mut().map(move |(tile, offset)| {
            let mut pos = offset + tile.render_offset();
            // Round to physical pixels.
            if round {
                pos = pos.to_physical_precise_round(scale).to_logical(scale);
            }
            (tile, pos)
        })
    }

    pub fn tiles_with_ipc_layouts(&self) -> impl Iterator<Item = (&Tile<W>, WindowLayout)> {
        let scale = self.scale;
        self.tiles_with_offsets().map(move |(tile, offset)| {
            // Do not include animated render offset here to avoid IPC spam.
            let pos = offset;
            // Round to physical pixels.
            let pos = pos.to_physical_precise_round(scale).to_logical(scale);

            let layout = WindowLayout {
                tile_pos_in_workspace_view: Some(pos.into()),
                ..tile.ipc_layout_template()
            };
            (tile, layout)
        })
    }

    pub fn new_window_toplevel_bounds(&self, rules: &ResolvedWindowRules) -> Size<i32, Logical> {
        let border_config = self.options.layout.border.merged_with(&rules.border);
        compute_toplevel_bounds(border_config, self.working_area.size)
    }

    /// Returns the geometry of the active window relative to and clamped to the working area.
    ///
    /// During animations, assumes the final tile position.
    pub fn active_window_visual_rectangle(&self) -> Option<Rectangle<f64, Logical>> {
        let (tile, offset) = self.tiles_with_offsets().next()?;

        let window_pos = offset + tile.window_loc();
        let window_size = tile.window_size();
        let window_rect = Rectangle::new(window_pos, window_size);

        self.working_area.intersection(window_rect)
    }

    pub fn popup_target_rect(&self, id: &W::Id) -> Option<Rectangle<f64, Logical>> {
        for (tile, pos) in self.tiles_with_offsets() {
            if tile.window().id() == id {
                // Position within the working area.
                let mut target = self.working_area;
                target.loc -= pos;
                target.loc -= tile.window_loc();

                return Some(target);
            }
        }
        None
    }

    fn idx_of(&self, id: &W::Id) -> Option<usize> {
        self.tiles.iter().position(|tile| tile.window().id() == id)
    }

    fn contains(&self, id: &W::Id) -> bool {
        self.idx_of(id).is_some()
    }

    pub fn active_window(&self) -> Option<&W> {
        let id = self.active_window_id.as_ref()?;
        self.tiles
            .iter()
            .find(|tile| tile.window().id() == id)
            .map(Tile::window)
    }

    pub fn active_window_mut(&mut self) -> Option<&mut W> {
        let id = self.active_window_id.as_ref()?;
        self.tiles
            .iter_mut()
            .find(|tile| tile.window().id() == id)
            .map(Tile::window_mut)
    }

    pub fn has_window(&self, id: &W::Id) -> bool {
        self.tiles.iter().any(|tile| tile.window().id() == id)
    }

    pub fn is_empty(&self) -> bool {
        self.tiles.is_empty()
    }

    pub fn add_tile(&mut self, tile: Tile<W>, activate: bool) {
        let mut idx = 0;

        // GNOME windowing: a window that opens without taking focus must not
        // cover the focused window either — stack it just below (mutter's
        // meta_window_stack_just_below in meta_window_show).
        if !activate && self.options.layout.windowing_mode == WindowingMode::Floating {
            if let Some(active_idx) = self
                .active_window_id
                .as_ref()
                .and_then(|id| self.idx_of(id))
            {
                idx = active_idx + 1;
            }
        }

        self.add_tile_at(idx, tile, activate);
    }

    fn add_tile_at(&mut self, mut idx: usize, mut tile: Tile<W>, activate: bool) {
        tile.update_config(self.view_size, self.scale, self.options.clone());

        let gnome_mode = self.options.layout.windowing_mode == WindowingMode::Floating;
        match tile.window().pending_sizing_mode() {
            // GNOME mode: this space holds maximized and fullscreen windows itself. The state
            // survives, but is re-applied against *this* space — the tile may be arriving from a
            // workspace on another output, or from one with different struts.
            SizingMode::Fullscreen if gnome_mode => tile.request_fullscreen(true, None),
            SizingMode::Maximized if gnome_mode => {
                let size = self.working_area.size;
                tile.request_maximized(size, true, None);
            }
            // In niri's scrolling mode only the scrolling layout can size a window to the screen,
            // so a window arriving here leaves those states behind. Restore the previous floating
            // window size, and in case the tile is fullscreen, unfullscreen it.
            _ => {
                let floating_size = tile.floating_window_size;
                let win = tile.window_mut();
                // A remembered floating size is ours to restore either way. Without one, the two
                // modes want opposite things.
                //
                // In niri's scrolling mode a window can arrive here straight out of the tiling
                // layout, which owned its size; nothing else would pin it, so ask for the size it
                // currently has.
                //
                // In GNOME mode it cannot — this space owns maximize and fullscreen, so a window
                // with no remembered size is one that has just mapped, and its size is its own.
                // Ask for (0, 0), "you choose". GNOME never sizes a window from its own geometry:
                // a client-driven geometry change updates min/max size and recalculates features,
                // nothing more (mutter `meta-wayland-xdg-shell.c:1081-1103`). Handing it back
                // freezes whatever the geometry happened to be at this instant, and on a map that
                // is *before* a toolkit has drawn its decorations, so the window loses them and
                // shrinks — see `a_window_is_not_configured_smaller_than_it_asked_for`.
                let mut size = floating_size.unwrap_or_else(|| {
                    if gnome_mode {
                        Size::default()
                    } else {
                        win.expected_size().unwrap_or_default()
                    }
                });

                // Apply min/max size window rules. If requesting a concrete size, apply
                // completely; if requesting (0, 0), apply only when min/max results in a fixed
                // size.
                let min_size = win.min_size();
                let max_size = win.max_size();
                size.w = ensure_min_max_size_maybe_zero(size.w, min_size.w, max_size.w);
                size.h = ensure_min_max_size_maybe_zero(size.h, min_size.h, max_size.h);

                win.request_size_once(size, true);
            }
        }

        let win = tile.window_mut();
        if activate || self.tiles.is_empty() {
            self.active_window_id = Some(win.id().clone());
        }

        // Make sure the tile isn't inserted below its parent.
        for (i, tile_above) in self.tiles.iter().enumerate().take(idx) {
            if win.is_child_of(tile_above.window()) {
                idx = i;
                break;
            }
        }

        let pos = self.stored_or_default_tile_pos(&tile).unwrap_or_else(|| {
            if self.options.layout.windowing_mode == WindowingMode::Floating {
                self.place_new_tile(&tile)
            } else {
                center_preferring_top_left_in_area(self.working_area, tile.tile_size())
            }
        });

        let data = Data::new(self.working_area, &tile, pos);
        self.data.insert(idx, data);
        self.tiles.insert(idx, tile);

        self.bring_up_descendants_of(idx);
    }

    pub fn add_tile_above(&mut self, above: &W::Id, mut tile: Tile<W>, activate: bool) {
        let idx = self.idx_of(above).unwrap();

        let above_pos = self.data[idx].logical_pos;
        let above_size = self.data[idx].size;
        let tile_size = tile.tile_size();
        let pos = if self.options.layout.windowing_mode == WindowingMode::Floating {
            // mutter's transient placement: centered horizontally, vertically
            // at the top-biased third (place.c meta_window_place).
            above_pos
                + Point::from((
                    (above_size.w - tile_size.w) / 2.,
                    (above_size.h - tile_size.h) / 3.,
                ))
        } else {
            above_pos + (above_size.to_point() - tile_size.to_point()).downscale(2.)
        };
        let pos = self.clamp_within_working_area(pos, tile_size);
        tile.floating_pos = Some(self.logical_to_size_frac(pos));

        self.add_tile_at(idx, tile, activate);
    }

    fn bring_up_descendants_of(&mut self, idx: usize) {
        let tile = &self.tiles[idx];
        let win = tile.window();

        // We always maintain the correct stacking order, so walking descendants back to front
        // should give us all of them.
        let mut descendants: Vec<usize> = Vec::new();
        for (i, tile_below) in self.tiles.iter().enumerate().skip(idx + 1).rev() {
            let win_below = tile_below.window();
            if win_below.is_child_of(win)
                || descendants
                    .iter()
                    .any(|idx| win_below.is_child_of(self.tiles[*idx].window()))
            {
                descendants.push(i);
            }
        }

        // Now, descendants is in back-to-front order, and repositioning them in the front-to-back
        // order will preserve the subsequent indices and work out right.
        let mut idx = idx;
        #[allow(clippy::explicit_counter_loop)]
        for descendant_idx in descendants.into_iter().rev() {
            self.raise_window(descendant_idx, idx);
            idx += 1;
        }
    }

    pub fn remove_active_tile(&mut self) -> Option<RemovedTile<W>> {
        let id = self.active_window_id.clone()?;
        Some(self.remove_tile(&id))
    }

    pub fn remove_tile(&mut self, id: &W::Id) -> RemovedTile<W> {
        let idx = self.idx_of(id).unwrap();
        self.remove_tile_by_idx(idx)
    }

    fn remove_tile_by_idx(&mut self, idx: usize) -> RemovedTile<W> {
        let mut tile = self.tiles.remove(idx);
        let data = self.data.remove(idx);

        if self.tiles.is_empty() {
            self.active_window_id = None;
        } else if Some(tile.window().id()) == self.active_window_id.as_ref() {
            // The active tile was removed, make the topmost tile active.
            self.active_window_id = Some(self.tiles[0].window().id().clone());
        }

        // Stop interactive resize.
        if let Some(resize) = &self.interactive_resize {
            if tile.window().id() == &resize.window {
                self.interactive_resize = None;
            }
        }

        // Store the floating size if we have one.
        if let Some(size) = tile.window().expected_size() {
            tile.floating_window_size = Some(size);
        }
        // Store the floating position.
        tile.floating_pos = Some(data.pos);

        let width = ColumnWidth::Fixed(tile.tile_expected_or_current_size().w);
        RemovedTile {
            tile,
            width,
            is_full_width: false,
            is_floating: true,
        }
    }

    pub fn start_close_animation_for_window(&mut self, id: &W::Id, blocker: TransactionBlocker) {
        let (tile, tile_pos) = self
            .tiles_with_render_positions_mut(false)
            .find(|(tile, _)| tile.window().id() == id)
            .unwrap();

        let Some(snapshot) = tile.take_unmap_snapshot() else {
            return;
        };

        let tile_size = tile.tile_size();

        self.start_close_animation_for_tile(snapshot, tile_size, tile_pos, blocker);
    }

    pub fn activate_window_without_raising(&mut self, id: &W::Id) -> bool {
        if !self.contains(id) {
            return false;
        }

        self.active_window_id = Some(id.clone());
        true
    }

    pub fn activate_window(&mut self, id: &W::Id) -> bool {
        let Some(idx) = self.idx_of(id) else {
            return false;
        };

        self.raise_window(idx, 0);
        self.active_window_id = Some(id.clone());
        self.bring_up_descendants_of(0);

        true
    }

    /// Show `id` above every other tile for as long as a cycler is up. See
    /// [`cycler_raised`](Self::cycler_raised).
    pub fn set_cycler_raised(&mut self, id: Option<&W::Id>) {
        self.cycler_raised = id.cloned();
    }

    fn raise_window(&mut self, from_idx: usize, to_idx: usize) {
        assert!(to_idx <= from_idx);

        let tile = self.tiles.remove(from_idx);
        let data = self.data.remove(from_idx);
        self.tiles.insert(to_idx, tile);
        self.data.insert(to_idx, data);
    }

    pub fn start_close_animation_for_tile(
        &mut self,
        snapshot: TileUnmapSnapshot,
        tile_size: Size<f64, Logical>,
        tile_pos: Point<f64, Logical>,
        blocker: TransactionBlocker,
    ) {
        let anim = Animation::new(
            self.clock.clone(),
            0.,
            1.,
            0.,
            self.options.animations.window_close.anim,
        );

        let blocker = if self.options.disable_transactions {
            TransactionBlocker::completed()
        } else {
            blocker
        };

        self.closing_windows.push(ClosingWindow::new(
            snapshot, tile_size, tile_pos, blocker, anim,
        ));
    }

    pub fn toggle_window_width(&mut self, id: Option<&W::Id>, forwards: bool) {
        let Some(id) = id.or(self.active_window_id.as_ref()).cloned() else {
            return;
        };
        let idx = self.idx_of(&id).unwrap();

        let available_size = self.working_area.size.w;

        let len = self.options.layout.preset_column_widths.len();
        let tile = &mut self.tiles[idx];
        let preset_idx = if let Some(idx) = tile.floating_preset_width_idx {
            (idx + if forwards { 1 } else { len - 1 }) % len
        } else {
            let current_window = tile.window_expected_or_current_size().w;
            let current_tile = tile.tile_expected_or_current_size().w;

            let mut it = self
                .options
                .layout
                .preset_column_widths
                .iter()
                .map(|preset| resolve_preset_size(*preset, available_size));

            if forwards {
                it.position(|resolved| {
                    match resolved {
                        // Some allowance for fractional scaling purposes.
                        ResolvedSize::Tile(resolved) => current_tile + 1. < resolved,
                        ResolvedSize::Window(resolved) => current_window + 1. < resolved,
                    }
                })
                .unwrap_or(0)
            } else {
                it.rposition(|resolved| {
                    match resolved {
                        // Some allowance for fractional scaling purposes.
                        ResolvedSize::Tile(resolved) => resolved + 1. < current_tile,
                        ResolvedSize::Window(resolved) => resolved + 1. < current_window,
                    }
                })
                .unwrap_or(len - 1)
            }
        };

        let preset = self.options.layout.preset_column_widths[preset_idx];
        self.set_window_width(Some(&id), SizeChange::from(preset), true);

        self.tiles[idx].floating_preset_width_idx = Some(preset_idx);

        self.interactive_resize_end(Some(&id));
    }

    pub fn start_open_animation(&mut self, id: &W::Id) -> bool {
        let Some(idx) = self.idx_of(id) else {
            return false;
        };

        self.tiles[idx].start_open_animation();
        true
    }

    pub fn toggle_window_height(&mut self, id: Option<&W::Id>, forwards: bool) {
        let Some(id) = id.or(self.active_window_id.as_ref()).cloned() else {
            return;
        };
        let idx = self.idx_of(&id).unwrap();

        let available_size = self.working_area.size.h;

        let len = self.options.layout.preset_window_heights.len();
        let tile = &mut self.tiles[idx];
        let preset_idx = if let Some(idx) = tile.floating_preset_height_idx {
            (idx + if forwards { 1 } else { len - 1 }) % len
        } else {
            let current_window = tile.window_expected_or_current_size().h;
            let current_tile = tile.tile_expected_or_current_size().h;

            let mut it = self
                .options
                .layout
                .preset_window_heights
                .iter()
                .map(|preset| resolve_preset_size(*preset, available_size));

            if forwards {
                it.position(|resolved| {
                    match resolved {
                        // Some allowance for fractional scaling purposes.
                        ResolvedSize::Tile(resolved) => current_tile + 1. < resolved,
                        ResolvedSize::Window(resolved) => current_window + 1. < resolved,
                    }
                })
                .unwrap_or(0)
            } else {
                it.rposition(|resolved| {
                    match resolved {
                        // Some allowance for fractional scaling purposes.
                        ResolvedSize::Tile(resolved) => resolved + 1. < current_tile,
                        ResolvedSize::Window(resolved) => resolved + 1. < current_window,
                    }
                })
                .unwrap_or(len - 1)
            }
        };

        let preset = self.options.layout.preset_window_heights[preset_idx];
        self.set_window_height(Some(&id), SizeChange::from(preset), true);

        let tile = &mut self.tiles[idx];
        tile.floating_preset_height_idx = Some(preset_idx);

        self.interactive_resize_end(Some(&id));
    }

    pub fn set_window_width(&mut self, id: Option<&W::Id>, change: SizeChange, animate: bool) {
        let Some(id) = id.or(self.active_window_id.as_ref()) else {
            return;
        };
        let idx = self.idx_of(id).unwrap();

        let tile = &mut self.tiles[idx];
        tile.floating_preset_width_idx = None;

        let available_size = self.working_area.size.w;
        let win = tile.window();
        let current_window = win.expected_size().unwrap_or_else(|| win.size()).w;
        let current_tile = tile.tile_expected_or_current_size().w;

        const MAX_PX: f64 = 100000.;
        const MAX_F: f64 = 10000.;

        let win_width = match change {
            SizeChange::SetFixed(win_width) => f64::from(win_width),
            SizeChange::SetProportion(prop) => {
                let prop = (prop / 100.).clamp(0., MAX_F);
                let tile_width = available_size * prop;
                tile.window_width_for_tile_width(tile_width)
            }
            SizeChange::AdjustFixed(delta) => f64::from(current_window.saturating_add(delta)),
            SizeChange::AdjustProportion(delta) => {
                let current_prop = current_tile / available_size;
                let prop = (current_prop + delta / 100.).clamp(0., MAX_F);
                let tile_width = available_size * prop;
                tile.window_width_for_tile_width(tile_width)
            }
        };
        let win_width = win_width.round().clamp(1., MAX_PX) as i32;

        let win = tile.window_mut();
        let min_size = win.min_size();
        let max_size = win.max_size();

        let win_width = ensure_min_max_size(win_width, min_size.w, max_size.w);

        let win_height = win.expected_size().unwrap_or_default().h;
        let win_height = ensure_min_max_size(win_height, min_size.h, max_size.h);

        let win_size = Size::from((win_width, win_height));
        win.request_size_once(win_size, animate);
    }

    pub fn set_window_height(&mut self, id: Option<&W::Id>, change: SizeChange, animate: bool) {
        let Some(id) = id.or(self.active_window_id.as_ref()) else {
            return;
        };
        let idx = self.idx_of(id).unwrap();

        let tile = &mut self.tiles[idx];
        tile.floating_preset_height_idx = None;

        let available_size = self.working_area.size.h;
        let win = tile.window();
        let current_window = win.expected_size().unwrap_or_else(|| win.size()).h;
        let current_tile = tile.tile_expected_or_current_size().h;

        const MAX_PX: f64 = 100000.;
        const MAX_F: f64 = 10000.;

        let win_height = match change {
            SizeChange::SetFixed(win_height) => f64::from(win_height),
            SizeChange::SetProportion(prop) => {
                let prop = (prop / 100.).clamp(0., MAX_F);
                let tile_height = available_size * prop;
                tile.window_height_for_tile_height(tile_height)
            }
            SizeChange::AdjustFixed(delta) => f64::from(current_window.saturating_add(delta)),
            SizeChange::AdjustProportion(delta) => {
                let current_prop = current_tile / available_size;
                let prop = (current_prop + delta / 100.).clamp(0., MAX_F);
                let tile_height = available_size * prop;
                tile.window_height_for_tile_height(tile_height)
            }
        };
        let win_height = win_height.round().clamp(1., MAX_PX) as i32;

        let win = tile.window_mut();
        let min_size = win.min_size();
        let max_size = win.max_size();

        let win_height = ensure_min_max_size(win_height, min_size.h, max_size.h);

        let win_width = win.expected_size().unwrap_or_default().w;
        let win_width = ensure_min_max_size(win_width, min_size.w, max_size.w);

        let win_size = Size::from((win_width, win_height));
        win.request_size_once(win_size, animate);
    }

    fn focus_directional(
        &mut self,
        distance: impl Fn(Point<f64, Logical>, Point<f64, Logical>) -> f64,
    ) -> bool {
        let Some(active_id) = &self.active_window_id else {
            return false;
        };
        let active_idx = self.idx_of(active_id).unwrap();
        let center = self.data[active_idx].center();

        let result = zip(&self.tiles, &self.data)
            .filter(|(tile, _)| tile.window().id() != active_id)
            .map(|(tile, data)| (tile, distance(center, data.center())))
            .filter(|(_, dist)| *dist > 0.)
            .min_by(|(_, dist_a), (_, dist_b)| f64::total_cmp(dist_a, dist_b));
        if let Some((tile, _)) = result {
            let id = tile.window().id().clone();
            self.activate_window(&id);
            true
        } else {
            false
        }
    }

    pub fn focus_left(&mut self) -> bool {
        self.focus_directional(|focus, other| focus.x - other.x)
    }

    pub fn focus_right(&mut self) -> bool {
        self.focus_directional(|focus, other| other.x - focus.x)
    }

    pub fn focus_up(&mut self) -> bool {
        self.focus_directional(|focus, other| focus.y - other.y)
    }

    pub fn focus_down(&mut self) -> bool {
        self.focus_directional(|focus, other| other.y - focus.y)
    }

    pub fn focus_leftmost(&mut self) {
        let result = self
            .tiles_with_offsets()
            .min_by(|(_, pos_a), (_, pos_b)| f64::total_cmp(&pos_a.x, &pos_b.x));
        if let Some((tile, _)) = result {
            let id = tile.window().id().clone();
            self.activate_window(&id);
        }
    }

    pub fn focus_rightmost(&mut self) {
        let result = self
            .tiles_with_offsets()
            .max_by(|(_, pos_a), (_, pos_b)| f64::total_cmp(&pos_a.x, &pos_b.x));
        if let Some((tile, _)) = result {
            let id = tile.window().id().clone();
            self.activate_window(&id);
        }
    }

    pub fn focus_topmost(&mut self) {
        let result = self
            .tiles_with_offsets()
            .min_by(|(_, pos_a), (_, pos_b)| f64::total_cmp(&pos_a.y, &pos_b.y));
        if let Some((tile, _)) = result {
            let id = tile.window().id().clone();
            self.activate_window(&id);
        }
    }

    pub fn focus_bottommost(&mut self) {
        let result = self
            .tiles_with_offsets()
            .max_by(|(_, pos_a), (_, pos_b)| f64::total_cmp(&pos_a.y, &pos_b.y));
        if let Some((tile, _)) = result {
            let id = tile.window().id().clone();
            self.activate_window(&id);
        }
    }

    fn move_to(&mut self, idx: usize, new_pos: Point<f64, Logical>, animate: bool) {
        if animate {
            self.move_and_animate(idx, new_pos);
        } else {
            self.data[idx].set_logical_pos(new_pos);
        }

        self.interactive_resize_end(None);
    }

    fn move_by(&mut self, amount: Point<f64, Logical>) {
        let Some(active_id) = &self.active_window_id else {
            return;
        };
        let idx = self.idx_of(active_id).unwrap();

        let new_pos = self.data[idx].logical_pos + amount;
        self.move_to(idx, new_pos, true)
    }

    pub fn move_left(&mut self) {
        self.move_by(Point::from((-DIRECTIONAL_MOVE_PX, 0.)));
    }

    pub fn move_right(&mut self) {
        self.move_by(Point::from((DIRECTIONAL_MOVE_PX, 0.)));
    }

    pub fn move_up(&mut self) {
        self.move_by(Point::from((0., -DIRECTIONAL_MOVE_PX)));
    }

    pub fn move_down(&mut self) {
        self.move_by(Point::from((0., DIRECTIONAL_MOVE_PX)));
    }

    pub fn move_window(
        &mut self,
        id: Option<&W::Id>,
        x: PositionChange,
        y: PositionChange,
        animate: bool,
    ) {
        let Some(id) = id.or(self.active_window_id.as_ref()) else {
            return;
        };
        let idx = self.idx_of(id).unwrap();

        let mut pos = self.data[idx].logical_pos;

        let available_width = self.working_area.size.w;
        let available_height = self.working_area.size.h;
        let working_area_loc = self.working_area.loc;

        const MAX_F: f64 = 10000.;

        match x {
            PositionChange::SetFixed(x) => pos.x = x + working_area_loc.x,
            PositionChange::SetProportion(prop) => {
                let prop = (prop / 100.).clamp(0., MAX_F);
                pos.x = available_width * prop + working_area_loc.x;
            }
            PositionChange::AdjustFixed(x) => pos.x += x,
            PositionChange::AdjustProportion(prop) => {
                let current_prop = (pos.x - working_area_loc.x) / available_width.max(1.);
                let prop = (current_prop + prop / 100.).clamp(0., MAX_F);
                pos.x = available_width * prop + working_area_loc.x;
            }
        }
        match y {
            PositionChange::SetFixed(y) => pos.y = y + working_area_loc.y,
            PositionChange::SetProportion(prop) => {
                let prop = (prop / 100.).clamp(0., MAX_F);
                pos.y = available_height * prop + working_area_loc.y;
            }
            PositionChange::AdjustFixed(y) => pos.y += y,
            PositionChange::AdjustProportion(prop) => {
                let current_prop = (pos.y - working_area_loc.y) / available_height.max(1.);
                let prop = (current_prop + prop / 100.).clamp(0., MAX_F);
                pos.y = available_height * prop + working_area_loc.y;
            }
        }

        self.move_to(idx, pos, animate);
    }

    /// Tiles the window to the given half of the work area, or untiles it if
    /// already tiled there (GNOME Super+Left/Right; mutter's
    /// `handle_toggle_tiled`).
    pub fn toggle_tiled(&mut self, id: Option<&W::Id>, side: TileSide) {
        let Some(id) = id.or(self.active_window_id.as_ref()).cloned() else {
            return;
        };
        let Some(idx) = self.idx_of(&id) else {
            return;
        };

        if self.tiles[idx].window().edge_tiled_side() == Some(side) {
            self.untile(idx);
        } else {
            self.tile_to(idx, side);
        }
    }

    /// Untiles the window if it is edge-tiled, restoring its saved geometry
    /// (mutter's `meta_window_untile`).
    pub fn untile_window(&mut self, id: &W::Id) {
        let Some(idx) = self.idx_of(id) else {
            return;
        };
        if self.tiles[idx].window().edge_tiled_side().is_some() {
            self.untile(idx);
        }
    }

    /// Clears the edge-tile state without issuing a resize and returns the
    /// saved pre-tile geometry. For handing the window to a state that owns
    /// the next configure (e.g. maximize), so the eventual restore lands on
    /// the pre-tile rect like mutter's `saved_rect`.
    #[allow(clippy::type_complexity)]
    pub fn take_tile_restore(
        &mut self,
        id: &W::Id,
    ) -> Option<(Option<Size<i32, Logical>>, Option<Point<f64, SizeFrac>>)> {
        let idx = self.idx_of(id)?;
        let tile = &mut self.tiles[idx];
        tile.window().edge_tiled_side()?;
        tile.window_mut().set_edge_tiled(None);
        Some((
            tile.tiled_restore_size.take(),
            tile.tiled_restore_pos.take(),
        ))
    }

    /// mutter's tile geometry (`meta_window_get_tile_area` with the default
    /// 0.5 fraction): half the work area wide, full height, snapped to the
    /// side's edge.
    fn tile_to(&mut self, idx: usize, side: TileSide) {
        let area = self.working_area;

        // First tile from floating: save the restore rect (mutter's `meta_window_save_rect`).
        // Re-tiling to the other side keeps it, and so does tiling a maximized or fullscreen
        // window, which already saved one on its way there (mutter clears the maximization and
        // tiles from the pre-maximize rect).
        if self.tiles[idx].window().pending_sizing_mode().is_normal() {
            self.save_restore_rect(idx);
        }

        let tile = &mut self.tiles[idx];
        tile.saved_maximize = false;

        let tile_width = (area.size.w / 2.).round();
        let win_width = tile.window_width_for_tile_width(tile_width);
        let win_height = tile.window_height_for_tile_height(area.size.h);

        let win = tile.window_mut();
        let min_size = win.min_size();
        let max_size = win.max_size();
        let win_width = ensure_min_max_size(win_width.round() as i32, min_size.w, max_size.w);
        let win_height = ensure_min_max_size(win_height.round() as i32, min_size.h, max_size.h);
        win.request_size_once(Size::from((win_width, win_height)), true);
        win.set_edge_tiled(Some(side));

        // An edge-tiled window is positioned like a free-floating one, so drop any maximize or
        // fullscreen anchor before moving it.
        let x = match side {
            TileSide::Left => area.loc.x,
            TileSide::Right => area.loc.x + area.size.w - tile_width,
        };
        let prev_pos = self.data[idx].logical_pos;
        self.data[idx].set_logical_pos(Point::from((x, area.loc.y)));
        self.data[idx].set_anchor(Anchor::Free);
        self.animate_move_from(idx, prev_pos);
        self.interactive_resize_end(None);
    }

    fn untile(&mut self, idx: usize) {
        self.restore_normal(idx);
    }

    /// Maximizes or unmaximizes the window (mutter's `meta_window_maximize` /
    /// `meta_window_unmaximize`). Returns whether anything changed.
    pub fn set_maximized(&mut self, id: &W::Id, maximize: bool) -> bool {
        let Some(idx) = self.idx_of(id) else {
            return false;
        };

        let mode = self.tiles[idx].window().pending_sizing_mode();

        // Under fullscreen, maximizing only records where unfullscreening should land (mutter's
        // `saved_maximize`); the window itself stays fullscreen.
        if mode.is_fullscreen() {
            let changed = self.tiles[idx].saved_maximize != maximize;
            self.tiles[idx].saved_maximize = maximize;
            return changed;
        }

        if mode.is_maximized() == maximize {
            // An edge-tiled window counts as maximized for mutter's handle_unmaximize, which
            // untiles it.
            if !maximize && self.tiles[idx].window().edge_tiled_side().is_some() {
                self.untile(idx);
                return true;
            }
            return false;
        }

        if maximize {
            self.save_restore_rect(idx);
            self.apply_maximized(idx);
        } else {
            self.restore_normal(idx);
        }

        true
    }

    /// Whether the active window is fullscreen and should therefore cover the top layer (the
    /// panel). Keyed off the tile's *committed* sizing mode, like the scrolling layer's version:
    /// hiding the panel before the client has actually resized would flash the desktop.
    pub fn render_above_top_layer(&self) -> bool {
        let Some(idx) = self
            .active_window_id
            .as_ref()
            .and_then(|id| self.idx_of(id))
        else {
            return false;
        };

        self.tiles[idx].sizing_mode().is_fullscreen()
    }

    /// Whether the window is maximized, counting the maximization held underneath a fullscreen
    /// (mutter's `saved_maximize`) — the same bit a scrolling `Column` tracks separately from its
    /// fullscreen state.
    pub fn is_maximized(&self, id: &W::Id) -> bool {
        let Some(idx) = self.idx_of(id) else {
            return false;
        };

        let tile = &self.tiles[idx];
        let mode = tile.window().pending_sizing_mode();
        if mode.is_fullscreen() {
            tile.saved_maximize
        } else {
            mode.is_maximized()
        }
    }

    /// Fullscreens or unfullscreens the window. Returns whether anything changed.
    pub fn set_fullscreen(&mut self, id: &W::Id, is_fullscreen: bool) -> bool {
        let Some(idx) = self.idx_of(id) else {
            return false;
        };

        let mode = self.tiles[idx].window().pending_sizing_mode();
        if mode.is_fullscreen() == is_fullscreen {
            return false;
        }

        if is_fullscreen {
            if mode.is_maximized() {
                // Come back to maximized rather than to the saved rect, which stays as it was
                // saved on the way into maximized.
                self.tiles[idx].saved_maximize = true;
            } else {
                self.save_restore_rect(idx);
            }

            let prev_pos = self.data[idx].logical_pos;
            self.tiles[idx].request_fullscreen(true, None);
            self.data[idx].set_anchor(Anchor::Output);
            self.animate_move_from(idx, prev_pos);
        } else if self.tiles[idx].saved_maximize {
            self.tiles[idx].saved_maximize = false;
            self.apply_maximized(idx);
        } else {
            self.restore_normal(idx);
        }

        true
    }

    /// mutter's `meta_window_save_rect`: remembers the geometry to come back to.
    ///
    /// A window that is already edge-tiled has one saved from before the tile and keeps it — the
    /// restore target is the pre-tile rect, not the tiled one (mutter's `saved_rect` flows from
    /// tile into maximize). Only ever called on a window that is currently normal-sized.
    fn save_restore_rect(&mut self, idx: usize) {
        let area = self.working_area;
        let current_pos = self.data[idx].logical_pos;
        let tile = &mut self.tiles[idx];

        if tile.window().edge_tiled_side().is_some() {
            tile.window_mut().set_edge_tiled(None);
            return;
        }

        let win = tile.window();
        let size = win.expected_size().unwrap_or_else(|| win.size());
        tile.tiled_restore_size = Some(size);
        tile.tiled_restore_pos = Some(Data::logical_to_size_frac_in_working_area(
            area,
            current_pos,
        ));
    }

    /// Sizes the tile to the work area and pins it there.
    fn apply_maximized(&mut self, idx: usize) {
        let prev_pos = self.data[idx].logical_pos;
        let size = self.working_area.size;
        self.tiles[idx].request_maximized(size, true, None);
        self.data[idx].set_anchor(Anchor::WorkArea);
        self.animate_move_from(idx, prev_pos);
    }

    /// Returns the tile to normal sizing on its saved rect, from maximized, fullscreen or tiled.
    fn restore_normal(&mut self, idx: usize) {
        let tile = &mut self.tiles[idx];
        tile.window_mut().set_edge_tiled(None);
        tile.saved_maximize = false;

        let size = tile.tiled_restore_size.take().unwrap_or_default();
        let restore_pos = tile.tiled_restore_pos.take();

        let win = tile.window_mut();
        let min_size = win.min_size();
        let max_size = win.max_size();
        let w = ensure_min_max_size_maybe_zero(size.w, min_size.w, max_size.w);
        let h = ensure_min_max_size_maybe_zero(size.h, min_size.h, max_size.h);
        win.request_size_once(Size::from((w, h)), true);

        let prev_pos = self.data[idx].logical_pos;
        if let Some(pos) = restore_pos {
            let pos = self.scale_by_working_area(pos);
            self.data[idx].set_logical_pos(pos);
        }
        self.data[idx].set_anchor(Anchor::Free);
        self.animate_move_from(idx, prev_pos);

        self.interactive_resize_end(None);
    }

    pub fn center_window(&mut self, id: Option<&W::Id>) {
        let Some(id) = id.or(self.active_window_id.as_ref()).cloned() else {
            return;
        };
        let idx = self.idx_of(&id).unwrap();

        let new_pos = center_preferring_top_left_in_area(self.working_area, self.data[idx].size);
        self.move_to(idx, new_pos, true);
    }

    pub fn descendants_added(&mut self, id: &W::Id) -> bool {
        let Some(idx) = self.idx_of(id) else {
            return false;
        };

        self.bring_up_descendants_of(idx);
        true
    }

    pub fn update_window(&mut self, id: &W::Id, serial: Option<Serial>) -> bool {
        let Some(tile_idx) = self.idx_of(id) else {
            return false;
        };

        let tile = &mut self.tiles[tile_idx];
        let data = &mut self.data[tile_idx];

        let resize = tile.window_mut().interactive_resize_data();

        // Do this before calling update_window() so it can get up-to-date info.
        if let Some(serial) = serial {
            tile.window_mut().on_commit(serial);
        }

        let prev_size = data.size;

        tile.update_window();
        data.update(tile);

        // When resizing by top/left edge, update the position accordingly.
        if let Some(resize) = resize {
            let mut offset = Point::from((0., 0.));
            if resize.edges.contains(ResizeEdge::LEFT) {
                offset.x += prev_size.w - data.size.w;
            }
            if resize.edges.contains(ResizeEdge::TOP) {
                offset.y += prev_size.h - data.size.h;
            }
            data.set_logical_pos(data.logical_pos + offset);
        }

        true
    }

    pub fn render(
        &self,
        mut ctx: RenderCtx,
        xray_pos: XrayPos,
        view_rect: Rectangle<f64, Logical>,
        focus_ring: bool,
        push: &mut dyn FnMut(FloatingSpaceRenderElement),
    ) {
        let scale = Scale::from(self.scale);

        // Draw the closing windows on top of the other windows.
        //
        // FIXME: I guess this should rather preserve the stacking order when the window is closed.
        for closing in self.closing_windows.iter().rev() {
            {
                let vctx = ctx.r();
                if let Some(elem) =
                    closing.render_vulkan(vctx.renderer, view_rect, scale, vctx.target)
                {
                    push(elem.into());
                }
            }
        }

        let active = self.active_window_id.clone();
        let raised = self.cycler_raised.clone();
        // `push` is front-to-back, so the cycler's window goes out *first* to land on top, and
        // is then skipped in its own place below.
        let mut draw = |tile: &Tile<W>, tile_pos, ctx: &mut RenderCtx| {
            let focus_ring = focus_ring && Some(tile.window().id()) == active.as_ref();
            let xray_pos = xray_pos.offset(tile_pos);
            tile.render(ctx.r(), tile_pos, xray_pos, focus_ring, &mut |elem| {
                push(elem.into())
            });
        };

        if let Some(id) = &raised {
            if let Some((tile, tile_pos)) = self
                .tiles_with_render_positions()
                .find(|(tile, _)| tile.window().id() == id)
            {
                draw(tile, tile_pos, &mut ctx);
            }
        }

        for (tile, tile_pos) in self.tiles_with_render_positions() {
            if Some(tile.window().id()) == raised.as_ref() {
                continue;
            }
            draw(tile, tile_pos, &mut ctx);
        }
    }

    pub fn interactive_resize_begin(&mut self, window: W::Id, edges: ResizeEdge) -> bool {
        if self.interactive_resize.is_some() {
            return false;
        }

        let tile = self
            .tiles
            .iter_mut()
            .find(|tile| tile.window().id() == &window)
            .unwrap();

        // A maximized or fullscreen window owns its geometry; mutter refuses the resize grab
        // outright (`meta_window_begin_grab_op` -> `has_resize_func`).
        if !tile.window().pending_sizing_mode().is_normal() {
            return false;
        }

        let original_window_size = tile.window_size();

        let resize = InteractiveResize {
            window,
            original_window_size,
            data: InteractiveResizeData { edges },
        };
        self.interactive_resize = Some(resize);

        true
    }

    pub fn interactive_resize_update(
        &mut self,
        window: &W::Id,
        delta: Point<f64, Logical>,
    ) -> bool {
        let Some(resize) = &self.interactive_resize else {
            return false;
        };

        if window != &resize.window {
            return false;
        }

        let original_window_size = resize.original_window_size;
        let edges = resize.data.edges;

        if edges.intersects(ResizeEdge::LEFT_RIGHT) {
            let mut dx = delta.x;
            if edges.contains(ResizeEdge::LEFT) {
                dx = -dx;
            };

            let window_width = (original_window_size.w + dx).round() as i32;
            self.set_window_width(Some(window), SizeChange::SetFixed(window_width), false);
        }

        if edges.intersects(ResizeEdge::TOP_BOTTOM) {
            let mut dy = delta.y;
            if edges.contains(ResizeEdge::TOP) {
                dy = -dy;
            };

            let window_height = (original_window_size.h + dy).round() as i32;
            self.set_window_height(Some(window), SizeChange::SetFixed(window_height), false);
        }

        true
    }

    pub fn interactive_resize_end(&mut self, window: Option<&W::Id>) {
        let Some(resize) = &self.interactive_resize else {
            return;
        };

        if let Some(window) = window {
            if window != &resize.window {
                return;
            }
        }

        self.interactive_resize = None;
    }

    pub fn refresh(&mut self, is_active: bool, is_focused: bool) {
        let active = self.active_window_id.clone();
        for tile in &mut self.tiles {
            let win = tile.window_mut();

            win.set_active_in_column(true);
            win.set_floating(true);

            let mut is_active = is_active && Some(win.id()) == active.as_ref();
            if self.options.deactivate_unfocused_windows {
                is_active &= is_focused;
            }
            win.set_activated(is_active);

            let resize_data = self
                .interactive_resize
                .as_ref()
                .filter(|resize| &resize.window == win.id())
                .map(|resize| resize.data);
            win.set_interactive_resize(resize_data);

            let border_config = self.options.layout.border.merged_with(&win.rules().border);
            let bounds = compute_toplevel_bounds(border_config, self.working_area.size);
            win.set_bounds(bounds);

            // If transactions are disabled, also disable combined throttling, for more
            // intuitive behavior.
            let intent = if self.options.disable_resize_throttling {
                ConfigureIntent::CanSend
            } else {
                win.configure_intent()
            };

            if matches!(
                intent,
                ConfigureIntent::CanSend | ConfigureIntent::ShouldSend
            ) {
                win.send_pending_configure();
            }

            win.refresh();
        }
    }

    pub fn clamp_within_working_area(
        &self,
        pos: Point<f64, Logical>,
        size: Size<f64, Logical>,
    ) -> Point<f64, Logical> {
        let mut rect = Rectangle::new(pos, size);
        clamp_preferring_top_left_in_area(self.working_area, &mut rect);
        rect.loc
    }

    pub fn scale_by_working_area(&self, pos: Point<f64, SizeFrac>) -> Point<f64, Logical> {
        Data::scale_by_working_area(self.working_area, pos)
    }

    pub fn logical_to_size_frac(&self, logical_pos: Point<f64, Logical>) -> Point<f64, SizeFrac> {
        Data::logical_to_size_frac_in_working_area(self.working_area, logical_pos)
    }

    fn move_and_animate(&mut self, idx: usize, new_pos: Point<f64, Logical>) {
        let prev_pos = self.data[idx].logical_pos;
        self.data[idx].set_logical_pos(new_pos);
        self.animate_move_from(idx, prev_pos);
    }

    /// Animates the tile in from where it used to be, after its position has already changed.
    fn animate_move_from(&mut self, idx: usize, prev_pos: Point<f64, Logical>) {
        // Moves up to this logical pixel distance are not animated.
        const ANIMATION_THRESHOLD_SQ: f64 = 10. * 10.;

        let diff = prev_pos - self.data[idx].logical_pos;
        if diff.x * diff.x + diff.y * diff.y > ANIMATION_THRESHOLD_SQ {
            self.tiles[idx].animate_move_from(diff);
        }
    }

    pub fn new_window_size(
        &self,
        width: Option<PresetSize>,
        height: Option<PresetSize>,
        rules: &ResolvedWindowRules,
    ) -> Size<i32, Logical> {
        let border = self.options.layout.border.merged_with(&rules.border);

        let resolve = |size: Option<PresetSize>, working_area_size: f64| {
            if let Some(size) = size {
                let size = match resolve_preset_size(size, working_area_size) {
                    ResolvedSize::Tile(mut size) => {
                        if !border.off {
                            size -= border.width * 2.;
                        }
                        size
                    }
                    ResolvedSize::Window(size) => size,
                };

                max(1, size.floor() as i32)
            } else {
                0
            }
        };

        let width = resolve(width, self.working_area.size.w);
        let height = resolve(height, self.working_area.size.h);

        Size::from((width, height))
    }

    /// Places a tile with no stored or rule-given position, GNOME style.
    ///
    /// Reproduces mutter's `meta_window_place()` (`src/core/place.c`) with
    /// `attach-modal-dialogs` off and LTR text direction. Transients center on
    /// their parent; for the rest, `center-new-windows` picks the strategy
    /// exactly as mutter's `window_place_centered()` does — on (GNOME's
    /// default since mutter 48) the window goes to the middle of the work area
    /// and `find_first_fit` never runs at all, off it tries first-fit first.
    ///
    /// **Divergence (approved 2026-08-07, option B in
    /// `docs/fork/window-placement.md` §5).** The off branch's *fallback* is
    /// mutter's centered cascade, not its origin one. mutter only ever pairs
    /// the centered cascade with skipping first-fit, so this is a mode it does
    /// not have: try not to overlap, and when nothing fits pile up from the
    /// middle of the screen rather than from the top-left corner. §1's
    /// complaint was windows appearing in the corner, and the origin cascade is
    /// the one path that still put them there.
    ///
    /// Two of mutter's inputs have no xdg-shell equivalent. Window *types*
    /// (splash, utility, dock) are absent, so `window_place_centered()`'s
    /// unconditional centering of dialogs collapses into the pref: a dialog
    /// with a parent takes the transient path below, and a parentless one is
    /// indistinguishable from a normal window to us. The constraint pipeline is
    /// approximated by [`Data`]'s mutter-derived off-screen limits.
    fn place_new_tile(&self, tile: &Tile<W>) -> Point<f64, Logical> {
        let size = tile.tile_size();

        // Transient windows center horizontally on their parent, vertically
        // with twice as much space below as above (place.c "chosen to be the
        // same as the placement of the child within the parent's frame").
        let win = tile.window();
        for (other, data) in self.tiles.iter().zip(&self.data) {
            if win.is_child_of(other.window()) {
                let x = data.logical_pos.x + (data.size.w - size.w) / 2.;
                let y = data.logical_pos.y + (data.size.h - size.h) / 3.;
                return Point::from((x, y));
            }
        }

        // place.c:1018-1032. The centered branch goes straight to the cascade —
        // there is no first-fit attempt, so nothing tries to keep windows from
        // overlapping.
        if self.options.gnome_center_new_windows {
            return self.find_next_cascade(size);
        }

        self.find_first_fit(size)
            .unwrap_or_else(|| self.find_next_cascade(size))
    }

    /// mutter's `find_first_fit()`: the first candidate position where the
    /// window fits inside the work area without overlapping any existing
    /// window.
    ///
    /// Candidates, in order: the "centered tile" slot, then below each
    /// existing window (top-most/left-most first), then to the right of each
    /// (left-most/top-most first).
    fn find_first_fit(&self, size: Size<f64, Logical>) -> Option<Point<f64, Logical>> {
        let windows: Vec<_> = self
            .tiles
            .iter()
            .zip(&self.data)
            .map(|(tile, data)| {
                (
                    Rectangle::new(data.logical_pos, data.size),
                    tile.window().is_transient(),
                )
            })
            .collect();
        self.find_first_fit_among(size, &windows)
    }

    /// [`Self::find_first_fit`] over an explicit window list — mutter passes a
    /// one-element list here when re-placing a focus-denied window
    /// (place.c:1073-1078). Each entry carries whether it is a transient, i.e.
    /// whether it counts as an obstacle.
    fn find_first_fit_among(
        &self,
        size: Size<f64, Logical>,
        windows: &[(Rectangle<f64, Logical>, bool)],
    ) -> Option<Point<f64, Logical>> {
        let area = self.working_area;
        let others: Vec<Rectangle<f64, Logical>> = windows.iter().map(|(rect, _)| *rect).collect();

        // Candidate positions come from *every* window, but only some of them
        // block a candidate: `rectangle_overlaps_some_window` (place.c:503-548)
        // skips dialogs, docks and splash screens, while place.c:698 and :724
        // walk the unfiltered list. See `LayoutElement::is_transient` for how
        // "dialog" maps onto xdg-shell.
        let obstacles: Vec<Rectangle<f64, Logical>> = windows
            .iter()
            .filter(|(_, transient)| !transient)
            .map(|(rect, _)| *rect)
            .collect();

        let fits = |pos: Point<f64, Logical>| {
            let rect = Rectangle::new(pos, size);
            area.contains_rect(rect) && !obstacles.iter().any(|other| other.overlaps(rect))
        };

        // The "centered tile" slot: the top-left tile of a hypothetical grid
        // of same-size windows spread over the work area (place.c
        // `center_tile_rect_in_area`; the remainder is the leftover space).
        let candidate = Point::from((
            area.loc.x + (area.size.w % (size.w + 1.)) / 2.,
            area.loc.y + (area.size.h % (size.h + 1.)) / 3.,
        ));
        if fits(candidate) {
            return Some(candidate);
        }

        let mut below_sorted = others.clone();
        below_sorted.sort_by(|a, b| (a.loc.y, a.loc.x).partial_cmp(&(b.loc.y, b.loc.x)).unwrap());
        for other in &below_sorted {
            let candidate = Point::from((other.loc.x, other.loc.y + other.size.h));
            if fits(candidate) {
                return Some(candidate);
            }
        }

        let mut end_sorted = others.clone();
        end_sorted.sort_by(|a, b| (a.loc.x, a.loc.y).partial_cmp(&(b.loc.x, b.loc.y)).unwrap());
        for other in &end_sorted {
            let candidate = Point::from((other.loc.x + other.size.w, other.loc.y));
            if fits(candidate) {
                return Some(candidate);
            }
        }

        None
    }

    /// mutter's `find_next_cascade()`: walk the windows, stepping the cascade
    /// slot diagonally past every window already sitting on it; overflowing the
    /// work area starts a fresh column shifted right.
    ///
    /// Always mutter's `place_centered = TRUE` shape: the slot starts at the
    /// center of the work area, and the walk order is nearest-the-center-first
    /// (`window_distance_cmp`, place.c:64-101). mutter's other shape — slot at
    /// the work-area origin, walk order northwest-first by `x + y` — has no
    /// caller here; see [`Self::place_new_tile`] for why, and
    /// `docs/fork/window-placement.md` §5 for what it looked like.
    fn find_next_cascade(&self, size: Size<f64, Logical>) -> Point<f64, Logical> {
        // place.c: CASCADE_FUZZ, META_WINDOW_TITLEBAR_HEIGHT, CASCADE_INTERVAL.
        const FUZZ: f64 = 15.;
        const STEP: f64 = 50.;
        const INTERVAL: f64 = 50.;

        let area = self.working_area;
        // place.c:225-244. Note the center is `w / 2 - size / 2`, not
        // `(w - size) / 2`: mutter computes it that way and the two disagree by
        // a half-pixel, so keep its version.
        let center: Point<f64, Logical> = Point::from((
            area.loc.x + area.size.w / 2. - size.w / 2.,
            area.loc.y + area.size.h / 2. - size.h / 2.,
        ));
        let origin = Point::from((center.x, f64::max(0., center.y)));

        let mut others: Vec<(Point<f64, Logical>, Size<f64, Logical>)> = self
            .data
            .iter()
            .map(|data| (data.logical_pos, data.size))
            .collect();
        // Squared distance from the centered corner. mutter measures from the
        // *new* window's centered corner (so only its size enters here), and
        // uses the unclamped one, unlike the slot above.
        let dist = |pos: Point<f64, Logical>| {
            let dx = center.x - pos.x;
            let dy = center.y - pos.y;
            dx * dx + dy * dy
        };
        others.sort_by(|a, b| dist(a.0).partial_cmp(&dist(b.0)).unwrap());

        let mut stage = 0.;
        'restart: loop {
            let mut cascade = Point::from((origin.x + stage * INTERVAL, origin.y));
            if cascade.x + size.w > area.loc.x + area.size.w || cascade.x < area.loc.x {
                // Out of horizontal space entirely; give up at the origin. (The
                // left-edge test only bites when centering a window wider than
                // the work area.)
                return origin;
            }

            for (pos, _) in &others {
                if (pos.x - cascade.x).abs() < FUZZ && (pos.y - cascade.y).abs() < FUZZ {
                    // Something already sits at this slot; step past it.
                    cascade = *pos + Point::from((STEP, STEP));
                    if cascade.x + size.w > area.loc.x + area.size.w
                        || cascade.y + size.h > area.loc.y + area.size.h
                    {
                        stage += 1.;
                        continue 'restart;
                    }
                }
            }

            return cascade;
        }
    }

    /// mutter's denied-focus placement (place.c:1052-1086): a window that was
    /// refused focus, and is not a transient of the window holding it, must not
    /// cover that window if there is anywhere else to put it. Re-runs first-fit
    /// against the focus window alone, then falls back to whichever side of it
    /// shows the most of the new window.
    ///
    /// mutter runs this inside `meta_window_place`; we run it just after the
    /// tile lands, which is why it re-checks that the window was auto-placed —
    /// a stored position or a `default_floating_position` rule skips placement
    /// entirely and must not be overridden here.
    ///
    /// Returns whether the window moved.
    pub fn avoid_focus_window(&mut self, window: &W::Id, focus: &W::Id) -> bool {
        let (Some(idx), Some(focus_idx)) = (self.idx_of(window), self.idx_of(focus)) else {
            // A focus window on another workspace or output cannot be overlapped.
            return false;
        };
        if idx == focus_idx || self.stored_or_default_tile_pos(&self.tiles[idx]).is_some() {
            return false;
        }

        let size = self.data[idx].size;
        let pos = self.data[idx].logical_pos;
        let avoid = Rectangle::new(self.data[focus_idx].logical_pos, self.data[focus_idx].size);

        // `window_overlaps_focus_window` (place.c:425-446). No overlap, nothing to do.
        if !Rectangle::new(pos, size).overlaps(avoid) {
            return false;
        }

        let placed = self
            .find_first_fit_among(size, &[(avoid, false)])
            .unwrap_or_else(|| self.find_most_freespace(size, avoid, pos));
        if placed == pos {
            return false;
        }

        self.data[idx].set_logical_pos(placed);
        true
    }

    /// mutter's `find_most_freespace()` (place.c:332-423): put the window on
    /// whichever side of `avoid` shows the most of it, flush against `avoid`
    /// when the window fits beside it and against the work area when it does
    /// not. Returns `current` unchanged when there is nowhere to go.
    fn find_most_freespace(
        &self,
        size: Size<f64, Logical>,
        avoid: Rectangle<f64, Logical>,
        current: Point<f64, Logical>,
    ) -> Point<f64, Logical> {
        enum Side {
            Left,
            Right,
            Top,
            Bottom,
        }

        let area = self.working_area;
        let max_width = f64::min(avoid.size.w, size.w);
        let max_height = f64::min(avoid.size.h, size.h);

        let left_space = avoid.loc.x - area.loc.x;
        let right_space = area.size.w - (avoid.loc.x + avoid.size.w - area.loc.x);
        let top_space = avoid.loc.y - area.loc.y;
        let bottom_space = area.size.h - (avoid.loc.y + avoid.size.h - area.loc.y);

        let left = f64::min(left_space, size.w);
        let right = f64::min(right_space, size.w);
        let top = f64::min(top_space, size.h);
        let bottom = f64::min(bottom_space, size.h);

        // Ties go to the earlier side, like mutter's chain of strict `>`.
        let mut side = Side::Left;
        let mut max_area = left * max_height;
        if right * max_height > max_area {
            side = Side::Right;
            max_area = right * max_height;
        }
        if top * max_width > max_area {
            side = Side::Top;
            max_area = top * max_width;
        }
        if bottom * max_width > max_area {
            side = Side::Bottom;
            max_area = bottom * max_width;
        }

        // Nowhere to put it — the focus window is maximized. mutter tests
        // `max_area == 0`; we take `<= 0` as well, since a negative product
        // means the focus window is already outside the work area and the
        // chosen side would push the new window off-screen.
        if max_area <= 0. {
            return current;
        }

        match side {
            Side::Left => Point::from((
                if left_space > size.w {
                    avoid.loc.x - size.w
                } else {
                    area.loc.x
                },
                avoid.loc.y,
            )),
            Side::Right => Point::from((
                if right_space > size.w {
                    avoid.loc.x + avoid.size.w
                } else {
                    area.loc.x + area.size.w - size.w
                },
                avoid.loc.y,
            )),
            Side::Top => Point::from((
                avoid.loc.x,
                if top_space > size.h {
                    avoid.loc.y - size.h
                } else {
                    area.loc.y
                },
            )),
            Side::Bottom => Point::from((
                avoid.loc.x,
                if bottom_space > size.h {
                    avoid.loc.y + avoid.size.h
                } else {
                    area.loc.y + area.size.h - size.h
                },
            )),
        }
    }

    pub fn stored_or_default_tile_pos(&self, tile: &Tile<W>) -> Option<Point<f64, Logical>> {
        let pos = tile.floating_pos.map(|pos| self.scale_by_working_area(pos));
        pos.or_else(|| {
            tile.window().rules().default_floating_position.map(|pos| {
                let relative_to = pos.relative_to;
                let size = tile.tile_size();
                let area = self.working_area;

                let mut pos = Point::from((pos.x.0, pos.y.0));
                if relative_to == RelativeTo::TopRight
                    || relative_to == RelativeTo::BottomRight
                    || relative_to == RelativeTo::Right
                {
                    pos.x = area.size.w - size.w - pos.x;
                }
                if relative_to == RelativeTo::BottomLeft
                    || relative_to == RelativeTo::BottomRight
                    || relative_to == RelativeTo::Bottom
                {
                    pos.y = area.size.h - size.h - pos.y;
                }
                if relative_to == RelativeTo::Top || relative_to == RelativeTo::Bottom {
                    pos.x += area.size.w / 2.0 - size.w / 2.0
                }
                if relative_to == RelativeTo::Left || relative_to == RelativeTo::Right {
                    pos.y += area.size.h / 2.0 - size.h / 2.0
                }

                pos + self.working_area.loc
            })
        })
    }

    #[cfg(test)]
    pub fn view_size(&self) -> Size<f64, Logical> {
        self.view_size
    }

    pub fn working_area(&self) -> Rectangle<f64, Logical> {
        self.working_area
    }

    #[cfg(test)]
    pub fn scale(&self) -> f64 {
        self.scale
    }

    #[cfg(test)]
    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    #[cfg(test)]
    pub fn options(&self) -> &Rc<Options> {
        &self.options
    }

    #[cfg(test)]
    pub fn verify_invariants(&self) {
        assert!(self.scale > 0.);
        assert!(self.scale.is_finite());
        assert_eq!(self.tiles.len(), self.data.len());

        for (i, (tile, data)) in zip(&self.tiles, &self.data).enumerate() {
            use crate::layout::SizingMode;

            assert!(Rc::ptr_eq(&self.options, &tile.options));
            assert_eq!(self.view_size, tile.view_size());
            assert_eq!(self.clock, tile.clock);
            assert_eq!(self.scale, tile.scale());
            tile.verify_invariants();

            if let Some(idx) = tile.floating_preset_width_idx {
                assert!(idx < self.options.layout.preset_column_widths.len());
            }
            if let Some(idx) = tile.floating_preset_height_idx {
                assert!(idx < self.options.layout.preset_window_heights.len());
            }

            // Only in niri's scrolling mode: there, sizing a window to the screen is the
            // scrolling layout's job, so `Workspace::set_fullscreen` migrates the tile out
            // of here first. In GNOME mode there is nowhere to migrate to — the floating
            // space owns maximize and fullscreen (`Workspace::set_fullscreen` hands
            // straight to `FloatingSpace::set_fullscreen`), so a tile in a non-normal
            // sizing mode is the expected state, not a leak.
            if self.options.layout.windowing_mode == WindowingMode::Scrolling {
                assert_eq!(
                    tile.window().pending_sizing_mode(),
                    SizingMode::Normal,
                    "in scrolling mode floating windows cannot be maximized or fullscreen"
                );
            }

            data.verify_invariants();

            let mut data2 = *data;
            data2.update(tile);
            data2.update_config(self.working_area);
            assert_eq!(data, &data2, "tile data must be up to date");

            for tile_below in &self.tiles[i + 1..] {
                assert!(
                    !tile_below.window().is_child_of(tile.window()),
                    "children must be stacked above parents"
                );
            }
        }

        if let Some(id) = &self.active_window_id {
            assert!(!self.tiles.is_empty());
            assert!(self.contains(id), "active window must be present in tiles");
        } else {
            assert!(self.tiles.is_empty());
        }

        if let Some(resize) = &self.interactive_resize {
            assert!(
                self.contains(&resize.window),
                "interactive resize window must be present in tiles"
            );
        }
    }
}

fn compute_toplevel_bounds(
    border_config: synoik_config::Border,
    working_area_size: Size<f64, Logical>,
) -> Size<i32, Logical> {
    let mut border = 0.;
    if !border_config.off {
        border = border_config.width * 2.;
    }

    Size::from((
        f64::max(working_area_size.w - border, 1.),
        f64::max(working_area_size.h - border, 1.),
    ))
    .to_i32_floor()
}

fn resolve_preset_size(preset: PresetSize, view_size: f64) -> ResolvedSize {
    match preset {
        PresetSize::Proportion(proportion) => ResolvedSize::Tile(view_size * proportion),
        PresetSize::Fixed(width) => ResolvedSize::Window(f64::from(width)),
    }
}
