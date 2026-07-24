//! The overview **app grid** — the page of installed apps the show-apps button
//! reveals (`js/ui/appDisplay.js` `AppDisplay`). Like the dash ([`crate::ui::dash`])
//! and the search ([`crate::ui::overview_search`]), this owns only plain state (a
//! snapshot of grid entries plus the mouse hover); it does NOT own the catalog.
//! `Niri::sync_app_grid` snapshots the model in and [`Niri`] launches on a click.
//!
//! **Faithful behavior (cited 50.1).** Membership = every installed app that should
//! show, **minus** the favorites (they live in the dash) and minus parental-control
//! hidden apps: `AppDisplay._loadApps` is `Shell.AppSystem.get_installed().filter(a =>
//! !appFavorites.isFavorite(a.get_id()) && parentalControls.shouldShowApp(a))`
//! (`appDisplay.js:1492-1504`). Order is `a.name.localeCompare(b.name)`
//! (`_compareItems`, `appDisplay.js:1122-1124`). A click launches the app and closes
//! the overview (`AppIcon.activate` → `Main.overview.hide`, `appDisplay.js:3060,3077`).
//! Tiles reuse the shared 96 px `.overview-tile` ([`widget::TileMetrics::OVERVIEW`]),
//! which GNOME builds from the same `IconGrid.BaseIcon` as the search results.
//!
//! **Divergences (S8a MVP), revisited later.** Single page only: GNOME's `IconGrid`
//! picks a fixed rows×columns page mode ({3×8,4×6,6×4,8×3}, `iconGrid.js:30-46`) and
//! *paginates* the rest; we lay a fill-by-width grid into the `app_display` band and
//! **drop** the apps that don't fit (they stay reachable through the overview search).
//! Pagination, folders, and drag-reorder are follow-ups, so we also ignore the saved
//! `app-picker-layout` and sort purely by name. No keyboard navigation yet (Escape
//! closes the grid via the overview bind; arrow/Enter keynav is a follow-up). The
//! sort is a case-folded `to_lowercase` compare rather than full locale collation
//! (`localeCompare`): std has no collator, so accented initials can misplace; an
//! `icu` collator is the faithful fix. Like the dash and search, the grid draws on
//! **every** output with one shared hover (GNOME shows it on the primary only);
//! hit-testing stays per-output.

use std::cell::RefCell;

use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::{ContextId, Renderer};
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Size, Transform};

use crate::app_system::AppIconRef;
use crate::render_helpers::icon::AppIconCache;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::ui::widget::{self, style, AppIconUploads, Painter, TileMetrics};

/// Grid tile label point size (`%caption`), shared with the search results.
const LABEL_PT: f64 = 10.;
/// Gap between grid tiles, matching the search grid (`.grid` `spacing`,
/// `$base_padding*5`=30).
const GRID_SPACING: f64 = 30.;

/// One grid app — a plain-data snapshot (not a live catalog borrow), like
/// [`crate::ui::dash::DashEntry`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppGridEntry {
    pub id: String,
    pub name: String,
    pub icon: AppIconRef,
}

#[derive(Default)]
struct GridCache {
    context: Option<ContextId<VkTexture>>,
    bake: widget::BakeCache,
    /// Full-color icon uploads (shared key space with the dash's and search's).
    icons: AppIconUploads,
}

/// The app-grid model. Owned on `Niri`; fed by `sync_app_grid`.
pub struct AppGrid {
    entries: Vec<AppGridEntry>,
    /// The mouse-hovered tile, if any — drives the `.overview-tile:hover` wash.
    hovered: Option<usize>,
    /// Bumped on any change that affects the bake (entries/hover).
    content_rev: u64,
    cache: RefCell<GridCache>,
}

impl Default for AppGrid {
    fn default() -> Self {
        Self::new()
    }
}

/// Computed tile geometry for one `app_display` box.
struct GridLayout {
    /// The tile boxes (logical, output coords), in row-major order — only those that
    /// fit the box; trailing apps are dropped.
    tiles: Vec<Rectangle<f64, Logical>>,
    /// The bounding block of the tiles (what the chrome bakes into).
    block: Rectangle<f64, Logical>,
}

