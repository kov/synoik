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
//!
//! **Paginated layout (`IconGrid`, `iconGrid.js`).** The page mode `(columns, rows)`
//! is the `defaultGridModes` entry (`{3×8,4×6,6×4,8×3}`, `iconGrid.js:30-47`) whose
//! aspect ratio is closest to the page's (`_findBestModeForSize`); a wide `app_display`
//! picks 8×3. The icon shrinks to the largest `IconSize` whose square cells fit
//! (`_findBestIconSize`, tiles laid in a `max(w,h)` square cell). Column/row spacing
//! grows from `.icon-grid`'s 12 to a max of 36 to absorb slack, then the remainder
//! centers the page (`_calculateSpacing`, FILL). Overflow paginates: a dots row below
//! the grid (`.page-indicator`, 10px, inactive at 2/3 scale + half opacity) plus flat
//! circular **navigation arrows** in the side gutters (`.page-navigation-arrow`,
//! `carousel-arrow-{previous,next}-symbolic`, `appDisplay.js:553-575`; shown when a
//! previous/next page exists, `appDisplay.js:255-302`). Either dot, arrow, a wheel
//! notch (debounced 150ms), or a reset to page 0 on a fresh overview open (`'hidden'` →
//! `goToPage(0)`, `appDisplay.js:1342`) changes the page.
//!
//! **Divergences, revisited later.** No `indicatorsPadding` (the ~10% side reserve for
//! the DnD peek/arrows, `appDisplay.js:162-171`): geometry-identical at 1920, but it
//! shifts mode/icon-size selection at narrow widths, and (lacking that reserve) the
//! navigation arrows sit in the grid's centering gutter rather than a fixed 10% band —
//! they can crowd the edge tiles at very narrow widths. No page-slide animation (snap),
//! no touchpad **swipe** (continuous scroll over the grid is consumed but inert — the
//! 1:1 swipe is deferred), no keyboard paging
//! (`Page_Up/Down`), no folders/drag-reorder, and the saved `app-picker-layout` is
//! ignored (pure name sort). The sort is a case-folded `to_lowercase` compare rather
//! than full locale collation (`localeCompare`): std has no collator, so accented
//! initials can misplace; an `icu` collator is the faithful fix. Like the dash and
//! search, the grid draws on **every** output with one shared hover/page (GNOME shows
//! it on the primary only); hit-testing stays per-output.

use std::cell::RefCell;
use std::collections::HashMap;

use ordered_float::NotNan;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::{ContextId, Renderer};
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Size, Transform};

use crate::app_system::AppIconRef;
use crate::render_helpers::icon::{AppIconCache, IconCache};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::ui::widget::{self, style, AppIconUploads, Painter, TileMetrics};

/// Grid tile label point size (`%caption`), shared with the search results.
const LABEL_PT: f64 = 10.;

// `.icon-grid` page metrics (`_app-grid.scss:7-15`; `$base_padding` = 6). Spacing
// starts at the base and grows (distributing slack) up to the max, after which the
// remaining slack centers the page (`iconGrid.js` `_calculateSpacing`, FILL branch).
const COL_SPACING: f64 = 12.; // column-spacing: $base_padding*2
const ROW_SPACING: f64 = 12.; // row-spacing: $base_padding*2
const MAX_COL_SPACING: f64 = 36.; // max-column-spacing: $base_padding*6
const MAX_ROW_SPACING: f64 = 36.; // max-row-spacing: $base_padding*6
const PAGE_PAD_H: f64 = 18.; // page-padding-left/right: $base_padding*3
const PAGE_PAD_V: f64 = 24.; // page-padding-top/bottom: $base_padding*4

/// Adaptive page modes `(columns, rows)` (`iconGrid.js:30-47` `defaultGridModes`),
/// chosen by whichever `columns/rows` ratio is closest to the page's aspect ratio
/// (`_findBestModeForSize`, `iconGrid.js:1224`). A wide `app_display` picks 8×3.
const GRID_MODES: [(usize, usize); 4] = [(3, 8), (4, 6), (6, 4), (8, 3)];
/// The icon-size ladder, largest first (`IconSize`, `iconGrid.js:16-23`); the grid
/// shrinks the icon to the largest size whose page fits (`_findBestIconSize`).
const ICON_SIZES: [f64; 6] = [96., 64., 48., 32., 24., 16.];
/// A labelled `.overview-tile` is `icon + 48` tall (padding 12, icon, gap 6, label
/// 18, padding 12) and `icon + 24` wide; the grid lays it in a square cell of the
/// taller side (`iconGrid.js` `_getChildrenMaxSize` = `max(w, h)`), centered within.
const TILE_EXTRA_H: f64 = 48.;

// Page indicator dots (`_app-grid.scss:119-131`): 10px circles, `$system_fg_color`,
// `.page-indicator` padding `6 12 0`, `.page-indicators` margin-bottom 24.
const DOT_SIZE: f64 = 10.;
const DOT_PAD_TOP: f64 = 6.;
const DOT_PAD_SIDE: f64 = 12.;
const INDICATORS_MARGIN_BOTTOM: f64 = 24.;
/// The strip reserved below the grid for the dots row.
const INDICATORS_STRIP_H: f64 = DOT_PAD_TOP + DOT_SIZE + INDICATORS_MARGIN_BOTTOM;
/// Inactive dots are scaled to 2/3 and half-opacity (`pageIndicators.js:6-9,88-102`).
const INACTIVE_DOT_SCALE: f64 = 2. / 3.;
const INACTIVE_DOT_ALPHA: f32 = 0.5;

// Page navigation arrows (`.page-navigation-arrow`, `_app-grid.scss:172-185`): a flat
// circular button — transparent at rest, [`style::HOVER_WASH`] on hover — holding a
// `$medium_icon_size` (24px) `carousel-arrow-*-symbolic` chevron with `$base_padding*3`
// (18px) padding, so the disc is 24 + 36 = 60px.
const ARROW_ICON_PX: f64 = 24.;
const ARROW_PAD: f64 = 18.;
const ARROW_DISC: f64 = ARROW_ICON_PX + 2. * ARROW_PAD;

/// Which navigation arrow — the previous (left) or next (right) page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageArrow {
    Prev,
    Next,
}

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
    /// The page-indicator dots row.
    dots_bake: widget::BakeCache,
    /// The (constant) navigation-arrow hover-wash disc.
    arrow_bake: widget::BakeCache,
    /// Recolored arrow-chevron uploads, keyed by (output scale, is-next).
    arrow_icons: HashMap<(NotNan<f64>, bool), TextureBuffer<VkTexture>>,
    /// Full-color icon uploads (shared key space with the dash's and search's).
    icons: AppIconUploads,
}

/// The app-grid model. Owned on `Niri`; fed by `sync_app_grid`.
pub struct AppGrid {
    entries: Vec<AppGridEntry>,
    /// The mouse-hovered tile (an absolute entry index) — drives the
    /// `.overview-tile:hover` wash.
    hovered: Option<usize>,
    /// The mouse-hovered navigation arrow — drives its `.page-navigation-arrow:hover`
    /// wash.
    hovered_arrow: Option<PageArrow>,
    /// The current page (`AppDisplay` paginates; `iconGrid.js`). Clamped to the page
    /// count at layout time; reset to 0 on a fresh overview open.
    current_page: usize,
    /// Bumped on any change that affects the bake (entries/hover/page).
    content_rev: u64,
    cache: RefCell<GridCache>,
}

impl Default for AppGrid {
    fn default() -> Self {
        Self::new()
    }
}

/// Computed geometry for the app grid in one `app_display` box: the current page's
/// tiles plus the page-indicator layout.
struct GridLayout {
    /// The current page's tile boxes (logical, output coords), row-major.
    tiles: Vec<Rectangle<f64, Logical>>,
    /// The bounding block of the current page's cells (what the chrome bakes into).
    block: Rectangle<f64, Logical>,
    /// The chosen tile metrics (icon size may have shrunk to fit the page).
    metrics: TileMetrics,
    /// The absolute entry index of `tiles[0]` (= `page * items_per_page`).
    first_index: usize,
    /// Total page count (0 when there are no apps).
    n_pages: usize,
    /// The page these `tiles` belong to (clamped `current_page`).
    page: usize,
    /// The dot centers below the grid, one per page — `None` when `n_pages <= 1`.
    indicators: Option<Vec<Point<f64, Logical>>>,
    /// The previous-page arrow's disc box — `Some` only when a previous page exists.
    prev_arrow: Option<Rectangle<f64, Logical>>,
    /// The next-page arrow's disc box — `Some` only when a next page exists.
    next_arrow: Option<Rectangle<f64, Logical>>,
}