impl AppGrid {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            hovered: None,
            content_rev: 0,
            cache: RefCell::new(GridCache::default()),
        }
    }

    /// Replace the grid's apps (`AppDisplay._redisplay`). Returns whether anything
    /// changed (→ redraw). The caller sorts; this stores verbatim.
    pub fn set_entries(&mut self, entries: Vec<AppGridEntry>) -> bool {
        if self.entries == entries {
            return false;
        }
        self.entries = entries;
        // Drop a now-stale hover rather than washing the wrong tile.
        if self.hovered.is_some_and(|i| i >= self.entries.len()) {
            self.hovered = None;
        }
        self.content_rev += 1;
        true
    }

    /// Drop cached icon uploads (icon-theme / installed change).
    pub fn clear_icon_uploads(&self) {
        self.cache.borrow_mut().icons.clear();
    }

    /// The id of tile `i`, if present (what a click launches).
    pub fn entry_id(&self, i: usize) -> Option<&str> {
        self.entries.get(i).map(|e| e.id.as_str())
    }

    /// Set the mouse-hovered tile; returns whether it changed (→ redraw).
    pub fn set_hovered(&mut self, hovered: Option<usize>) -> bool {
        if self.hovered == hovered {
            return false;
        }
        self.hovered = hovered;
        self.content_rev += 1;
        true
    }

    /// Lay the apps into `area` (the `app_display` band): a fill-by-width grid,
    /// centered, capped to the rows that fit. Trailing apps are dropped.
    fn layout(&self, area: Rectangle<f64, Logical>) -> GridLayout {
        let empty = GridLayout {
            tiles: Vec::new(),
            block: Rectangle::from_size(Size::from((0., 0.))),
        };
        let n = self.entries.len();
        let tile = TileMetrics::OVERVIEW.size();
        if n == 0 || area.size.w < tile.w || area.size.h < tile.h {
            return empty;
        }

        // Columns/rows that fit, each at least one; a step is a tile plus one gap.
        let cols =
            (((area.size.w + GRID_SPACING) / (tile.w + GRID_SPACING)).floor() as usize).clamp(1, n);
        let rows_fit =
            (((area.size.h + GRID_SPACING) / (tile.h + GRID_SPACING)).floor() as usize).max(1);
        let rows = n.div_ceil(cols).min(rows_fit);
        let count = (cols * rows).min(n);

        // Center the block (sized for the full column count, so a partial last row
        // is left-aligned within it — gnome-shell's grid alignment).
        let block_w = cols as f64 * tile.w + (cols as f64 - 1.) * GRID_SPACING;
        let block_h = rows as f64 * tile.h + (rows as f64 - 1.) * GRID_SPACING;
        let start_x = (area.loc.x + (area.size.w - block_w) / 2.).round();
        let start_y = (area.loc.y + (area.size.h - block_h) / 2.).round();

        let tiles = (0..count)
            .map(|i| {
                let (r, c) = (i / cols, i % cols);
                let x = start_x + c as f64 * (tile.w + GRID_SPACING);
                let y = start_y + r as f64 * (tile.h + GRID_SPACING);
                Rectangle::new(Point::from((x, y)), tile)
            })
            .collect();

        GridLayout {
            tiles,
            block: Rectangle::new(
                Point::from((start_x, start_y)),
                Size::from((block_w, block_h)),
            ),
        }
    }

    /// Which tile is under `pos` (logical, output coords), if any. The caller gates
    /// this on the grid being open and no search active.
    pub fn hit_test(
        &self,
        pos: Point<f64, Logical>,
        area: Rectangle<f64, Logical>,
    ) -> Option<usize> {
        self.layout(area)
            .tiles
            .iter()
            .position(|tile| tile.contains(pos))
    }

    /// The logical center of tile `i` — a geometry probe for the conformance corpus
    /// (which clicks real pixels routed through [`hit_test`](Self::hit_test)).
    #[cfg(test)]
    pub fn tile_center(
        &self,
        i: usize,
        area: Rectangle<f64, Logical>,
    ) -> Option<Point<f64, Logical>> {
        self.layout(area)
            .tiles
            .get(i)
            .map(|t| Point::from((t.loc.x + t.size.w / 2., t.loc.y + t.size.h / 2.)))
    }

    /// The number of tiles laid into `area` (some apps may be dropped) — a test probe.
    #[cfg(test)]
    pub fn visible_len(&self, area: Rectangle<f64, Logical>) -> usize {
        self.layout(area).tiles.len()
    }

    /// The grid render elements for `output`, into the `app_display` box, at `alpha`.
    /// Icons are pushed first (topmost within the grid); the tile chrome (wash +
    /// labels) bakes last (below the icons) — the dash/search order. The grid as a
    /// whole is pushed below the dash and search in `Niri::render`.
    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        app_icons: &AppIconCache,
        output: &Output,
        area: Rectangle<f64, Logical>,
        alpha: f32,
    ) -> Vec<TextureRenderElement<VkTexture>> {
        if alpha <= 0. {
            return Vec::new();
        }
        let layout = self.layout(area);
        if layout.tiles.is_empty() {
            return Vec::new();
        }

        let scale = output.current_scale().fractional_scale();
        let metrics = TileMetrics::OVERVIEW;

        let mut cache = self.cache.borrow_mut();
        let context = renderer.context_id();
        if cache.context.as_ref() != Some(&context) {
            cache.icons.clear();
            cache.context = Some(context);
        }

        let mut elements = Vec::new();

        // --- App icons (topmost, over their tiles). ---
        for (i, entry) in self.entries.iter().take(layout.tiles.len()).enumerate() {
            let center = metrics.icon_center(layout.tiles[i]);
            if let Some(el) = widget::app_icon_element(
                renderer,
                &mut cache.icons,
                app_icons,
                &entry.icon,
                metrics.icon_px,
                scale,
                Point::from((0., 0.)),
                center,
                alpha,
            ) {
                elements.push(el);
            }
        }

        // --- The tile chrome (hover wash + labels), one baked transparent texture the
        //     size of the block. ---
        let block = layout.block;
        let origin = block.loc;
        let hovered = self.hovered;
        let rel_rects: Vec<Rectangle<f64, Logical>> = layout
            .tiles
            .iter()
            .map(|t| Rectangle::new(t.loc - origin, t.size))
            .collect();
        let names: Vec<String> = self
            .entries
            .iter()
            .take(layout.tiles.len())
            .map(|e| e.name.clone())
            .collect();
        match widget::bake(
            renderer,
            &mut cache.bake,
            scale,
            block.size,
            self.content_rev,
            move |r| {
                let mut shaper = widget::TextShaper::new(r, scale);
                names
                    .iter()
                    .map(|name| shaper.shape(name, widget::TextStyle::new(LABEL_PT)))
                    .collect::<anyhow::Result<Vec<_>>>()
            },
            move |frame, phys, labels| {
                let mut p = Painter::new(frame, scale, phys);
                p.clear(style::TRANSPARENT)?;
                for (i, (rel, label)) in rel_rects.iter().zip(labels.iter()).enumerate() {
                    p.labelled_tile(*rel, label, &metrics, hovered == Some(i), style::TEXT)?;
                }
                Ok(())
            },
        ) {
            Ok(texture) => {
                let buffer = TextureBuffer::from_texture(
                    renderer,
                    texture,
                    scale,
                    Transform::Normal,
                    vec![],
                );
                elements.push(TextureRenderElement::from_texture_buffer(
                    buffer,
                    block.loc,
                    alpha,
                    None,
                    None,
                    Kind::Unspecified,
                ));
            }
            Err(err) => tracing::error!("error baking the app grid: {err:#}"),
        }

        elements
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, name: &str) -> AppGridEntry {
        AppGridEntry {
            id: id.to_owned(),
            name: name.to_owned(),
            icon: AppIconRef::Fallback,
        }
    }

    fn grid(names: &[&str]) -> AppGrid {
        let mut g = AppGrid::new();
        g.set_entries(names.iter().map(|n| entry(n, n)).collect());
        g
    }

    fn rect(x: f64, y: f64, w: f64, h: f64) -> Rectangle<f64, Logical> {
        Rectangle::new(Point::from((x, y)), Size::from((w, h)))
    }

    #[test]
    fn fills_by_width_and_centers() {
        // A 400-wide band fits three 120-wide tiles with two 30 gaps (=420) — no,
        // 3*120+2*30=420 > 400, so two columns (2*120+30=270 <= 400).
        let g = grid(&["a", "b", "c"]);
        let area = rect(0., 0., 400., 500.);
        let l = g.layout(area);
        assert_eq!(l.tiles.len(), 3);
        // Two columns: tiles 0,1 on the first row, tile 2 on the second.
        assert_eq!(l.tiles[0].loc.y, l.tiles[1].loc.y);
        assert!(l.tiles[2].loc.y > l.tiles[0].loc.y);
        // The block is centered horizontally in the band.
        let block_w: f64 = 2. * 120. + 30.;
        assert_eq!(l.block.loc.x, ((400. - block_w) / 2.).round());
    }

    #[test]
    fn drops_apps_that_do_not_fit() {
        // A short band fits a single row; extra apps are dropped, not clipped.
        let g = grid(&["a", "b", "c", "d", "e", "f"]);
        // Width for two columns, height for one row (< 2*144+30).
        let area = rect(0., 0., 300., 200.);
        assert_eq!(g.visible_len(area), 2);
    }

    #[test]
    fn empty_when_no_apps_or_no_room() {
        assert_eq!(grid(&[]).visible_len(rect(0., 0., 400., 400.)), 0);
        // A band narrower than one tile fits nothing.
        assert_eq!(grid(&["a"]).visible_len(rect(0., 0., 50., 400.)), 0);
    }

    #[test]
    fn hit_test_finds_the_tile_under_the_point() {
        let g = grid(&["a", "b", "c"]);
        let area = rect(0., 0., 400., 500.);
        let c1 = g.tile_center(1, area).unwrap();
        assert_eq!(g.hit_test(c1, area), Some(1));
        // A point in the block's gutter hits nothing.
        assert_eq!(g.hit_test(Point::from((-100., -100.)), area), None);
    }

    #[test]
    fn stale_hover_is_dropped_on_shrink() {
        let mut g = grid(&["a", "b", "c"]);
        assert!(g.set_hovered(Some(2)));
        assert!(g.set_entries(vec![entry("a", "a")]));
        assert_eq!(g.hovered, None);
    }
}