/// The FILL spacing distribution for one axis (`iconGrid.js` `_calculateSpacing`):
/// the inter-cell spacing grows from `base` to absorb slack, and once it hits `max`
/// the remaining slack centers the run. Returns `(origin_offset, spacing)` where the
/// offset is measured from the page edge (it already includes `pad`).
fn distribute(page_size: f64, n: usize, cell: f64, base: f64, max: f64, pad: f64) -> (f64, f64) {
    if n <= 1 {
        let empty = page_size - cell - 2. * pad;
        return (pad + (empty / 2.).max(0.), 0.);
    }
    let nf = n as f64;
    // Slack beyond a base-spacing layout (`emptyHSpace`, already net of padding).
    let empty = page_size - cell * nf - base * (nf - 1.) - 2. * pad;
    let mut spacing = base + empty / (nf - 1.);
    let mut offset = pad;
    if spacing > max {
        let extra = (max - base) * (nf - 1.);
        spacing = max;
        offset += ((empty - extra) / 2.).max(0.);
    }
    (offset, spacing)
}

impl AppGrid {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            hovered: None,
            hovered_arrow: None,
            current_page: 0,
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

    /// Every entry's icon — for the startup decode prewarm (`Niri::prewarm_app_icons`).
    pub fn icon_refs(&self) -> impl Iterator<Item = &AppIconRef> {
        self.entries.iter().map(|e| &e.icon)
    }

    /// Set the mouse-hovered tile (an absolute entry index); returns whether it
    /// changed (→ redraw).
    pub fn set_hovered(&mut self, hovered: Option<usize>) -> bool {
        if self.hovered == hovered {
            return false;
        }
        self.hovered = hovered;
        self.content_rev += 1;
        true
    }

    /// Set the mouse-hovered navigation arrow; returns whether it changed (→ redraw).
    pub fn set_arrow_hovered(&mut self, arrow: Option<PageArrow>) -> bool {
        if self.hovered_arrow == arrow {
            return false;
        }
        self.hovered_arrow = arrow;
        self.content_rev += 1;
        true
    }

    /// The current page.
    pub fn current_page(&self) -> usize {
        self.current_page
    }

    /// The number of pages for `area` (0 when empty). Navigation clamps against this.
    pub fn page_count(&self, area: Rectangle<f64, Logical>) -> usize {
        self.layout(area).n_pages
    }

    /// Go to `page` (clamped to `[0, n_pages)` for `area`); returns whether it moved.
    pub fn set_page(&mut self, page: usize, area: Rectangle<f64, Logical>) -> bool {
        let n_pages = self.page_count(area);
        let page = page.min(n_pages.saturating_sub(1));
        if page == self.current_page {
            return false;
        }
        self.current_page = page;
        self.content_rev += 1;
        true
    }

    /// Reset to the first page (a fresh overview open, `Main.overview 'hidden'` →
    /// `goToPage(0)`, `appDisplay.js:1342`); returns whether it moved.
    pub fn reset_page(&mut self) -> bool {
        if self.current_page == 0 {
            return false;
        }
        self.current_page = 0;
        self.content_rev += 1;
        true
    }

    /// Lay the apps into `area` (the `app_display` band) as GNOME's paginated
    /// `IconGrid`: pick the page mode by aspect ratio, shrink the icon to the largest
    /// size that fits, distribute the spacing, and position the current page's tiles
    /// in square cells. A dots strip is reserved at the bottom.
    fn layout(&self, area: Rectangle<f64, Logical>) -> GridLayout {
        let empty = GridLayout {
            tiles: Vec::new(),
            block: Rectangle::from_size(Size::from((0., 0.))),
            metrics: TileMetrics::OVERVIEW,
            first_index: 0,
            n_pages: 0,
            page: 0,
            indicators: None,
            prev_arrow: None,
            next_arrow: None,
        };
        let n = self.entries.len();
        // The grid page is the band minus the reserved dots strip.
        let page_w = area.size.w;
        let page_h = (area.size.h - INDICATORS_STRIP_H).max(0.);
        let content_w = page_w - 2. * PAGE_PAD_H;
        let content_h = page_h - 2. * PAGE_PAD_V;
        if n == 0 || content_w <= 0. || content_h <= 0. {
            return empty;
        }

        // Grid mode: the (columns, rows) whose ratio is closest to the content's.
        let ratio = content_w / content_h;
        let &(cols, rows) = GRID_MODES
            .iter()
            .min_by(|a, b| {
                let da = (a.0 as f64 / a.1 as f64 - ratio).abs();
                let db = (b.0 as f64 / b.1 as f64 - ratio).abs();
                da.total_cmp(&db)
            })
            .unwrap();

        // Icon size: the largest whose square cells fit cols×rows at base spacing.
        // Horizontal fit is inclusive, vertical strict (`iconGrid.js:395`).
        let icon = ICON_SIZES
            .iter()
            .copied()
            .find(|&size| {
                let cell = size + TILE_EXTRA_H;
                let used_w = cell * cols as f64 + COL_SPACING * (cols as f64 - 1.);
                let used_h = cell * rows as f64 + ROW_SPACING * (rows as f64 - 1.);
                used_w <= content_w && used_h < content_h
            })
            .unwrap_or(16.);
        let metrics = TileMetrics {
            icon_px: icon,
            ..TileMetrics::OVERVIEW
        };
        let tile = metrics.size();
        let cell = tile.h; // square cell = the taller (label) side

        let per_page = cols * rows;
        let n_pages = n.div_ceil(per_page);
        let page = self.current_page.min(n_pages - 1);

        // Distribute spacing + centering per axis, then place the current page.
        let (x_off, h_sp) =
            distribute(page_w, cols, cell, COL_SPACING, MAX_COL_SPACING, PAGE_PAD_H);
        let (y_off, v_sp) =
            distribute(page_h, rows, cell, ROW_SPACING, MAX_ROW_SPACING, PAGE_PAD_V);
        let origin_x = area.loc.x + x_off;
        let origin_y = area.loc.y + y_off;

        let first_index = page * per_page;
        let end = (first_index + per_page).min(n);
        let tiles: Vec<_> = (first_index..end)
            .map(|i| {
                let k = i - first_index;
                let (r, c) = (k / cols, k % cols);
                let cell_x = origin_x + c as f64 * (cell + h_sp);
                let cell_y = origin_y + r as f64 * (cell + v_sp);
                // Tile centered horizontally in its square cell; it fills vertically.
                let tx = (cell_x + (cell - tile.w) / 2.).round();
                Rectangle::new(Point::from((tx, cell_y.round())), tile)
            })
            .collect();

        let rows_used = (end - first_index).div_ceil(cols) as f64;
        let block_w = cols as f64 * cell + (cols as f64 - 1.) * h_sp;
        let block_h = rows_used * cell + (rows_used - 1.) * v_sp;
        let block = Rectangle::new(
            Point::from((origin_x.round(), origin_y.round())),
            Size::from((block_w, block_h)),
        );

        // The dots strip, centered along the bottom of the band.
        let indicators = (n_pages > 1).then(|| {
            let pitch = DOT_SIZE + 2. * DOT_PAD_SIDE;
            let total_w = pitch * n_pages as f64;
            let first_cx = area.loc.x + (area.size.w - total_w) / 2. + pitch / 2.;
            let cy = area.loc.y + area.size.h - INDICATORS_STRIP_H + DOT_PAD_TOP + DOT_SIZE / 2.;
            (0..n_pages)
                .map(|p| Point::from((first_cx + p as f64 * pitch, cy)))
                .collect()
        });

        // Navigation arrows, centered in each side gutter and vertically on the block
        // (`.page-navigation-arrow`; shown when a previous / next page exists). Absent
        // the ~10% `indicatorsPadding` reserve, they ride the grid's centering slack.
        let disc = |cx: f64| {
            let cy = block.loc.y + block.size.h / 2.;
            Rectangle::new(
                Point::from((
                    (cx - ARROW_DISC / 2.).round(),
                    (cy - ARROW_DISC / 2.).round(),
                )),
                Size::from((ARROW_DISC, ARROW_DISC)),
            )
        };
        let block_right = block.loc.x + block.size.w;
        let area_right = area.loc.x + area.size.w;
        let prev_arrow = (page > 0).then(|| disc(area.loc.x + (block.loc.x - area.loc.x) / 2.));
        let next_arrow =
            (page + 1 < n_pages).then(|| disc(block_right + (area_right - block_right) / 2.));

        GridLayout {
            tiles,
            block,
            metrics,
            first_index,
            n_pages,
            page,
            indicators,
            prev_arrow,
            next_arrow,
        }
    }

    /// The absolute entry index of the tile under `pos` (logical, output coords), if
    /// any, on the current page. The caller gates this on the grid being open and no
    /// search active.
    pub fn hit_test(
        &self,
        pos: Point<f64, Logical>,
        area: Rectangle<f64, Logical>,
    ) -> Option<usize> {
        let layout = self.layout(area);
        layout
            .tiles
            .iter()
            .position(|tile| tile.contains(pos))
            .map(|k| layout.first_index + k)
    }

    /// The page whose indicator dot is under `pos`, if any. The clickable target is
    /// the dot's padded box (`.page-indicator` padding `6 12 0`), not the 10px circle.
    pub fn indicator_hit(
        &self,
        pos: Point<f64, Logical>,
        area: Rectangle<f64, Logical>,
    ) -> Option<usize> {
        let layout = self.layout(area);
        let centers = layout.indicators.as_ref()?;
        centers.iter().position(|c| {
            (pos.x - c.x).abs() <= DOT_SIZE / 2. + DOT_PAD_SIDE
                && (pos.y - c.y).abs() <= DOT_SIZE / 2. + DOT_PAD_TOP
        })
    }

    /// The navigation arrow under `pos` (logical, output coords), if any. `Prev`/`Next`
    /// map to `current_page ∓ 1` for the caller (clamped by [`set_page`](Self::set_page)).
    pub fn arrow_hit(
        &self,
        pos: Point<f64, Logical>,
        area: Rectangle<f64, Logical>,
    ) -> Option<PageArrow> {
        let layout = self.layout(area);
        if layout.prev_arrow.is_some_and(|r| r.contains(pos)) {
            return Some(PageArrow::Prev);
        }
        if layout.next_arrow.is_some_and(|r| r.contains(pos)) {
            return Some(PageArrow::Next);
        }
        None
    }

    /// The logical center of the current page's tile `k` — a geometry probe for the
    /// conformance corpus (which clicks real pixels routed through
    /// [`hit_test`](Self::hit_test)).
    #[cfg(test)]
    pub fn tile_center(
        &self,
        k: usize,
        area: Rectangle<f64, Logical>,
    ) -> Option<Point<f64, Logical>> {
        self.layout(area)
            .tiles
            .get(k)
            .map(|t| Point::from((t.loc.x + t.size.w / 2., t.loc.y + t.size.h / 2.)))
    }

    /// The center of page `p`'s indicator dot — a geometry probe for the conformance
    /// corpus (which clicks real pixels routed through
    /// [`indicator_hit`](Self::indicator_hit)).
    #[cfg(test)]
    pub fn indicator_center(
        &self,
        p: usize,
        area: Rectangle<f64, Logical>,
    ) -> Option<Point<f64, Logical>> {
        self.layout(area).indicators.and_then(|c| c.get(p).copied())
    }

    /// The number of tiles drawn on the current page — a test probe.
    #[cfg(test)]
    pub fn visible_len(&self, area: Rectangle<f64, Logical>) -> usize {
        self.layout(area).tiles.len()
    }

    /// The center of a navigation arrow's disc — a geometry probe for the conformance
    /// corpus (which clicks real pixels routed through [`arrow_hit`](Self::arrow_hit)).
    #[cfg(test)]
    pub fn arrow_center(
        &self,
        arrow: PageArrow,
        area: Rectangle<f64, Logical>,
    ) -> Option<Point<f64, Logical>> {
        let layout = self.layout(area);
        let disc = match arrow {
            PageArrow::Prev => layout.prev_arrow,
            PageArrow::Next => layout.next_arrow,
        }?;
        Some(Point::from((
            disc.loc.x + disc.size.w / 2.,
            disc.loc.y + disc.size.h / 2.,
        )))
    }

    /// The grid render elements for `output`, into the `app_display` box, at `alpha`.
    /// Icons are pushed first (topmost within the grid); the tile chrome (wash +
    /// labels) bakes last (below the icons) — the dash/search order. The grid as a
    /// whole is pushed below the dash and search in `Niri::render`.
    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        app_icons: &AppIconCache,
        sym_icons: &IconCache,
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
        let metrics = layout.metrics;
        let first = layout.first_index;
        let page_entries = &self.entries[first..first + layout.tiles.len()];

        let mut cache = self.cache.borrow_mut();
        let context = renderer.context_id();
        if cache.context.as_ref() != Some(&context) {
            cache.icons.clear();
            cache.context = Some(context);
        }

        let mut elements = Vec::new();

        // --- App icons (topmost, over their tiles). ---
        for (k, entry) in page_entries.iter().enumerate() {
            let center = metrics.icon_center(layout.tiles[k]);
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
        let names: Vec<String> = page_entries.iter().map(|e| e.name.clone()).collect();
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
                for (k, (rel, label)) in rel_rects.iter().zip(labels.iter()).enumerate() {
                    let active = hovered == Some(first + k);
                    p.labelled_tile(*rel, label, &metrics, active, style::TEXT)?;
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

        // --- The page-indicator dots, below the grid (a single baked strip). ---
        if let Some(centers) = &layout.indicators {
            let page = layout.page;
            // Tight bounding box of the dot circles.
            let left = centers.first().unwrap().x - DOT_SIZE / 2.;
            let right = centers.last().unwrap().x + DOT_SIZE / 2.;
            let top = centers[0].y - DOT_SIZE / 2.;
            let dots_box = Rectangle::new(
                Point::from((left, top)),
                Size::from((right - left, DOT_SIZE)),
            );
            // Circle centers relative to the box.
            let rel: Vec<(f64, bool)> = centers
                .iter()
                .enumerate()
                .map(|(p, c)| (c.x - left, p == page))
                .collect();
            match widget::bake(
                renderer,
                &mut cache.dots_bake,
                scale,
                dots_box.size,
                self.content_rev,
                |_| Ok(()),
                move |frame, phys, _: &()| {
                    let mut p = Painter::new(frame, scale, phys);
                    p.clear(style::TRANSPARENT)?;
                    for (cx, is_active) in rel {
                        // The active dot is full 10px; the others scale to 2/3 and
                        // draw at half opacity (`pageIndicators.js`).
                        let r = if is_active {
                            DOT_SIZE / 2.
                        } else {
                            DOT_SIZE / 2. * INACTIVE_DOT_SCALE
                        };
                        let a = if is_active { 1. } else { INACTIVE_DOT_ALPHA };
                        let color = [style::TEXT[0], style::TEXT[1], style::TEXT[2], a];
                        let disc = Rectangle::new(
                            Point::from((cx - r, DOT_SIZE / 2. - r)),
                            Size::from((2. * r, 2. * r)),
                        );
                        p.fill_rounded(disc, r, color)?;
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
                        dots_box.loc,
                        alpha,
                        None,
                        None,
                        Kind::Unspecified,
                    ));
                }
                Err(err) => tracing::error!("error baking the app-grid dots: {err:#}"),
            }
        }

        // --- The page-navigation arrows (flat circular buttons in the side gutters). A
        //     hovered arrow gets the standard wash disc beneath its chevron. ---
        let scale_key = NotNan::new(scale).ok();
        for (is_next, disc) in [(false, layout.prev_arrow), (true, layout.next_arrow)] {
            let Some(disc) = disc else { continue };
            // The chevron glyph (topmost — pushed before its wash), its own upload cache.
            let name = if is_next {
                "carousel-arrow-next-symbolic"
            } else {
                "carousel-arrow-previous-symbolic"
            };
            if let (Some(scale_key), Some(buffer)) = (
                scale_key,
                sym_icons.buffer(name, ARROW_ICON_PX, scale, style::TEXT),
            ) {
                let key = (scale_key, is_next);
                #[allow(clippy::map_entry)]
                if !cache.arrow_icons.contains_key(&key) {
                    match TextureBuffer::from_memory_buffer(renderer, &buffer) {
                        Ok(tb) => {
                            cache.arrow_icons.insert(key, tb);
                        }
                        Err(err) => tracing::error!("error uploading nav arrow: {err:#}"),
                    }
                }
                if let Some(tb) = cache.arrow_icons.get(&key) {
                    let logical = tb.logical_size();
                    let center =
                        Point::from((disc.loc.x + disc.size.w / 2., disc.loc.y + disc.size.h / 2.));
                    let loc = center - Point::from((logical.w / 2., logical.h / 2.));
                    elements.push(TextureRenderElement::from_texture_buffer(
                        tb.clone(),
                        loc,
                        alpha,
                        None,
                        None,
                        Kind::Unspecified,
                    ));
                }
            }
        }
        // The hover wash disc, beneath the hovered arrow's chevron (a constant bake,
        // repositioned). Only one arrow is hovered at a time.
        if let Some(disc) = self.hovered_arrow.and_then(|a| match a {
            PageArrow::Prev => layout.prev_arrow,
            PageArrow::Next => layout.next_arrow,
        }) {
            let size = disc.size;
            match widget::bake(
                renderer,
                &mut cache.arrow_bake,
                scale,
                size,
                0,
                |_| Ok(()),
                move |frame, phys, _: &()| {
                    let mut p = Painter::new(frame, scale, phys);
                    p.clear(style::TRANSPARENT)?;
                    let full = Rectangle::from_size(size);
                    p.fill_rounded(full, ARROW_DISC / 2., style::HOVER_WASH)?;
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
                        disc.loc,
                        alpha,
                        None,
                        None,
                        Kind::Unspecified,
                    ));
                }
                Err(err) => tracing::error!("error baking the nav-arrow wash: {err:#}"),
            }
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

    fn grid_n(n: usize) -> AppGrid {
        let mut g = AppGrid::new();
        g.set_entries(
            (0..n)
                .map(|i| entry(&format!("app{i:02}.desktop"), &format!("App {i:02}")))
                .collect(),
        );
        g
    }

    fn rect(x: f64, y: f64, w: f64, h: f64) -> Rectangle<f64, Logical> {
        Rectangle::new(Point::from((x, y)), Size::from((w, h)))
    }

    /// A landscape band the size of a 1920×1080 work area's app-display: 8 columns ×
    /// 3 rows, 96px icons, one page for 24 apps.
    fn wide() -> Rectangle<f64, Logical> {
        rect(0., 0., 1920., 550.)
    }

    #[test]
    fn wide_band_lays_eight_columns_at_ninety_six_px() {
        let l = grid_n(24).layout(wide());
        assert_eq!(l.n_pages, 1);
        assert_eq!(l.tiles.len(), 24);
        // First eight tiles share row 0; the ninth starts row 1 → 8 columns.
        assert!((0..8).all(|i| l.tiles[i].loc.y == l.tiles[0].loc.y));
        assert!(l.tiles[8].loc.y > l.tiles[0].loc.y);
        // 96px icon → 120-wide tile; a single page shows no dots.
        assert_eq!(l.tiles[0].size.w, 120.);
        assert!(l.indicators.is_none());
    }

    #[test]
    fn paginates_and_navigates_pages() {
        let mut g = grid_n(30);
        let area = wide();
        assert_eq!(g.page_count(area), 2);
        // Page 0 holds a full 24; page 1 the remaining 6.
        let l0 = g.layout(area);
        assert_eq!((l0.first_index, l0.tiles.len()), (0, 24));
        assert!(g.set_page(1, area));
        let l1 = g.layout(area);
        assert_eq!((l1.page, l1.first_index, l1.tiles.len()), (1, 24, 6));
    }

    #[test]
    fn hit_test_returns_the_absolute_index_on_a_later_page() {
        let mut g = grid_n(30);
        let area = wide();
        g.set_page(1, area);
        // The first tile on page 1 is entry 24.
        let c = g.tile_center(0, area).unwrap();
        assert_eq!(g.hit_test(c, area), Some(24));
        assert_eq!(g.hit_test(Point::from((-100., -100.)), area), None);
    }

    #[test]
    fn indicators_track_the_pages() {
        let g = grid_n(30);
        let area = wide();
        let dots = g.layout(area).indicators.unwrap();
        assert_eq!(dots.len(), 2);
        // The padded box around a dot is clickable → its page.
        assert_eq!(g.indicator_hit(dots[1], area), Some(1));
        // A single page shows no dots.
        assert!(grid_n(10).layout(area).indicators.is_none());
    }

    #[test]
    fn arrows_appear_only_beside_a_neighbouring_page() {
        let mut g = grid_n(30);
        let area = wide();
        // Page 0: no previous, a next.
        let l0 = g.layout(area);
        assert!(l0.prev_arrow.is_none());
        assert!(l0.next_arrow.is_some());
        // Last page: a previous, no next.
        g.set_page(1, area);
        let l1 = g.layout(area);
        assert!(l1.prev_arrow.is_some());
        assert!(l1.next_arrow.is_none());
        // A single page shows neither.
        let l = grid_n(10).layout(area);
        assert!(l.prev_arrow.is_none() && l.next_arrow.is_none());
    }

    #[test]
    fn arrow_hit_steps_the_page() {
        let mut g = grid_n(30);
        let area = wide();
        let next = g.arrow_center(PageArrow::Next, area).unwrap();
        assert_eq!(g.arrow_hit(next, area), Some(PageArrow::Next));
        // The next arrow sits to the right of the grid block; the prev arrow is absent.
        assert!(g.arrow_center(PageArrow::Prev, area).is_none());
        // Clicking through it advances, and the prev arrow then appears and returns.
        g.set_page(1, area);
        let prev = g.arrow_center(PageArrow::Prev, area).unwrap();
        assert_eq!(g.arrow_hit(prev, area), Some(PageArrow::Prev));
        assert_eq!(g.arrow_hit(Point::from((-100., -100.)), area), None);
    }

    #[test]
    fn set_page_clamps_and_resets() {
        let mut g = grid_n(30);
        let area = wide();
        assert!(g.set_page(5, area)); // clamps to the last page (1)
        assert_eq!(g.current_page(), 1);
        assert!(!g.set_page(5, area)); // already there → no move
        assert!(g.reset_page());
        assert_eq!(g.current_page(), 0);
    }

    #[test]
    fn narrow_tall_band_picks_more_rows_and_shrinks_the_icon() {
        // A portrait band picks the 3×8 mode and drops below 96px to fit.
        let l = grid_n(24).layout(rect(0., 0., 400., 1000.));
        assert!((0..3).all(|i| l.tiles[i].loc.y == l.tiles[0].loc.y));
        assert!(l.tiles[3].loc.y > l.tiles[0].loc.y);
        assert!(l.tiles[0].size.w < 120.);
    }

    #[test]
    fn empty_when_no_apps_or_no_room() {
        assert_eq!(grid_n(0).visible_len(wide()), 0);
        // A band too small for the padding + one tile.
        assert_eq!(grid_n(1).layout(rect(0., 0., 20., 20.)).tiles.len(), 0);
    }

    #[test]
    fn stale_hover_is_dropped_on_shrink() {
        let mut g = grid_n(3);
        assert!(g.set_hovered(Some(2)));
        assert!(g.set_entries(vec![entry("a", "a")]));
        assert_eq!(g.hovered, None);
    }
}
