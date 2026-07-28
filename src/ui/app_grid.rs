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
//! (`appDisplay.js:1492-1504`). Order is each app's saved `(page, position)` in
//! `org.gnome.shell app-picker-layout`, with everything unplaced appended by name
//! (`_compareItems`, `appDisplay.js:1475-1490`; the sort itself lives in
//! `Niri::sync_app_grid`). A click launches the app and closes the overview
//! (`AppIcon.activate` → `Main.overview.hide`, `appDisplay.js:3060,3077`). A caption
//! too long for its box is ellipsized, and expands to the whole name wrapped while the
//! tile is highlighted (`_updateMultiline`, `appDisplay.js:1891-1924`).
//!
//! **Paginated layout (`IconGrid`, `iconGrid.js`).** The page mode `(columns, rows)`
//! is the `defaultGridModes` entry (`{3×8,4×6,6×4,8×3}`, `iconGrid.js:30-47`) whose
//! aspect ratio is closest to the page's (`_findBestModeForSize`); a wide `app_display`
//! picks 8×3. The icon shrinks to the largest `IconSize` whose square cells fit
//! (`_findBestIconSize`, tiles laid in a `max(w,h)` square cell). Column/row spacing
//! grows from `.icon-grid`'s 12 to a max of 36 to absorb slack, then the remainder
//! centers the page (`_calculateSpacing`, FILL). A band of [`indicators_w`] — 10% of
//! the width, floored at an arrow — is reserved on each side *before* that
//! (`indicatorsPadding`, `appDisplay.js:162-171,405-430`); it holds the navigation
//! arrows and is where the adjacent pages peek in during a drag. Overflow paginates: a
//! dots row below the grid (`.page-indicator`, 10px, inactive at 2/3 scale + half
//! opacity) plus flat circular **navigation arrows** in those bands
//! (`.page-navigation-arrow`, `carousel-arrow-{previous,next}-symbolic`,
//! `appDisplay.js:553-575`; shown when a previous/next page exists,
//! `appDisplay.js:255-302`). Either dot, arrow, a wheel notch (debounced 150ms), or a
//! reset to page 0 on a fresh overview open (`'hidden'` → `goToPage(0)`,
//! `appDisplay.js:1342`) changes the page.
//!
//! **Drag-reorder.** Dragging an icon within the grid moves it: the pointer resolves
//! to a `(page, position)` insertion point ([`AppGrid::drop_target_at`]), the grid
//! reflows around it once the target has held still for [`DELAYED_MOVE_MS`], and the
//! drop persists the arrangement to `app-picker-layout` (`_savePages`). A drag nobody
//! accepted puts the order back.
//!
//! **Page previews.** While any overview item drag is in flight the two reserved side
//! bands fade in as `.page-navigation-hint` gradients and the adjacent pages' tiles
//! slide into them ([`AppGrid::set_drag_active`], `_syncPageIndicators`,
//! `appDisplay.js:364-397`). Hovering a band for [`PAGE_SWITCH_INITIAL_MS`] flips the
//! page and then keeps flipping every [`PAGE_SWITCH_REPEAT_MS`]; bumping the pointer
//! within [`EDGE_BUMP_PX`] of the grid's edge flips it at once. Dropping *on* a band
//! sends the app to that page, creating one past the end.
//!
//! **Divergences, revisited later.** No page-slide animation (snap),
//! no touchpad **swipe** (continuous scroll over the grid is consumed but inert — the
//! 1:1 swipe is deferred), and no keyboard paging (`Page_Up/Down`). Folders are read
//! only: a folder takes a grid slot, hides its members from the top level and draws
//! them as a raised `.app-folder` tile ([`AppGridEntry::folder`]) that opens
//! [`crate::ui::folder_dialog`] on a click, but nothing here creates, renames or edits
//! one — a drag can never make a folder, and dropping an app on one does nothing.
//! (The one write is the once-per-profile default-folder seed,
//! [`crate::gnome::GnomeSettingsWriter::ensure_default_folders`], which is what makes
//! any folder exist at all on a profile that never ran gnome-shell.) A folder
//! *dragged* carries the fallback icon rather than its
//! own composition, since a drag proxy is one [`AppIconRef`]. Its hover uses the grid's
//! shared [`style::HOVER_WASH`] (10% white) where GNOME lightens the raised fill 4%;
//! about 5/255 apart. Pages here are always
//! **full**: our order is a flat list chunked by the page size, where GNOME's grid is
//! built with `allow_incomplete_pages: true` (`appDisplay.js:655`) and can leave holes.
//! The name fallback sort is a case-folded `to_lowercase` compare rather
//! than full locale collation (`localeCompare`): std has no collator, so accented
//! initials can misplace; an `icu` collator is the faithful fix. An expanded caption is
//! capped at [`widget::TILE_LABEL_EXPAND_LINES`] lines (GNOME grows the tile without a
//! limit) and it does not animate open. Like the dash and
//! search, the grid draws on **every** output with one shared hover/page (GNOME shows
//! it on the primary only); hit-testing stays per-output.

use std::cell::RefCell;

use ordered_float::NotNan;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::{ContextId, Renderer};
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Size, Transform};

use crate::animation::{Animation, Clock, Curve};
use crate::app_system::AppIconRef;
use crate::render_helpers::icon::{AppIconCache, IconCache};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::ui::widget::{self, style, AppIconUploads, Painter, Rgba, TileMetrics};

/// Grid tile label point size, shared with the search results.
///
/// `.overview-tile` sets **no** `font-size` (`_app-grid.scss:21-37`), and neither does
/// the `.overview-icon` inside it, so an app name renders at the inherited stage size —
/// `$base_font_size`. It is not a `%caption`; that this said 10pt (and cited `%caption`,
/// which is 9pt anyway) made every app name ~9% short of GNOME's.
const LABEL_PT: f64 = crate::ui::BASE_FONT_PT;

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
/// The one mode a folder's inner grid offers (`FolderGrid.setGridModes`,
/// `appDisplay.js:2077-2082`) — a folder never re-flows to the box it is given.
const FOLDER_GRID_MODES: [(usize, usize); 1] = [(3, 3)];
/// The icon-size ladder, largest first (`IconSize`, `iconGrid.js:16-23`); the grid
/// shrinks the icon to the largest size whose page fits (`_findBestIconSize`).
const ICON_SIZES: [f64; 6] = [96., 64., 48., 32., 24., 16.];
/// A labelled `.overview-tile` is `icon + 48` **square** (padding 12, icon, gap 6,
/// label 18, padding 12 — see [`TileMetrics::size`]), so the square cell the grid lays
/// it in (`iconGrid.js` `_getChildrenMaxSize` = `max(w, h)`) is exactly the tile.
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
/// `.page-navigation-arrow` `margin: $base_padding` — so the band an arrow needs is
/// the disc plus a margin each side.
const ARROW_MARGIN: f64 = 6.;

/// Leeway at each tile edge within which a drag drops *between* icons rather than on
/// one (`LEFT_DIVIDER_LEEWAY` / `RIGHT_DIVIDER_LEEWAY`, `iconGrid.js:49-50`).
const DIVIDER_LEEWAY: f64 = 20.;

/// How long a drop target must hold still before the grid reflows around it
/// (`DELAYED_MOVE_TIMEOUT`, `appDisplay.js:55`). The reflow is live and provisional —
/// the drop commits it, a drag that ends elsewhere throws it away.
pub const DELAYED_MOVE_MS: u64 = 200;

/// Bump the pointer within this many px of the grid's edge during a drag and the page
/// switches at once (`DRAG_PAGE_SWITCH_IMMEDIATELY_THRESHOLD_PX`, `appDisplay.js:51`).
/// It is also the distance the pointer must come back inside before another bump
/// counts, so leaning on the edge switches once, not continuously.
pub const EDGE_BUMP_PX: f64 = 20.;
/// Hovering a hint band this long switches the page
/// (`DRAG_PAGE_SWITCH_INITIAL_TIMEOUT`, `appDisplay.js:50`).
pub const PAGE_SWITCH_INITIAL_MS: u64 = 1000;
/// …and it keeps switching at this interval afterwards, from either mechanism
/// (`DRAG_PAGE_SWITCH_REPEAT_TIMEOUT`, `appDisplay.js:53`).
pub const PAGE_SWITCH_REPEAT_MS: u64 = 1000;

/// How long the page previews take to slide in and out (`PAGE_PREVIEW_ANIMATION_TIME`,
/// `appDisplay.js:45`; `EASE_OUT_CUBIC`, `appDisplay.js:445-448`).
const PAGE_PREVIEW_MS: u64 = 150;

/// `.page-navigation-hint`'s gradient stop (`_app-grid.scss:152,157`): `$system_fg_color`
/// at 5%, fading to transparent toward the screen edge.
const HINT_COLOR: [f32; 4] = [1., 1., 1., 0.05];
/// The same band while a drag is over it (`.page-navigation-hint.dnd`,
/// `_app-grid.scss:151-153`) — a flat 10% fill, no gradient.
const HINT_DND_COLOR: [f32; 4] = [1., 1., 1., 0.1];
/// `$modal_radius * 1.5` (`_app-grid.scss:160,168`), i.e. `$base_border_radius * 2 * 1.5`
/// = 24 (`_common.scss:33,40`). Only the band's **inner** corners are cut; see
/// [`widget::Painter::fill_rounded_faded`] for how that is expressed.
const HINT_RADIUS: f64 = 24.;

/// How many members a folder tile composes into its icon — the fixed `i < 4` loop of
/// `createFolderIcon` (`appDisplay.js:2153`). A folder with fewer simply leaves cells
/// empty; one with more shows no hint that it has them, same as GNOME.
const FOLDER_SUBICONS: usize = 4;

/// `PAGE_SWITCH_TIME` (`iconGrid.js:13`) — how long the view takes to slide one page.
const PAGE_SWITCH_MS: u64 = 300;

/// A **touchpad** page is `TOUCHPAD_BASE_WIDTH` of travel (`swipeTracker.js:14`, passed as
/// the gesture `distance` at `:183`). Clutter cannot ask libinput for the real touchpad
/// size, so GNOME picks a fixed value and every GTK app agrees on it.
///
/// This is touchpad-*only*. A pointer drag divides by `SwipeTracker.distance`
/// (`_updatePanGesture`, `:578-585`), which `_swipeBegin` sets to the grid's own
/// allocation width (`appDisplay.js:713-716`, `swipeTracker.js:710-711`) — so a drag is
/// one *page width* per page, i.e. actually 1:1 with the content under the pointer.
const SWIPE_TOUCHPAD_PAGE_PX: f64 = 400.;
/// `SCROLL_MULTIPLIER` (`swipeTracker.js:18`) — a two-finger scroll delta is scaled up
/// before it counts as gesture travel.
pub const SWIPE_SCROLL_MULTIPLIER: f64 = 10.;
/// `VELOCITY_THRESHOLD_TOUCHPAD` / `VELOCITY_THRESHOLD_TOUCH` (`swipeTracker.js:22-23`) —
/// in **pixels** per millisecond, not pages: the velocity history holds raw deltas
/// (`:597,676`) and the threshold is compared against them before the normalization at
/// `:644`. Below it a release is a slow drag and simply falls to whichever page is
/// nearest. A pointer or touch drag gets the lower bar, because it is real travel rather
/// than a scroll delta scaled by [`SWIPE_SCROLL_MULTIPLIER`].
const SWIPE_VELOCITY_THRESHOLD_TOUCHPAD: f64 = 0.6;
const SWIPE_VELOCITY_THRESHOLD_TOUCH: f64 = 0.3;

/// Where a live page swipe is coming from — `SwipeTracker` runs a `TouchpadSwipeGesture`
/// and a `Clutter.PanGesture` side by side over the same state (`swipeTracker.js:383-404`),
/// and the two differ in how a release is judged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwipeSource {
    /// Two-finger scrolling: deltas are scaled by [`SWIPE_SCROLL_MULTIPLIER`].
    Touchpad,
    /// A pointer (or touch) drag: 1:1 with real travel, and `allowDrag` defaults to true
    /// with `min_n_points: 1`, which is what makes a plain click-drag page the grid.
    Pointer,
}
/// `MIN_ANIMATION_DURATION` / `MAX_ANIMATION_DURATION` (`swipeTracker.js:20-21`). The max
/// is `400 * log2(1 + nPoints)` and a swipe can only ever cross one page here, so it is
/// exactly 400.
const SWIPE_MIN_MS: u64 = 100;
const SWIPE_MAX_MS: u64 = 400;
/// `DURATION_MULTIPLIER` (`swipeTracker.js:31`) — the derivative of `easeOutCubic` at 0,
/// which is what makes the settle continue at the speed the finger left off.
const SWIPE_DURATION_MULTIPLIER: f64 = 3.;

/// Share of the band reserved for the two page-preview strips (`PAGE_PREVIEW_RATIO`,
/// `appDisplay.js:47`) — half of it on each side.
const PAGE_PREVIEW_RATIO: f64 = 0.20;

/// Width of one reserved side band (`_getIndicatorsWidth`, `appDisplay.js:221-237`):
/// the preview share, but never narrower than a navigation arrow.
///
/// This is `AppGrid.indicatorsPadding` and it is **permanent**, not drag-only — it is
/// *added* to the `.icon-grid` page padding (`_updatePadding`, `appDisplay.js:162-171`),
/// so the grid content box is always inset by it, the navigation arrows sit inside it
/// rather than in the grid's centering slack, and it is the room the adjacent page's
/// icons slide into when a drag makes the previews appear.
fn indicators_w(band_w: f64) -> f64 {
    (band_w * PAGE_PREVIEW_RATIO / 2.).max(ARROW_DISC + 2. * ARROW_MARGIN)
}

/// What a page does with the space its cells do not fill (`pageHalign`/`pageValign`,
/// applied by `_calculateSpacing`, `iconGrid.js:591-635`). Both axes of one grid use
/// the same value in GNOME's two grids, so this is one knob rather than two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageAlign {
    /// The slack is handed to the *spacing*, which grows from its base up to the max
    /// and only then centers what is left — the app grid's default
    /// (`IconGrid._init`, `iconGrid.js:1171`).
    Fill,
    /// The spacing stays at its base and the slack becomes leading padding, so the
    /// cells sit as a centered block (`FolderGrid`, `appDisplay.js:2073-2074`).
    Center,
}

/// `.overview-tile:focus`'s ring width — `focus_ring()`'s `box-shadow: inset 0 0 0 2px`
/// (`_drawing.scss:57-66`).
const FOCUS_RING_W: f64 = 2.;
/// …and its color's alpha: `st-transparentize($accent_color, $focus_border_opacity)` with
/// `$focus_border_opacity: .2` (`_default-colors.scss:41`).
const FOCUS_RING_ALPHA: f32 = 0.8;

/// The focused tile's background — `focus_bg_color(transparentize($system_base_color, .75))`
/// for a flat `tile_button` (`_drawing.scss:317-323`), i.e. `st-mix($accent, rgba(#222226,
/// .25), 5%)`. `st-mix` is St's own premultiplied LERP toward the *second* color
/// (`st-theme-node.c:637-693`), not Sass's alpha-weighted `mix()`, so it is spelled out
/// here rather than approximated. `ring` supplies the accent RGB (its alpha is ignored).
fn focus_bg(ring: Rgba) -> Rgba {
    // `$system_base_color` #222226 at 25% — `transparentize` only subtracts alpha.
    const BASE: Rgba = [0.133, 0.133, 0.149, 0.25];
    // `factor = 1 - 0.05`: 5% of the accent, 95% of the base.
    const F: f32 = 0.95;
    let a = 1. + (BASE[3] - 1.) * F;
    let ch = |i: usize| (ring[i] + (BASE[i] * BASE[3] - ring[i]) * F) / a;
    [ch(0), ch(1), ch(2), a]
}

/// A keyboard navigation direction — St's four spatial directions
/// (`StDirectionType` `ST_DIR_UP`/`DOWN`/`LEFT`/`RIGHT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusDir {
    Left,
    Right,
    Up,
    Down,
}

/// Which navigation arrow — the previous (left) or next (right) page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageArrow {
    Prev,
    Next,
}

/// Where a drag sits relative to the tile under it (`DragLocation`,
/// `iconGrid.js:53-59`; `INVALID` is our `None`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragLocation {
    /// Within [`DIVIDER_LEEWAY`] of the tile's leading edge — insert before it.
    StartEdge,
    /// Over the body of a tile — not an insertion point; the grid does not reflow.
    OnIcon,
    /// Within [`DIVIDER_LEEWAY`] of the tile's trailing edge — insert after it.
    EndEdge,
    /// Past the last tile of the page — append to it.
    EmptySpace,
}

/// A resolved drop target inside the grid ([`AppGrid::drop_target_at`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridDropTarget {
    pub page: usize,
    /// Position within the page. `None` is GNOME's `-1` — append (`EmptySpace`).
    pub position: Option<usize>,
    pub location: DragLocation,
}

/// One grid slot — a plain-data snapshot (not a live catalog borrow), like
/// [`crate::ui::dash::DashEntry`]. It is an app, or a *folder* of apps: GNOME's
/// `_redisplay` pushes `FolderIcon`s into the same list as `AppIcon`s
/// (`appDisplay.js:1508-1533`), so a folder sorts and drags exactly like an app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppGridEntry {
    /// A desktop id for an app; a `folder-children` id for a folder. Both share the
    /// `app-picker-layout` id space.
    pub id: String,
    pub name: String,
    /// The app's icon. A folder's tile does *not* draw this — it composes its members'
    /// icons ([`Self::folder`]) — so for a folder it is only the single-icon proxy a
    /// drag of it carries.
    pub icon: AppIconRef,
    /// `Some` for a folder: its members in display order, resolved by
    /// [`crate::app_system::AppSystem::folder_members`]. Never empty — an empty
    /// folder is not displayed at all (`appDisplay.js:1523-1527`).
    pub folder: Option<Vec<AppGridEntry>>,
}

#[derive(Default)]
struct GridCache {
    context: Option<ContextId<VkTexture>>,
    /// The page chrome + short captions, **one bake per page**: a slide has two pages on
    /// screen at once, so a single texture would have them fighting over it.
    bakes: std::collections::HashMap<usize, widget::BakeCache>,
    /// The page-indicator dots row.
    dots_bake: widget::BakeCache,
    /// The tile hover wash — a separate element so a hover change repositions it without
    /// re-baking (re-shaping) the labels (keeps the open animation smooth under the mouse).
    hover_bake: widget::BakeCache,
    /// The `.overview-tile:focus` ring, likewise one tile's worth, moved as focus moves.
    focus_bake: widget::BakeCache,
    /// Captions too long for one line, one bake each, keyed by `(page, page-relative tile)`.
    ///
    /// They cannot live in the page bake: the hovered one expands to the full wrapped
    /// name (`_updateMultiline`, `appDisplay.js:1891-1924`), so the page bake would
    /// depend on the hover again — the re-shape-every-frame stutter `c5336421` removed.
    /// Giving each its own element means a hover change re-bakes one caption-sized
    /// texture and leaves the other ~23 labels alone. Names that fit stay in the page
    /// bake, so a typical page adds only a handful of elements.
    long_labels: std::collections::HashMap<(usize, usize), widget::BakeCache>,
    /// The resting backgrounds of the page's folder tiles — `.app-folder` is the one
    /// *raised* tile in the grid, so unlike an app tile it has a fill at rest. Its own
    /// element, below `hover_bake`, so a hovered folder still lightens (an opaque fill
    /// baked into the page texture would sit on top of the wash and swallow it). One per
    /// page, like [`Self::bakes`].
    folder_bakes: std::collections::HashMap<usize, widget::BakeCache>,
    /// The resting background of the ONE tile that is fading (the folder whose dialog is
    /// open), kept apart from `folder_bakes` so its alpha can move per frame without
    /// touching a bake shared with the rest of the page.
    fade_bake: widget::BakeCache,
    /// The (constant) navigation-arrow hover-wash disc.
    arrow_bake: widget::BakeCache,
    /// The two page-preview hint bands, previous then next.
    hint_bakes: [widget::BakeCache; 2],
    /// Captions of the pages peeking in during a drag, one bake per page — dropped
    /// when the previews go away, since they exist only for the length of a drag.
    peek_bakes: std::collections::HashMap<usize, widget::BakeCache>,
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
    /// The keyboard-focused tile (an absolute entry index) — drives the
    /// `.overview-tile:focus` ring, and expands its caption exactly like hover does
    /// (`appDisplay.js:1901`). Independent of [`Self::hovered`]: moving the pointer
    /// never moves key focus in GNOME, and both can be lit at once.
    focused: Option<usize>,
    /// The current page (`AppDisplay` paginates; `iconGrid.js`). Clamped to the page
    /// count at layout time; reset to 0 on a fresh overview open.
    ///
    /// This is the **destination**: `goToPage` assigns `_currentPage` up front and only
    /// eases the view after it (`iconGrid.js:1364-1377`), so hit-testing, key focus and
    /// drop targets all follow the page being moved to, mid-slide included.
    current_page: usize,
    /// Where the view actually is, in pages — GNOME's scroll adjustment, whose value
    /// divided by the page size is a *fractional* page (`appDisplay.js:721-724`). One
    /// continuous quantity so the slide animation and a swipe are the same state, driven
    /// from either end. `None` while a gesture holds it directly.
    slide: Animation,
    gesture: Option<f64>,
    /// Where a live gesture started, in pages. A swipe may cross **at most one** page:
    /// `_getBounds` clamps the live progress to the snap points either side of where the
    /// gesture began unless `allowLongSwipes` is set, and `AppDisplay` does not set it
    /// (`swipeTracker.js:547-577`).
    gesture_from: f64,
    /// Velocity history for the release projection (`swipeTracker.js:601-631`).
    swipe: crate::input::swipe_tracker::SwipeTracker,
    swipe_source: SwipeSource,
    /// How much travel makes one page for the gesture in flight — `SwipeTracker.distance`.
    swipe_distance: f64,
    /// Bumped on any change that affects the bake (entries/hover/page).
    content_rev: u64,
    /// The order as it was when a drag started, so an unsuccessful drop can put it
    /// back — the live reflow is provisional (`_onDragCancelled`, `appDisplay.js:979`).
    reorder_restore: Option<Vec<String>>,
    /// 0→1 while an item drag is in flight: how far the page previews have slid in
    /// (`_pageIndicatorsAdjustment`, `appDisplay.js:441-468`). Shown for *any* overview
    /// item drag, not only a grid one — the bands are also how an icon reaches a page
    /// it can't see.
    peek: Animation,
    /// Which hint band the drag is over, if any — the `.dnd` flat fill.
    hint_hovered: Option<PageArrow>,
    /// A tile that must draw at reduced opacity, and how much: the folder whose dialog is
    /// up fades its source tile out while the dialog zooms out of it and back in as it
    /// shrinks home (`appDisplay.js:2441-2451`). The *id* is folded into the bake
    /// revision, the alpha deliberately is not — see [`Self::set_tile_fade`].
    tile_fade: Option<(String, f64)>,
    /// The page modes this grid may re-flow to, and what a page does with its slack.
    ///
    /// GNOME builds the folder's inner view from the *same* `AppGrid` as the top-level
    /// one — `FolderGrid extends AppGrid` (`appDisplay.js:2066-2084`) — differing only
    /// in these two parameters, so the folder dialog reuses this whole widget (hover,
    /// captions, pagination, dots, arrows, icon uploads) instead of re-deriving it.
    modes: &'static [(usize, usize)],
    align: PageAlign,
    clock: Clock,
    cache: RefCell<GridCache>,
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
    /// Columns per page, and the distributed cell spacing on each axis — the drop
    /// target reads all three (a hit is allowed half a spacing beyond a tile).
    cols: usize,
    h_sp: f64,
    v_sp: f64,
    /// Tiles per page (`cols * rows`), which is what a `(page, position)` pair means.
    per_page: usize,
    /// Total page count (0 when there are no apps).
    n_pages: usize,
    /// The page these `tiles` belong to (clamped `current_page`).
    page: usize,
    /// The dot centers below the grid, one per page — `None` when `n_pages <= 1`.
    indicators: Option<Vec<Point<f64, Logical>>>,
    /// The reserved side bands, previous then next ([`indicators_w`]) — the navigation
    /// arrows sit in them and the page previews slide into them.
    hints: [Rectangle<f64, Logical>; 2],
    /// The previous-page arrow's disc box — `Some` only when a previous page exists.
    prev_arrow: Option<Rectangle<f64, Logical>>,
    /// The next-page arrow's disc box — `Some` only when a next page exists.
    next_arrow: Option<Rectangle<f64, Logical>>,
}

/// The spacing distribution for one axis (`iconGrid.js` `_calculateSpacing`).
///
/// Under [`PageAlign::Fill`] the inter-cell spacing grows from `base` to absorb the
/// slack, and once it hits `max` the remaining slack centers the run; under
/// [`PageAlign::Center`] the spacing stays at `base` and *all* the slack becomes
/// leading padding. Returns `(origin_offset, spacing)` where the offset is measured
/// from the page edge (it already includes `pad`).
fn distribute(
    page_size: f64,
    n: usize,
    cell: f64,
    base: f64,
    max: f64,
    pad: f64,
    align: PageAlign,
) -> (f64, f64) {
    if align == PageAlign::Center {
        // `leftEmptySpace += Math.floor(emptyHSpace / 2)`, `hSpacing = columnSpacing`.
        let nf = n as f64;
        let empty = page_size - cell * nf - base * (nf - 1.) - 2. * pad;
        return (pad + (empty / 2.).floor().max(0.), base);
    }
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
    pub fn new(clock: Clock) -> Self {
        Self::with_modes(clock, &GRID_MODES, PageAlign::Fill)
    }

    /// A folder's inner grid — `FolderGrid` (`appDisplay.js:2066-2084`): the one 3×3
    /// mode, its cells centered as a block rather than spread to fill the page.
    pub fn folder_view(clock: Clock) -> Self {
        Self::with_modes(clock, &FOLDER_GRID_MODES, PageAlign::Center)
    }

    fn with_modes(clock: Clock, modes: &'static [(usize, usize)], align: PageAlign) -> Self {
        Self {
            entries: Vec::new(),
            hovered: None,
            hovered_arrow: None,
            focused: None,
            current_page: 0,
            slide: Animation::ease(clock.clone(), 0., 0., 0., 0, Curve::EaseOutCubic),
            gesture: None,
            gesture_from: 0.,
            swipe: crate::input::swipe_tracker::SwipeTracker::new(),
            swipe_source: SwipeSource::Touchpad,
            swipe_distance: SWIPE_TOUCHPAD_PAGE_PX,
            content_rev: 0,
            reorder_restore: None,
            peek: Animation::ease(clock.clone(), 0., 0., 0., 0, Curve::EaseOutCubic),
            hint_hovered: None,
            tile_fade: None,
            modes,
            align,
            clock,
            cache: RefCell::new(GridCache::default()),
        }
    }

    /// Slide the page previews in or out (`showPageIndicators` / `hidePageIndicators`,
    /// `appDisplay.js:441-468`). Returns whether it started moving (→ redraw).
    pub fn set_drag_active(&mut self, active: bool) -> bool {
        let to = if active { 1. } else { 0. };
        if self.peek.to() == to {
            return false;
        }
        self.peek = Animation::ease(
            self.clock.clone(),
            self.peek.value(),
            to,
            0.,
            PAGE_PREVIEW_MS,
            Curve::EaseOutCubic,
        );
        if !active {
            self.hint_hovered = None;
        }
        true
    }

    /// Mark which hint band a drag is over — its `.dnd` fill. Returns whether it
    /// changed (→ redraw).
    pub fn set_hint_hovered(&mut self, hint: Option<PageArrow>) -> bool {
        if self.hint_hovered == hint {
            return false;
        }
        self.hint_hovered = hint;
        true
    }

    /// The hint band `pos` is inside, if the previews are showing and that band leads
    /// anywhere. The *next* band is live even on the last page — dropping there is how
    /// a new page gets made (`appDisplay.js:270-274`).
    pub fn hint_at(
        &self,
        pos: Point<f64, Logical>,
        area: Rectangle<f64, Logical>,
    ) -> Option<PageArrow> {
        if self.peek.value() <= 0. {
            return None;
        }
        let layout = self.layout(area);
        if layout.n_pages == 0 {
            return None;
        }
        if layout.page > 0 && layout.hints[0].contains(pos) {
            return Some(PageArrow::Prev);
        }
        layout.hints[1].contains(pos).then_some(PageArrow::Next)
    }

    /// Whether the preview slide or a page slide is still running (→ hold the redraw
    /// loop open).
    pub fn are_animations_ongoing(&self) -> bool {
        !self.peek.is_done() || (self.gesture.is_none() && !self.slide.is_done())
    }

    /// Replace the grid's apps (`AppDisplay._redisplay`). Returns whether anything
    /// changed (→ redraw). The caller sorts; this stores verbatim.
    pub fn set_entries(&mut self, entries: Vec<AppGridEntry>) -> bool {
        if self.entries == entries {
            return false;
        }
        self.entries = entries;
        // Drop a now-stale hover / key focus rather than lighting the wrong tile.
        if self.hovered.is_some_and(|i| i >= self.entries.len()) {
            self.hovered = None;
        }
        if self.focused.is_some_and(|i| i >= self.entries.len()) {
            self.focused = None;
        }
        self.content_rev += 1;
        true
    }

    /// Drop cached icon uploads (icon-theme / installed change).
    pub fn clear_icon_uploads(&self) {
        self.cache.borrow_mut().icons.clear();
    }

    /// Drop one icon's uploads, so the next frame re-uploads it from the freshly
    /// decoded pixels — see [`widget::drop_app_icon_upload`].
    pub fn drop_icon_upload(&self, icon: &crate::app_system::AppIconRef, logical_px: u16) {
        crate::ui::widget::drop_app_icon_upload(
            &mut self.cache.borrow_mut().icons,
            icon,
            logical_px,
        );
    }

    /// The id of tile `i`, if present (what a click launches).
    pub fn entry_id(&self, i: usize) -> Option<&str> {
        self.entries.get(i).map(|e| e.id.as_str())
    }

    /// The index of the entry with `id`, if it is in the grid — how the folder dialog
    /// finds the tile it must zoom out of.
    pub fn index_of(&self, id: &str) -> Option<usize> {
        self.entries.iter().position(|e| e.id == id)
    }

    /// The display name of tile `i` — a folder's is the title its dialog carries.
    pub fn entry_name(&self, i: usize) -> Option<&str> {
        self.entries.get(i).map(|e| e.name.as_str())
    }

    /// Tile `i`'s folder members, if it is a folder rather than an app.
    pub fn entry_folder(&self, i: usize) -> Option<&[AppGridEntry]> {
        self.entries.get(i)?.folder.as_deref()
    }

    /// The icon of tile `i`, if present (what a drag of that tile carries).
    pub fn entry_icon(&self, i: usize) -> Option<&AppIconRef> {
        self.entries.get(i).map(|e| &e.icon)
    }

    /// Every app entry's icon — for the startup decode prewarm
    /// (`Niri::prewarm_app_icons`). Folders have none of their own; theirs is
    /// [`Self::folder_icon_refs`], which decodes at a different size.
    pub fn icon_refs(&self) -> impl Iterator<Item = &AppIconRef> {
        self.entries
            .iter()
            .filter(|e| e.folder.is_none())
            .map(|e| &e.icon)
    }

    /// The icons a folder tile composes — the members that fit its 2×2, drawn at
    /// [`widget::TileMetrics::folder_subicon_px`] rather than the full tile icon size,
    /// so the prewarm has to warm them at that size separately.
    pub fn folder_icon_refs(&self) -> impl Iterator<Item = &AppIconRef> {
        self.entries
            .iter()
            .filter_map(|e| e.folder.as_ref())
            .flat_map(|members| members.iter().take(FOLDER_SUBICONS))
            .map(|m| &m.icon)
    }

    /// Set the mouse-hovered tile (an absolute entry index); returns whether it
    /// changed (→ redraw). Deliberately does **not** bump `content_rev`: the hover wash
    /// is a separate element, so a hover change repositions it without re-baking (and
    /// re-shaping) the page's labels — the difference between a smooth and a stuttering
    /// open animation when the mouse is moving.
    pub fn set_hovered(&mut self, hovered: Option<usize>) -> bool {
        if self.hovered == hovered {
            return false;
        }
        self.hovered = hovered;
        true
    }

    /// The keyboard-focused tile, if any (what Enter activates).
    pub fn focused(&self) -> Option<usize> {
        self.focused
    }

    /// Set (or clear) the keyboard-focused tile; returns whether it changed (→ redraw).
    /// Like [`set_hovered`](Self::set_hovered) it does not bump `content_rev` — the focus
    /// ring is its own element, and the caption expansion it drives already re-bakes on
    /// its own per-tile revision (the shaped lines change).
    pub fn set_focused(&mut self, focused: Option<usize>) -> bool {
        let focused = focused.filter(|&i| i < self.entries.len());
        if self.focused == focused {
            return false;
        }
        self.focused = focused;
        true
    }

    /// Move the keyboard focus one step in `dir`, paging the grid to follow it. Returns
    /// whether anything moved (→ redraw).
    ///
    /// This is St's spatial navigation, not `index ± 1`: `filter_by_position` keeps only
    /// the tiles strictly in `dir` (bbox thresholds with 0.1 px of slop) and
    /// `sort_by_distance` takes the nearest by squared **midpoint** distance
    /// (`st-widget.c:1932-2030`). GNOME runs it over the whole paginated viewport, whose
    /// pages sit edge to edge, so [`Self::virtual_tile`] reconstructs that viewport from
    /// the one page we lay out. Landing on another page then pages there —
    /// `key-focus-in` → `_ensureItemIsVisible` (`iconGrid.js:1196-1208`).
    ///
    /// Divergence: with nothing focused this takes the current page's first tile. GNOME
    /// arrives from the search entry through a stage-wide focus chain we do not have.
    pub fn focus_navigate(&mut self, dir: FocusDir, area: Rectangle<f64, Logical>) -> bool {
        let layout = self.layout(area);
        if layout.tiles.is_empty() {
            return false;
        }
        let Some(from) = self.focused.filter(|&i| i < self.entries.len()) else {
            self.focused = Some(layout.first_index);
            return true;
        };
        let from_box = self.virtual_tile(&layout, area, from);
        let (fx1, fy1) = (from_box.loc.x, from_box.loc.y);
        let (fx2, fy2) = (fx1 + from_box.size.w, fy1 + from_box.size.h);
        let mid = |r: Rectangle<f64, Logical>| (r.loc.x + r.size.w / 2., r.loc.y + r.size.h / 2.);
        let (mx, my) = mid(from_box);

        let target = (0..self.entries.len())
            .filter(|&i| i != from)
            .map(|i| (i, self.virtual_tile(&layout, area, i)))
            .filter(|(_, b)| {
                let (x1, y1) = (b.loc.x, b.loc.y);
                let (x2, y2) = (x1 + b.size.w, y1 + b.size.h);
                match dir {
                    FocusDir::Up => y2 <= fy1 + 0.1,
                    FocusDir::Down => y1 >= fy2 - 0.1,
                    FocusDir::Left => x2 <= fx1 + 0.1,
                    FocusDir::Right => x1 >= fx2 - 0.1,
                }
            })
            .min_by(|(_, a), (_, b)| {
                let d = |r: &Rectangle<f64, Logical>| {
                    let (cx, cy) = mid(*r);
                    (cx - mx).powi(2) + (cy - my).powi(2)
                };
                d(a).total_cmp(&d(b))
            })
            .map(|(i, _)| i);

        let Some(target) = target else {
            return false;
        };
        self.focused = Some(target);
        self.set_page(target / layout.per_page, area);
        true
    }

    /// Move the keyboard focus one step in **tab order**, wrapping — which is a different
    /// traversal from the arrows' spatial one: `st_widget_real_navigate_focus` walks
    /// `st_widget_get_focus_chain` for `TAB_FORWARD`/`TAB_BACKWARD` (`st-widget.c:2086-2103`),
    /// i.e. plain child order, and `st_widget_navigate_focus` retries from the start when
    /// it falls off the end (`:2214-2224`, `wrap_around` is set for Tab). Returns whether
    /// anything moved.
    ///
    /// With nothing focused this *enters* the grid: `navigate_focus(null, TAB_FORWARD)`
    /// takes the first item, backward the last (`overviewControls.js:464-470`, the handler
    /// that gets Tab when the focus manager found no group to move within).
    pub fn focus_tab(&mut self, forward: bool, area: Rectangle<f64, Logical>) -> bool {
        let n = self.entries.len();
        if n == 0 {
            return false;
        }
        let target = match self.focused.filter(|&i| i < n) {
            None if forward => 0,
            None => n - 1,
            Some(i) if forward => (i + 1) % n,
            Some(i) => (i + n - 1) % n,
        };
        if self.focused == Some(target) {
            return false;
        }
        self.focused = Some(target);
        // Focus drags the page with it, as ever (`_ensureItemIsVisible`).
        let per_page = self.layout(area).per_page.max(1);
        self.set_page(target / per_page, area);
        true
    }

    /// The box of absolute entry `i` in the *paginated viewport* — its in-page cell rect
    /// shifted by one band width per page away from the current one. Our layout only
    /// places the visible page; GNOME's grid is one actor holding every page side by side,
    /// and the spatial navigation above is defined against that.
    fn virtual_tile(
        &self,
        layout: &GridLayout,
        area: Rectangle<f64, Logical>,
        i: usize,
    ) -> Rectangle<f64, Logical> {
        let tile = layout.metrics.size();
        let cell = tile.h;
        let origin = layout.tiles[0].loc;
        let k = i % layout.per_page;
        let (r, c) = (k / layout.cols, k % layout.cols);
        let page_dx = (i / layout.per_page) as f64 - layout.page as f64;
        Rectangle::new(
            Point::from((
                origin.x + c as f64 * (cell + layout.h_sp) + page_dx * area.size.w,
                origin.y + r as f64 * (cell + layout.v_sp),
            )),
            tile,
        )
    }

    /// Fade one tile (by id) to `alpha`, or clear the fade. Returns whether anything
    /// changed (→ redraw).
    ///
    /// Changing *which* tile fades re-bakes the page — the faded tile leaves the shared
    /// label/background bakes and is re-emitted on its own, so the shared bakes' contents
    /// really do change. Changing only the *alpha* must not: it moves every frame of a
    /// 200 ms animation, and folding it into the revision would re-shape the whole page's
    /// text per frame — the bug class in [`crate::ui::app_grid`]'s sibling widgets that a
    /// per-frame bake always turns out to be.
    pub fn set_tile_fade(&mut self, fade: Option<(String, f64)>) -> bool {
        let was = self.tile_fade.as_ref().map(|(id, _)| id.clone());
        let now = fade.as_ref().map(|(id, _)| id.clone());
        if self.tile_fade == fade {
            return false;
        }
        self.tile_fade = fade;
        if was != now {
            self.content_rev += 1;
        }
        true
    }

    /// Set the mouse-hovered navigation arrow; returns whether it changed (→ redraw).
    /// Like [`set_hovered`](Self::set_hovered), does not bump `content_rev` — the arrow
    /// wash is its own element.
    pub fn set_arrow_hovered(&mut self, arrow: Option<PageArrow>) -> bool {
        if self.hovered_arrow == arrow {
            return false;
        }
        self.hovered_arrow = arrow;
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
    ///
    /// `goToPage` (`iconGrid.js:1348-1377`): the page changes **now** and the view eases
    /// after it, so everything reading [`Self::current_page`] — hit-testing, key focus,
    /// drop targets — is already on the destination while the slide is still running.
    pub fn set_page(&mut self, page: usize, area: Rectangle<f64, Logical>) -> bool {
        let n_pages = self.page_count(area);
        let page = page.min(n_pages.saturating_sub(1));
        if page == self.current_page {
            return false;
        }
        self.current_page = page;
        self.slide_to(page as f64, PAGE_SWITCH_MS);
        true
    }

    /// Reset to the first page (a fresh overview open, `Main.overview 'hidden'` →
    /// `goToPage(0)`, `appDisplay.js:1342`); returns whether it moved.
    pub fn reset_page(&mut self) -> bool {
        if self.current_page == 0 && self.page_pos() == 0. {
            return false;
        }
        self.current_page = 0;
        // Off screen: `goToPage` skips the ease when the grid is not mapped
        // (`iconGrid.js:1366-1367`), and this only ever runs while it is hidden.
        self.gesture = None;
        self.slide = Animation::ease(self.clock.clone(), 0., 0., 0., 0, Curve::EaseOutCubic);
        true
    }

    /// Begin a 1:1 page swipe (`_swipeBegin`, `appDisplay.js:706-719`): the running
    /// slide is dropped and the view follows the finger from wherever it had got to.
    pub fn gesture_begin(&mut self, source: SwipeSource, area: Rectangle<f64, Logical>) {
        let from = self.page_pos();
        self.gesture = Some(from);
        self.gesture_from = from;
        self.swipe = crate::input::swipe_tracker::SwipeTracker::new();
        self.swipe_source = source;
        self.swipe_distance = match source {
            SwipeSource::Touchpad => SWIPE_TOUCHPAD_PAGE_PX,
            // `confirmSwipe(this._grid.allocation.get_width(), …)`.
            SwipeSource::Pointer => area.size.w.max(1.),
        };
    }

    /// Whether a swipe is currently holding the view.
    pub fn gesture_is_active(&self) -> bool {
        self.gesture.is_some()
    }

    /// Move a live swipe by `delta_px` of horizontal travel (`_swipeUpdate`,
    /// `appDisplay.js:721-724` — the adjustment simply *is* the gesture position, which
    /// is why the drag is 1:1 and not a rate). Returns whether it moved (→ redraw).
    pub fn gesture_update(
        &mut self,
        delta_px: f64,
        timestamp: std::time::Duration,
        area: Rectangle<f64, Logical>,
    ) -> bool {
        let Some(pos) = self.gesture else {
            return false;
        };
        let last = self.page_count(area).saturating_sub(1) as f64;
        if last <= 0. {
            return false;
        }
        // The tracker holds *pixels*, which is the unit the release threshold is in.
        self.swipe.push(delta_px, timestamp);
        let step = delta_px / self.swipe_distance;
        // At most one page either side of where the gesture began (`_getBounds`).
        let lo = (self.gesture_from.floor() - 1.).clamp(0., last);
        let hi = (self.gesture_from.ceil() + 1.).clamp(0., last);
        let next = (pos + step).clamp(lo, hi);
        if next == pos {
            return false;
        }
        self.gesture = Some(next);
        true
    }

    /// Release a swipe (`_swipeEnd`, `appDisplay.js:726-735`): project where it would
    /// coast to, snap that to a page, and ease there — then the page bookkeeping catches
    /// up, which is `goToPage(endProgress, false)`. Returns whether anything changed.
    pub fn gesture_end(&mut self, area: Rectangle<f64, Logical>) -> bool {
        let Some(pos) = self.gesture else {
            return false;
        };
        let last = self.page_count(area).saturating_sub(1) as f64;
        // Pixels per millisecond (`velocity()` is per second).
        let velocity = self.swipe.velocity() / 1000.;
        let initial = self.gesture_from.round();

        let threshold = match self.swipe_source {
            SwipeSource::Touchpad => SWIPE_VELOCITY_THRESHOLD_TOUCHPAD,
            SwipeSource::Pointer => SWIPE_VELOCITY_THRESHOLD_TOUCH,
        };
        let target = if velocity.abs() < threshold {
            // A slow drag just falls to the nearest page (`_getEndProgress`, first branch).
            pos.round()
        } else if velocity > 0. {
            initial + 1.
        } else {
            initial - 1.
        }
        .clamp(0., last);
        // Above the threshold GNOME projects `velocity * slope` and clamps it to the snap
        // points either side of where the gesture began (`_getEndProgress` +
        // `_getBounds`). The projection is in pixels while the progress it is added to is
        // in pages, so for any velocity that clears the threshold at all it overshoots and
        // the clamp decides: a flick moves exactly one page, in the direction of travel.
        // That is what is reproduced above, rather than the arithmetic that gets there.

        // `|Δprogress| / velocity * DURATION_MULTIPLIER` with the velocity normalized to
        // pages (`swipeTracker.js:644,652-654`) — a fast flick settles quickly, a slow one
        // does not snap.
        let pages_per_ms = velocity / self.swipe_distance;
        let ms = if pages_per_ms == 0. {
            PAGE_SWITCH_MS
        } else {
            let raw = ((target - pos) / pages_per_ms * SWIPE_DURATION_MULTIPLIER).abs();
            (raw.round() as u64).clamp(SWIPE_MIN_MS, SWIPE_MAX_MS)
        };

        self.current_page = target as usize;
        self.slide_to(target, ms);
        true
    }

    /// Abandon a live swipe without a release — the pointer left the grid, or the grid
    /// went away. Settles onto the page it is nearest.
    pub fn gesture_cancel(&mut self, area: Rectangle<f64, Logical>) -> bool {
        if self.gesture.is_none() {
            return false;
        }
        self.swipe = crate::input::swipe_tracker::SwipeTracker::new();
        self.gesture_end(area)
    }

    /// Ease the view to `to` (in pages) over `ms`, from wherever it is now — an
    /// interrupted slide keeps its position rather than snapping
    /// (`adjustment.ease`, `iconGrid.js:1371-1375`).
    fn slide_to(&mut self, to: f64, ms: u64) {
        let from = self.page_pos();
        self.gesture = None;
        self.slide = Animation::ease(self.clock.clone(), from, to, 0., ms, Curve::EaseOutCubic);
    }

    /// Tiles per page for `area` — what a `(page, position)` pair counts in.
    pub fn items_per_page(&self, area: Rectangle<f64, Logical>) -> usize {
        self.layout(area).per_page
    }

    /// The id at `target`, if any — GNOME's `getItemAt(page, position)`. `None` for an
    /// append target or a position past the end.
    pub fn entry_id_at(&self, target: GridDropTarget, per_page: usize) -> Option<&str> {
        let position = target.position?;
        self.entry_id(target.page * per_page + position)
    }

    /// Where a drag holding `dragged` would insert, for a pointer at `pos`
    /// (`IconGrid.getDropTarget`, `iconGrid.js:1032-1120`, then the reflow adjustment
    /// in `AppDisplay._getDropTarget`, `appDisplay.js:1156-1201`). `None` is GNOME's
    /// `INVALID` — above or below the rows, or an empty grid.
    ///
    /// Only the current page is laid out (GNOME keeps every page in one scrolled
    /// actor), so a target is always on it; reaching another page is the job of the
    /// page-preview bands.
    pub fn drop_target_at(
        &self,
        pos: Point<f64, Logical>,
        area: Rectangle<f64, Logical>,
        dragged: &str,
    ) -> Option<GridDropTarget> {
        let layout = self.layout(area);
        if layout.tiles.is_empty() {
            return None;
        }
        let block = layout.block;
        // Above or below the rows is not a drop target at all.
        if pos.y < block.loc.y || pos.y > block.loc.y + block.size.h {
            return None;
        }
        let in_left = pos.x < block.loc.x;
        let in_right = pos.x > block.loc.x + block.size.w;
        let (half_h, half_v) = (layout.h_sp / 2., layout.v_sp / 2.);

        for (i, tile) in layout.tiles.iter().enumerate() {
            let (x1, x2) = (tile.loc.x, tile.loc.x + tile.size.w);
            let (y1, y2) = (tile.loc.y, tile.loc.y + tile.size.h);
            let first_in_row = i % layout.cols == 0;
            let last_in_row = i % layout.cols == layout.cols - 1;
            // In the side margins only the row matters: the outermost tile of the row
            // claims everything beside it.
            if (in_left && first_in_row) || (in_right && last_in_row) {
                if pos.y < y1 - half_v || pos.y > y2 + half_v {
                    continue;
                }
            } else if pos.x < x1 - half_h
                || pos.x > x2 + half_h
                || pos.y < y1 - half_v
                || pos.y > y2 + half_v
            {
                continue;
            }
            let location = if pos.x < x1 + DIVIDER_LEEWAY {
                DragLocation::StartEdge
            } else if pos.x > x2 - DIVIDER_LEEWAY {
                DragLocation::EndEdge
            } else {
                DragLocation::OnIcon
            };
            return Some(self.adjust_for_reflow(&layout, i, location, dragged));
        }

        Some(GridDropTarget {
            page: layout.page,
            position: None,
            location: DragLocation::EmptySpace,
        })
    }

    /// Retarget an edge hit to the adjacent tile when the reflow would push the wrong
    /// way (`appDisplay.js:1156-1201`). Dropping just left of an icon that will move
    /// *left* to make room can't "naturally push it away", so the insertion is really
    /// after its neighbour — except in the first/last column, where there is no
    /// neighbour to push.
    fn adjust_for_reflow(
        &self,
        layout: &GridLayout,
        position: usize,
        location: DragLocation,
        dragged: &str,
    ) -> GridDropTarget {
        let source = self.entries.iter().position(|e| e.id == dragged);
        let (source_page, source_position) = match source {
            Some(i) => (i / layout.per_page, i % layout.per_page),
            None => (usize::MAX, usize::MAX),
        };
        // The app grid is built with `allow_incomplete_pages: true`
        // (`appDisplay.js:655`), so GNOME's `sourcePage < targetPage` branch — the one
        // that forces a START reflow when pages must stay full — never applies here.
        let reflow_start = source_page == layout.page && source_position < position;
        let reflow_none = source_position == position && !reflow_start;

        let column = position % layout.cols;
        let (position, location) = match location {
            DragLocation::StartEdge if reflow_start && column > 0 => {
                (position - 1, DragLocation::EndEdge)
            }
            DragLocation::EndEdge if !reflow_start && !reflow_none && column + 1 < layout.cols => {
                (position + 1, DragLocation::StartEdge)
            }
            _ => (position, location),
        };
        GridDropTarget {
            page: layout.page,
            position: Some(position),
            location,
        }
    }

    /// Move `id` to `target` (`AppDisplay._moveItem` over `IconGrid.moveItem`,
    /// `appDisplay.js:1203-1209`): pull it out, then put it back at that position
    /// *within the shortened list*. Returns whether anything moved.
    pub fn move_entry(&mut self, id: &str, target: GridDropTarget, per_page: usize) -> bool {
        let Some(from) = self.entries.iter().position(|e| e.id == id) else {
            return false;
        };
        let entry = self.entries.remove(from);
        let page_start = (target.page * per_page).min(self.entries.len());
        let page_end = (page_start + per_page).min(self.entries.len());
        let to = match target.position {
            Some(position) => (page_start + position).min(page_end),
            None => page_end,
        };
        self.entries.insert(to, entry);
        if to == from {
            return false;
        }
        // The hover is an absolute index, so a reorder would leave it on a different
        // app; a drag suppresses the wash anyway, so just drop it.
        self.hovered = None;
        self.content_rev += 1;
        true
    }

    /// Snapshot the current order so a drag that ends nowhere can put it back
    /// (`_onDragCancelled` → `_redisplay`, `appDisplay.js:979-984`).
    pub fn begin_reorder(&mut self) {
        self.reorder_restore = Some(self.entries.iter().map(|e| e.id.clone()).collect());
    }

    /// Restore the pre-drag order. Returns whether anything moved back (→ redraw).
    pub fn cancel_reorder(&mut self) -> bool {
        let Some(order) = self.reorder_restore.take() else {
            return false;
        };
        if order.len() != self.entries.len()
            || order.iter().zip(&self.entries).all(|(id, e)| *id == e.id)
        {
            return false;
        }
        // Rebuild by id; anything the catalog changed under us keeps its current place.
        let mut restored = Vec::with_capacity(self.entries.len());
        for id in &order {
            if let Some(k) = self.entries.iter().position(|e| e.id == *id) {
                restored.push(self.entries.remove(k));
            }
        }
        restored.append(&mut self.entries);
        self.entries = restored;
        self.hovered = None;
        self.content_rev += 1;
        true
    }

    /// Accept the live reorder; returns whether the order actually changed, i.e.
    /// whether it is worth writing `app-picker-layout` back.
    pub fn finish_reorder(&mut self) -> bool {
        self.reorder_restore
            .take()
            .is_some_and(|order| !order.iter().zip(&self.entries).all(|(id, e)| *id == e.id))
    }

    /// The current order as pages of app ids — what `_savePages` persists
    /// (`appDisplay.js:1387-1404`).
    pub fn pages(&self, per_page: usize) -> Vec<Vec<String>> {
        self.entries
            .chunks(per_page.max(1))
            .map(|chunk| chunk.iter().map(|e| e.id.clone()).collect())
            .collect()
    }

    /// Lay the apps into `area` (the `app_display` band) as GNOME's paginated
    /// `IconGrid`: pick the page mode by aspect ratio, shrink the icon to the largest
    /// size that fits, distribute the spacing, and position the current page's tiles
    /// in square cells. A dots strip is reserved at the bottom.
    fn layout(&self, area: Rectangle<f64, Logical>) -> GridLayout {
        self.layout_at(area, self.current_page, 0.)
    }

    /// [`Self::layout`] for an arbitrary `page`, its tiles shifted `dx_pages` page widths
    /// sideways — how a page that is sliding in or out is placed. GNOME lays every page
    /// out side by side in one scroll view and moves the view; the shift here is the same
    /// thing seen from the page's end. Only the *tiles* move: the dots and the navigation
    /// arrows live outside the scroll view and stay where they are
    /// (`appDisplay.js:1251-1252`).
    fn layout_at(&self, area: Rectangle<f64, Logical>, page: usize, dx_pages: f64) -> GridLayout {
        let empty = GridLayout {
            tiles: Vec::new(),
            block: Rectangle::from_size(Size::from((0., 0.))),
            metrics: TileMetrics::OVERVIEW,
            first_index: 0,
            cols: 1,
            h_sp: 0.,
            v_sp: 0.,
            per_page: 1,
            n_pages: 0,
            page: 0,
            indicators: None,
            prev_arrow: None,
            next_arrow: None,
            hints: [Rectangle::default(); 2],
        };
        let n = self.entries.len();
        // The grid page is the band minus the reserved dots strip.
        let page_w = area.size.w;
        let page_h = (area.size.h - INDICATORS_STRIP_H).max(0.);
        // The side bands are reserved out of the page before anything else, and the
        // grid's own page padding sits inside them (`_updatePadding` *adds* the two).
        let hint_w = indicators_w(page_w);
        let pad_h = PAGE_PAD_H + hint_w;
        let hints = [
            Rectangle::new(area.loc, Size::from((hint_w, page_h))),
            Rectangle::new(
                Point::from((area.loc.x + page_w - hint_w, area.loc.y)),
                Size::from((hint_w, page_h)),
            ),
        ];
        let content_w = page_w - 2. * pad_h;
        let content_h = page_h - 2. * PAGE_PAD_V;
        if n == 0 || content_w <= 0. || content_h <= 0. {
            return empty;
        }

        // Grid mode: the (columns, rows) whose ratio is closest to the content's.
        let ratio = content_w / content_h;
        let &(cols, rows) = self
            .modes
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
        let cell = tile.h; // the tile is square, so the `max(w, h)` cell is the tile

        let per_page = cols * rows;
        let n_pages = n.div_ceil(per_page);
        let page = page.min(n_pages - 1);

        // Distribute spacing + centering per axis, then place the current page.
        let (x_off, h_sp) = distribute(
            page_w,
            cols,
            cell,
            COL_SPACING,
            MAX_COL_SPACING,
            pad_h,
            self.align,
        );
        let (y_off, v_sp) = distribute(
            page_h,
            rows,
            cell,
            ROW_SPACING,
            MAX_ROW_SPACING,
            PAGE_PAD_V,
            self.align,
        );
        let origin_x = area.loc.x + x_off;
        let origin_y = area.loc.y + y_off;

        // The page's own offset in the scroll view.
        let dx = dx_pages * area.size.w;
        let origin_x = origin_x + dx;

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

        // Navigation arrows, centered in their reserved band on both axes
        // (`allocate_align_fill(box, 0.5, 0.5)`, `appDisplay.js:422-427`; shown when a
        // previous / next page exists). The band, not the block: it does not move when
        // the last page has fewer rows.
        let disc = |band: Rectangle<f64, Logical>| {
            Rectangle::new(
                Point::from((
                    (band.loc.x + (band.size.w - ARROW_DISC) / 2.).round(),
                    (band.loc.y + (band.size.h - ARROW_DISC) / 2.).round(),
                )),
                Size::from((ARROW_DISC, ARROW_DISC)),
            )
        };
        let prev_arrow = (page > 0).then(|| disc(hints[0]));
        let next_arrow = (page + 1 < n_pages).then(|| disc(hints[1]));

        GridLayout {
            tiles,
            block,
            metrics,
            first_index,
            cols,
            h_sp,
            v_sp,
            per_page,
            n_pages,
            page,
            indicators,
            prev_arrow,
            next_arrow,
            hints,
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

    /// The tile metrics the grid will actually render `area` with.
    ///
    /// The icon size is **chosen from the band** — the largest of [`ICON_SIZES`] whose
    /// cells fit the mode — so it is not [`TileMetrics::OVERVIEW`]'s 96 on every display
    /// (a 1280×800 screen renders at 48). The decode cache is keyed by logical px, so a
    /// prewarm at the wrong size warms an entry nothing ever asks for, and every icon
    /// still decodes lazily the first time its page is looked at.
    pub fn metrics_for(&self, area: Rectangle<f64, Logical>) -> TileMetrics {
        self.layout(area).metrics
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

    /// The logical center of entry `i` — an *absolute* index, as
    /// [`hit_test`](Self::hit_test) reports and a drag carries. `None` when the
    /// entry is not on the current page (nothing is drawn for it).
    pub fn entry_center(
        &self,
        i: usize,
        area: Rectangle<f64, Logical>,
    ) -> Option<Point<f64, Logical>> {
        self.entry_rect(i, area)
            .map(|t| Point::from((t.loc.x + t.size.w / 2., t.loc.y + t.size.h / 2.)))
    }

    /// Entry `i`'s tile rect on the page it is on — what a context menu anchors on.
    /// `None` when `i` is on another page (the index is catalog-wide, the tiles are
    /// per-page).
    pub fn entry_rect(
        &self,
        i: usize,
        area: Rectangle<f64, Logical>,
    ) -> Option<Rectangle<f64, Logical>> {
        let layout = self.layout(area);
        let k = i.checked_sub(layout.first_index)?;
        layout.tiles.get(k).copied()
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

    /// The label-bake revision — a test probe for the invariant that a hover change does
    /// not invalidate it (which would re-shape the labels every mouse move).
    #[cfg(test)]
    pub fn content_rev(&self) -> u64 {
        self.content_rev
    }

    /// The logical center of the current page's tile `k`'s **icon** (not the whole tile —
    /// the icon sits above the label) — a render-test probe for sampling the drawn glyph.
    #[cfg(test)]
    pub fn icon_center(
        &self,
        k: usize,
        area: Rectangle<f64, Logical>,
    ) -> Option<Point<f64, Logical>> {
        let layout = self.layout(area);
        layout.tiles.get(k).map(|t| layout.metrics.icon_center(*t))
    }

    /// The logical center of sub-icon `sub` of the current page's folder tile `k`, and
    /// the sub-icon's side — the same probe for a folder's 2×2 composition.
    #[cfg(test)]
    pub fn folder_subicon_center(
        &self,
        k: usize,
        sub: usize,
        area: Rectangle<f64, Logical>,
    ) -> Option<(Point<f64, Logical>, f64)> {
        let layout = self.layout(area);
        let tile = layout.tiles.get(k)?;
        Some((
            layout.metrics.folder_subicon_center(*tile, sub),
            layout.metrics.folder_subicon_px(),
        ))
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

    /// One page's elements — its icons, captions, chrome, focus ring, hover wash and
    /// folder fills — with `layout` already placed (see [`Self::layout_at`]). Called once
    /// when the grid is settled, and twice while a page slide is running.
    #[allow(clippy::too_many_arguments)]
    fn render_page(
        &self,
        renderer: &mut VulkanRenderer,
        app_icons: &AppIconCache,
        scale: f64,
        accent: [u8; 3],
        alpha: f32,
        layout: &GridLayout,
        cache: &mut GridCache,
        elements: &mut Vec<TextureRenderElement<VkTexture>>,
    ) {
        let metrics = layout.metrics;
        let first = layout.first_index;
        let page = layout.page;
        let page_entries = &self.entries[first..first + layout.tiles.len()];

        // --- Batch-upload any page icons whose decode is ready but not yet on the GPU, so the
        //     first open pays ONE submit+fence for the whole page instead of ~24 (a real Venus
        //     stutter). `app_icon_element` below then finds them cached; an icon still decoding
        //     stays a miss and is simply drawn on a later frame, exactly as before. Only worth a
        //     batch for 2+ pending uploads — a lone miss uploads fine on its own. ---
        let subicon_px = metrics.folder_subicon_px();
        if let Ok(scale_key) = NotNan::new(scale) {
            let mut keys: Vec<(NotNan<f64>, AppIconRef, u16)> = Vec::new();
            let mut buffers = Vec::new();
            // A folder tile draws its members instead of an icon of its own, at the
            // smaller sub-icon size — so it contributes up to four uploads, not one.
            let pending: Vec<(&AppIconRef, f64)> = page_entries
                .iter()
                .flat_map(|entry| match &entry.folder {
                    None => vec![(&entry.icon, metrics.icon_px)],
                    Some(members) => members
                        .iter()
                        .take(FOLDER_SUBICONS)
                        .map(|m| (&m.icon, subicon_px))
                        .collect(),
                })
                .collect();
            for (icon, px) in pending {
                let key = (scale_key, icon.clone(), (px.round() as u16).max(1));
                if cache.icons.contains_key(&key) || keys.contains(&key) {
                    continue;
                }
                if let Some(buf) = app_icons.buffer(icon, px, scale) {
                    keys.push(key);
                    buffers.push(buf);
                }
            }
            if buffers.len() > 1 {
                let items: Vec<_> = buffers
                    .iter()
                    .map(|b| (b.data(), b.format(), b.size(), false))
                    .collect();
                match renderer.import_memory_batch(&items) {
                    Ok(textures) => {
                        for ((key, buf), texture) in keys.into_iter().zip(&buffers).zip(textures) {
                            let tb = TextureBuffer::from_texture(
                                renderer,
                                texture,
                                buf.scale(),
                                buf.transform(),
                                Vec::new(),
                            );
                            cache.icons.insert(key, tb);
                        }
                    }
                    Err(err) => tracing::error!("error batch-uploading app icons: {err:#}"),
                }
            }
        }

        // The tile the open folder dialog zoomed out of, page-relative, and the alpha it
        // draws at. It leaves both shared bakes below and is re-emitted on its own, so its
        // whole tile — background, caption and icons — fades as one actor does in GNOME.
        let faded: Option<(usize, f32)> = self.tile_fade.as_ref().and_then(|(id, fade)| {
            let k = page_entries.iter().position(|e| &e.id == id)?;
            Some((k, alpha * *fade as f32))
        });
        let tile_alpha = |k: usize| match faded {
            Some((f, a)) if f == k => a,
            _ => alpha,
        };

        // --- App icons (topmost, over their tiles). A folder has no icon of its own:
        //     its tile composes its first four members into a 2×2 instead
        //     (`createFolderIcon`, `appDisplay.js:2138-2162`). ---
        for (k, entry) in page_entries.iter().enumerate() {
            let tile = layout.tiles[k];
            let icons: Vec<(&AppIconRef, f64, Point<f64, Logical>)> = match &entry.folder {
                None => vec![(&entry.icon, metrics.icon_px, metrics.icon_center(tile))],
                Some(members) => members
                    .iter()
                    .take(FOLDER_SUBICONS)
                    .enumerate()
                    .map(|(i, m)| (&m.icon, subicon_px, metrics.folder_subicon_center(tile, i)))
                    .collect(),
            };
            for (icon, px, center) in icons {
                if let Some(el) = widget::app_icon_element(
                    renderer,
                    &mut cache.icons,
                    app_icons,
                    icon,
                    px,
                    scale,
                    Point::from((0., 0.)),
                    center,
                    tile_alpha(k),
                ) {
                    elements.push(el);
                }
            }
        }

        // --- Tile captions. A name that fits its label box on one line goes into the
        //     page bake below with everything else; a name that has to be cut becomes
        //     its OWN element, because the hovered tile shows the whole name wrapped
        //     (`_updateMultiline`, `appDisplay.js:1891-1924`) and folding that into the
        //     page bake would make the page depend on the hover again. ---
        let label_w = metrics.label_w();
        let page_range = first..first + layout.tiles.len();
        // `expand = _forcedHighlight || hover || has_key_focus()` (`appDisplay.js:1901`) —
        // hover and key focus expand independently, and may sit on different tiles.
        let hovered_at = self.hovered.filter(|i| page_range.contains(i));
        let focused_at = self.focused.filter(|i| page_range.contains(i));
        let collapsed: Vec<Vec<String>> = page_entries
            .iter()
            .map(|e| widget::tile_label_lines(&e.name, LABEL_PT, label_w, false))
            .collect();
        // Hover-independent by construction: whether a name fits is a property of the
        // name and the label box, so the page bake's contents still never move on hover.
        let mut fits: Vec<bool> = collapsed
            .iter()
            .zip(page_entries)
            .map(|(lines, e)| lines.first().is_none_or(|line| *line == e.name))
            .collect();
        // A faded tile takes the *per-tile* caption path even though its name fits: that
        // path already bakes one tile's label as its own element, which is exactly what
        // letting it fade on its own needs. It drops out of the shared page bake below by
        // the same `fits` filter.
        if let Some((k, _)) = faded {
            fits[k] = false;
        }

        cache
            .long_labels
            .retain(|(p, k), _| *p != page || fits.get(*k).is_some_and(|fits| !fits));
        // How much taller the expanded caption made the hovered / focused tile — GNOME's
        // tile allocation follows its label, so the `:hover` wash and the `:focus` ring
        // each grow with the one they sit on.
        let mut hover_extra_h = 0.;
        let mut focus_extra_h = 0.;
        for (k, entry) in page_entries.iter().enumerate() {
            if fits[k] {
                continue;
            }
            let i = first + k;
            let expanded = hovered_at == Some(i) || focused_at == Some(i);
            let lines = if expanded {
                widget::tile_label_lines(&entry.name, LABEL_PT, label_w, true)
            } else {
                collapsed[k].clone()
            };
            let line_h = metrics.label_h;
            if expanded {
                let extra = line_h * (lines.len() as f64 - 1.);
                if hovered_at == Some(i) {
                    hover_extra_h = extra;
                }
                if focused_at == Some(i) {
                    focus_extra_h = extra;
                }
            }
            let size = Size::from((label_w, line_h * lines.len() as f64));
            let revision = widget::Revision::new().each(&lines).done();
            let shape_lines = lines.clone();
            let bake = match widget::bake(
                renderer,
                cache.long_labels.entry((page, k)).or_default(),
                scale,
                size,
                revision,
                move |r| {
                    let mut shaper = widget::TextShaper::new(r, scale);
                    shape_lines
                        .iter()
                        .map(|line| shaper.shape(line, widget::TextStyle::new(LABEL_PT)))
                        .collect::<anyhow::Result<Vec<_>>>()
                },
                move |frame, phys, shaped| {
                    let mut p = Painter::new(frame, scale, phys);
                    p.clear(style::TRANSPARENT)?;
                    p.caption(shaped, label_w, line_h, style::TEXT)?;
                    Ok(())
                },
            ) {
                Ok(texture) => texture,
                Err(err) => {
                    tracing::error!("error baking an app-grid caption: {err:#}");
                    continue;
                }
            };
            let tile = layout.tiles[k];
            let at = Point::from((
                (tile.loc.x + (tile.size.w - label_w) / 2.).round(),
                metrics.label_top(tile),
            ));
            let buffer =
                TextureBuffer::from_texture(renderer, bake, scale, Transform::Normal, vec![]);
            elements.push(TextureRenderElement::from_texture_buffer(
                buffer,
                at,
                tile_alpha(k),
                None,
                None,
                Kind::Unspecified,
            ));
        }

        // --- The tile labels, one baked transparent texture the size of the block. The
        //     hover wash is drawn as a SEPARATE element below, so a hover change (every
        //     mouse move) never re-runs this text shaping — the bake is keyed on
        //     `content_rev`, which no longer bumps on hover. ---
        let block = layout.block;
        let origin = block.loc;
        let rel_rects: Vec<Rectangle<f64, Logical>> = layout
            .tiles
            .iter()
            .zip(&fits)
            .filter(|(_, fits)| **fits)
            .map(|(t, _)| Rectangle::new(t.loc - origin, t.size))
            .collect();
        let names: Vec<String> = page_entries
            .iter()
            .zip(&fits)
            .filter(|(_, fits)| **fits)
            .map(|(e, _)| e.name.clone())
            .collect();
        match widget::bake(
            renderer,
            cache.bakes.entry(page).or_default(),
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
                for (rel, label) in rel_rects.iter().zip(labels.iter()) {
                    p.labelled_tile(*rel, label, &metrics, false, style::TEXT)?;
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

        // --- The keyboard focus ring (`.overview-tile:focus`, `_drawing.scss:308-327`):
        //     a 2px inset accent stroke over a faint accent-tinted fill. Pushed *before*
        //     the hover wash, i.e. above it, because GNOME's `box-shadow: inset` paints
        //     over the background a `:focus:hover` tile still lightens. Same
        //     bake-one-tile-and-move-it shape as the wash; the accent rides its revision
        //     so a live accent change re-strokes it. ---
        if let Some((mut tile, ring_alpha)) = self
            .focused
            .filter(|&i| (first..first + layout.tiles.len()).contains(&i))
            .map(|i| (layout.tiles[i - first], tile_alpha(i - first)))
        {
            tile.size.h += focus_extra_h;
            let radius = metrics.radius;
            let ring = [
                f32::from(accent[0]) / 255.,
                f32::from(accent[1]) / 255.,
                f32::from(accent[2]) / 255.,
                FOCUS_RING_ALPHA,
            ];
            let bg = focus_bg(ring);
            let revision = widget::Revision::new().of(accent).done();
            match widget::bake(
                renderer,
                &mut cache.focus_bake,
                scale,
                tile.size,
                revision,
                |_| Ok(()),
                move |frame, phys, _: &()| {
                    let mut p = Painter::new(frame, scale, phys);
                    p.clear(style::TRANSPARENT)?;
                    let rect = Rectangle::from_size(tile.size);
                    p.fill_rounded(rect, radius, bg)?;
                    p.stroke_rounded(rect, radius, FOCUS_RING_W, ring)?;
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
                        tile.loc,
                        ring_alpha,
                        None,
                        None,
                        Kind::Unspecified,
                    ));
                }
                Err(err) => tracing::error!("error baking the app-grid focus ring: {err:#}"),
            }
        }

        // --- The tile hover wash (`.overview-tile:hover`), a separate rounded-lighten
        //     element under the labels/icons. Baked at one tile's size and just
        //     repositioned as the pointer moves between tiles, so it costs no re-shape. ---
        if let Some((mut tile, wash_alpha)) = self
            .hovered
            .filter(|&i| (first..first + layout.tiles.len()).contains(&i))
            .map(|i| (layout.tiles[i - first], tile_alpha(i - first)))
        {
            // An expanded caption is taller than the one line the tile box reserves;
            // GNOME re-allocates the tile around it, so the wash covers the extra lines.
            tile.size.h += hover_extra_h;
            let radius = metrics.radius;
            match widget::bake(
                renderer,
                &mut cache.hover_bake,
                scale,
                tile.size,
                0,
                |_| Ok(()),
                move |frame, phys, _: &()| {
                    let mut p = Painter::new(frame, scale, phys);
                    p.clear(style::TRANSPARENT)?;
                    p.fill_rounded(Rectangle::from_size(tile.size), radius, style::HOVER_WASH)?;
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
                        tile.loc,
                        wash_alpha,
                        None,
                        None,
                        Kind::Unspecified,
                    ));
                }
                Err(err) => tracing::error!("error baking the app-grid hover wash: {err:#}"),
            }
        }

        // --- Folder tile backgrounds (`.app-folder`), one bake for the page, *below*
        //     the hover wash so a hovered folder still lightens. `.app-folder` is a
        //     raised tile_button (`_app-grid.scss:41`) where an app tile is flat and
        //     transparent at rest, so this is the only resting fill in the grid. ---
        let folder_rects: Vec<Rectangle<f64, Logical>> = page_entries
            .iter()
            .zip(&layout.tiles)
            .enumerate()
            .filter(|(k, (e, _))| e.folder.is_some() && faded.is_none_or(|(f, _)| f != *k))
            .map(|(_, (_, t))| Rectangle::new(t.loc - origin, t.size))
            .collect();
        if !folder_rects.is_empty() {
            let radius = metrics.radius;
            match widget::bake(
                renderer,
                cache.folder_bakes.entry(page).or_default(),
                scale,
                block.size,
                self.content_rev,
                |_| Ok(()),
                move |frame, phys, _: &()| {
                    let mut p = Painter::new(frame, scale, phys);
                    p.clear(style::TRANSPARENT)?;
                    for rect in &folder_rects {
                        p.fill_rounded(*rect, radius, style::FOLDER_BG)?;
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
                Err(err) => tracing::error!("error baking the app-grid folder tiles: {err:#}"),
            }
        }

        // --- The faded tile's own resting fill. Same `.app-folder` background as the bake
        //     above, alone in its own element so the fade can move every frame. Only a
        //     folder has a resting fill, and only a folder is ever the faded tile. ---
        if let Some((k, fade_alpha)) = faded.filter(|(k, _)| page_entries[*k].folder.is_some()) {
            let tile = layout.tiles[k];
            let radius = metrics.radius;
            match widget::bake(
                renderer,
                &mut cache.fade_bake,
                scale,
                tile.size,
                0,
                |_| Ok(()),
                move |frame, phys, _: &()| {
                    let mut p = Painter::new(frame, scale, phys);
                    p.clear(style::TRANSPARENT)?;
                    p.fill_rounded(Rectangle::from_size(tile.size), radius, style::FOLDER_BG)?;
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
                        tile.loc,
                        fade_alpha,
                        None,
                        None,
                        Kind::Unspecified,
                    ));
                }
                Err(err) => tracing::error!("error baking the faded folder tile: {err:#}"),
            }
        }
    }

    /// Where the view sits, in pages — a gesture's live position if one is in flight,
    /// otherwise the slide animation's (`adjustment.value / page_size`,
    /// `appDisplay.js:723`).
    pub fn page_pos(&self) -> f64 {
        self.gesture.unwrap_or_else(|| self.slide.value())
    }

    /// The pages the view currently spans and how far each is from its resting place, in
    /// page widths. Settled that is one entry at 0; mid-slide it is the outgoing page and
    /// the incoming one, a page apart.
    fn visible_pages(&self, n_pages: usize) -> Vec<(usize, f64)> {
        let pos = self.page_pos();
        let lo = pos.floor().max(0.) as usize;
        let mut out = Vec::with_capacity(2);
        for page in [lo, lo + 1] {
            if page >= n_pages {
                continue;
            }
            let dx = page as f64 - pos;
            // A page a full width away is off the screen; skip it rather than bake it.
            if dx.abs() >= 1. {
                continue;
            }
            out.push((page, dx));
        }
        out
    }

    /// The grid render elements for `output`, into the `app_display` box, at `alpha`.
    /// Icons are pushed first (topmost within the grid); the tile chrome (wash +
    /// labels) bakes last (below the icons) — the dash/search order. The grid as a
    /// whole is pushed below the dash and search in `Niri::render`.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        app_icons: &AppIconCache,
        sym_icons: &IconCache,
        output: &Output,
        area: Rectangle<f64, Logical>,
        alpha: f32,
        accent: [u8; 3],
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

        let mut cache = self.cache.borrow_mut();
        let context = renderer.context_id();
        if cache.context.as_ref() != Some(&context) {
            cache.icons.clear();
            cache.context = Some(context);
        }

        let mut elements = Vec::new();

        // The pages the slide spans, and where each sits. Settled, that is just the
        // current page at rest; mid-slide it is the outgoing and incoming pair, a page
        // width apart. The band is the full output width, so a page on its way out
        // leaves the screen on its own — which is the clip GNOME gets from the scroll
        // view's own allocation.
        let visible = self.visible_pages(layout.n_pages);
        cache
            .bakes
            .retain(|p, _| visible.iter().any(|(v, _)| v == p));
        cache
            .folder_bakes
            .retain(|p, _| visible.iter().any(|(v, _)| v == p));
        cache
            .long_labels
            .retain(|(p, _), _| visible.iter().any(|(v, _)| v == p));
        for (page, dx_pages) in visible {
            let page_layout = self.layout_at(area, page, dx_pages);
            if page_layout.tiles.is_empty() {
                continue;
            }
            self.render_page(
                renderer,
                app_icons,
                scale,
                accent,
                alpha,
                &page_layout,
                &mut cache,
                &mut elements,
            );
        }
        // The resting page's block and caption box, which the drag peek below places
        // its neighbours relative to.
        let block = layout.block;
        let label_w = metrics.label_w();

        // --- The page-preview hint bands (`.page-navigation-hint`), below everything
        //     else in the grid. They slide in from the screen edge while an item drag
        //     is in flight (`_syncPageIndicators`, `appDisplay.js:364-397`): at peek 0
        //     each sits fully outside its own band, at 1 it fills it. ---
        let peek = self.peek.clamped_value();
        if peek > 0. {
            // The previous band leads nowhere from page 0; the next one is live even on
            // the last page, because dropping there is what makes a new page
            // (`appDisplay.js:270-274`).
            let live = [layout.page > 0, layout.n_pages > 0];
            for (i, band) in layout.hints.iter().enumerate() {
                if !live[i] {
                    continue;
                }
                let prev = i == 0;
                let dnd = self.hint_hovered
                    == Some(if prev {
                        PageArrow::Prev
                    } else {
                        PageArrow::Next
                    });
                let (color, ramp) = if dnd {
                    (HINT_DND_COLOR, (0., 0.))
                } else if prev {
                    // Brightest at the band's inner (right) edge, fading to the screen
                    // edge — `.previous:ltr` runs transparent → colour left to right.
                    (HINT_COLOR, (1., 0.))
                } else {
                    (HINT_COLOR, (0., 1.))
                };
                // Only the *inner* corners are cut (`border-radius: 0 36 36 0` on the
                // previous band, `36 0 0 36` on the next), so run the rect past the
                // buffer's outer edge and let those corners clip away.
                let fill = Rectangle::new(
                    Point::from((if prev { -HINT_RADIUS } else { 0. }, 0.)),
                    Size::from((band.size.w + HINT_RADIUS, band.size.h)),
                );
                let revision = widget::Revision::new().of(dnd).of(prev).done();
                match widget::bake(
                    renderer,
                    &mut cache.hint_bakes[i],
                    scale,
                    band.size,
                    revision,
                    |_| Ok(()),
                    move |frame, phys, _: &()| {
                        let mut p = Painter::new(frame, scale, phys);
                        p.clear(style::TRANSPARENT)?;
                        if ramp == (0., 0.) {
                            p.fill_rounded(fill, HINT_RADIUS, color)?;
                        } else {
                            p.fill_rounded_faded(fill, HINT_RADIUS, color, ramp.0, ramp.1)?;
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
                        // Off its own outer edge at peek 0, in place at 1.
                        let slide = (1. - peek) * band.size.w * if prev { -1. } else { 1. };
                        elements.push(TextureRenderElement::from_texture_buffer(
                            buffer,
                            Point::from((band.loc.x + slide, band.loc.y)),
                            alpha,
                            None,
                            None,
                            Kind::Unspecified,
                        ));
                    }
                    Err(err) => tracing::error!("error baking a page-preview hint: {err:#}"),
                }
            }

            // --- The preview itself: the adjacent pages' tiles, translated so that the
            //     previous page's last column and the next page's first come to rest
            //     just inside their band (`_translatePreviousPageIcons` /
            //     `_translateNextPageIcons`, `appDisplay.js:311-362`). They travel a
            //     whole page width, so at peek 0 they are off the output entirely —
            //     which is the clip GNOME gets from `clip_to_allocation`. ---
            let cell = layout.metrics.size().h;
            let neighbours = [
                (layout.page.checked_sub(1), {
                    // Last column's right edge one spacing short of the band's inside.
                    let at =
                        layout.hints[0].loc.x + layout.hints[0].size.w - layout.h_sp - block.size.w;
                    at - (1. - peek) * area.size.w
                }),
                (
                    (layout.page + 1 < layout.n_pages).then(|| layout.page + 1),
                    layout.hints[1].loc.x + layout.h_sp + (1. - peek) * area.size.w,
                ),
            ];
            for (page, block_x) in neighbours {
                let Some(page) = page else { continue };
                let first = page * layout.per_page;
                let Some(page_entries) = self
                    .entries
                    .get(first..(first + layout.per_page).min(self.entries.len()))
                else {
                    continue;
                };
                let rel: Vec<Rectangle<f64, Logical>> = (0..page_entries.len())
                    .map(|k| {
                        let (r, c) = (k / layout.cols, k % layout.cols);
                        Rectangle::new(
                            Point::from((
                                c as f64 * (cell + layout.h_sp),
                                r as f64 * (cell + layout.v_sp),
                            )),
                            layout.metrics.size(),
                        )
                    })
                    .collect();
                let origin = Point::from((block_x, block.loc.y));

                for (k, entry) in page_entries.iter().enumerate() {
                    let tile = Rectangle::new(origin + rel[k].loc, rel[k].size);
                    // Everything but the edge column is off the output; skip it.
                    if !tile.overlaps(area) {
                        continue;
                    }
                    if let Some(el) = widget::app_icon_element(
                        renderer,
                        &mut cache.icons,
                        app_icons,
                        &entry.icon,
                        metrics.icon_px,
                        scale,
                        Point::from((0., 0.)),
                        metrics.icon_center(tile),
                        alpha,
                    ) {
                        elements.push(el);
                    }
                }

                // The captions ride one bake per neighbouring page, keyed on the page
                // and the catalog — so the whole slide repositions a cached texture.
                let names: Vec<String> = page_entries
                    .iter()
                    .map(|e| {
                        widget::tile_label_lines(&e.name, LABEL_PT, label_w, false)
                            .into_iter()
                            .next()
                            .unwrap_or_default()
                    })
                    .collect();
                let revision = widget::Revision::new().of(page).of(self.content_rev).done();
                let paint_rects = rel.clone();
                match widget::bake(
                    renderer,
                    cache.peek_bakes.entry(page).or_default(),
                    scale,
                    block.size,
                    revision,
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
                        for (rect, label) in paint_rects.iter().zip(labels.iter()) {
                            p.labelled_tile(*rect, label, &metrics, false, style::TEXT)?;
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
                            origin,
                            alpha,
                            None,
                            None,
                            Kind::Unspecified,
                        ));
                    }
                    Err(err) => tracing::error!("error baking a page preview: {err:#}"),
                }
            }
        } else {
            cache.peek_bakes.clear();
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
                // The *active* page has to be in here. It used to ride `content_rev`,
                // which bumped on every page change; per-page bakes took that bump away
                // and left the strip frozen with the first dot lit.
                widget::Revision::new().of(self.content_rev).of(page).done(),
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
        for (is_next, disc) in [(false, layout.prev_arrow), (true, layout.next_arrow)] {
            let Some(disc) = disc else { continue };
            // The chevron glyph (topmost — pushed before its wash), its own upload cache.
            let name = if is_next {
                "carousel-arrow-next-symbolic"
            } else {
                "carousel-arrow-previous-symbolic"
            };
            {
                if let Some(tb) =
                    sym_icons.texture(renderer, name, ARROW_ICON_PX, scale, style::TEXT)
                {
                    let logical = tb.logical_size();
                    let center =
                        Point::from((disc.loc.x + disc.size.w / 2., disc.loc.y + disc.size.h / 2.));
                    let loc = center - Point::from((logical.w / 2., logical.h / 2.));
                    elements.push(TextureRenderElement::from_texture_buffer(
                        tb,
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
            folder: None,
        }
    }

    use std::time::Duration;

    fn grid_n(n: usize) -> AppGrid {
        grid_with_clock(n, Clock::with_time(Duration::ZERO))
    }

    fn grid_with_clock(n: usize, clock: Clock) -> AppGrid {
        let mut g = AppGrid::new(clock);
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
        // 96px icon → a 144 square tile, filling its square cell; one page, no dots.
        assert_eq!(l.tiles[0].size, Size::from((144., 144.)));
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

    /// The drop target classifies a point against the tile under it: the middle of a
    /// tile is `OnIcon` (nothing to insert between), each edge's 20px leeway is an
    /// insertion point, and past the last tile is an append (`iconGrid.js:1032-1120`).
    /// Above or below the rows is `INVALID`, i.e. no target at all.
    #[test]
    fn the_drop_target_reads_edges_body_and_empty_space() {
        let g = grid_n(3);
        let area = wide();
        let l = g.layout(area);
        let (t0, t2) = (l.tiles[0], l.tiles[2]);
        let mid_y = t0.loc.y + t0.size.h / 2.;
        let at = |x: f64, y: f64| g.drop_target_at(Point::from((x, y)), area, "nothing");

        let body = at(t0.loc.x + t0.size.w / 2., mid_y).expect("over a tile");
        assert_eq!(body.location, DragLocation::OnIcon);

        // Leading edge of the *third* tile: nothing is being dragged out of the grid
        // here, so the reflow pushes right and the target stays where it was.
        let lead = at(t2.loc.x + 5., mid_y).expect("a tile edge");
        assert_eq!(
            (lead.position, lead.location),
            (Some(2), DragLocation::StartEdge)
        );
        // Trailing edge of the first tile retargets to the next tile's leading edge —
        // the reflow can push tile 1 away, tile 0 it cannot.
        let trail = at(t0.loc.x + t0.size.w - 5., mid_y).expect("a tile edge");
        assert_eq!(
            (trail.position, trail.location),
            (Some(1), DragLocation::StartEdge)
        );

        // Past the last tile of the row, still within the rows: append.
        let past = at(t2.loc.x + t2.size.w + 200., mid_y).expect("empty space");
        assert_eq!(
            (past.position, past.location),
            (None, DragLocation::EmptySpace)
        );

        // Above and below the rows there is no target at all.
        assert!(at(t0.loc.x + 5., l.block.loc.y - 5.).is_none());
        assert!(at(t0.loc.x + 5., l.block.loc.y + l.block.size.h + 5.).is_none());
    }

    /// Dragging an icon *rightwards* flips which side of a tile counts as the
    /// insertion point: the icons behind it reflow left, so a hit on a tile's leading
    /// edge really means "after its neighbour" (`appDisplay.js:1180-1198`).
    #[test]
    fn a_forward_drag_retargets_the_edge_it_cannot_push() {
        let g = grid_n(4);
        let area = wide();
        let l = g.layout(area);
        let mid_y = l.tiles[0].loc.y + l.tiles[0].size.h / 2.;
        let dragged = g.entry_id(0).unwrap().to_owned();

        // Leading edge of tile 2 while dragging tile 0: retargeted back onto tile 1's
        // trailing edge, which is the slot that can actually open.
        let t = g
            .drop_target_at(Point::from((l.tiles[2].loc.x + 5., mid_y)), area, &dragged)
            .expect("a tile edge");
        assert_eq!((t.position, t.location), (Some(1), DragLocation::EndEdge));
    }

    /// A move is a remove-then-insert *in the shortened list*, so dragging forward
    /// lands one short of the raw index — the difference between "insert before what
    /// used to be here" and "insert at this index" (`IconGrid.moveItem`).
    #[test]
    fn moving_an_entry_reindexes_against_the_shortened_list() {
        let mut g = grid_n(4);
        let ids = |g: &AppGrid| -> Vec<String> { g.entries.iter().map(|e| e.id.clone()).collect() };
        let target = |position: Option<usize>| GridDropTarget {
            page: 0,
            position,
            location: DragLocation::StartEdge,
        };

        assert!(g.move_entry("app00.desktop", target(Some(2)), 24));
        assert_eq!(
            ids(&g),
            [
                "app01.desktop",
                "app02.desktop",
                "app00.desktop",
                "app03.desktop"
            ]
        );
        // An append target (`EmptySpace`, GNOME's -1) goes to the end of the page.
        assert!(g.move_entry("app00.desktop", target(None), 24));
        assert_eq!(*ids(&g).last().unwrap(), "app00.desktop");
        // A move that changes nothing reports nothing.
        assert!(!g.move_entry("app00.desktop", target(None), 24));
    }

    /// The live reflow is provisional: a drag nobody accepted puts every icon back
    /// (`_onDragCancelled` → `_redisplay`, `appDisplay.js:979-984`), and an accepted
    /// one reports whether the arrangement is worth writing back.
    #[test]
    fn a_cancelled_reorder_restores_the_order() {
        let mut g = grid_n(4);
        let ids = |g: &AppGrid| -> Vec<String> { g.entries.iter().map(|e| e.id.clone()).collect() };
        let before = ids(&g);
        let target = GridDropTarget {
            page: 0,
            position: Some(3),
            location: DragLocation::StartEdge,
        };

        g.begin_reorder();
        assert!(g.move_entry("app00.desktop", target, 24));
        assert_ne!(ids(&g), before);
        assert!(g.cancel_reorder());
        assert_eq!(ids(&g), before);

        g.begin_reorder();
        assert!(!g.finish_reorder(), "an untouched drag saves nothing");
        g.begin_reorder();
        assert!(g.move_entry("app00.desktop", target, 24));
        assert!(g.finish_reorder(), "a real move is worth persisting");
        assert!(!g.cancel_reorder(), "…and cannot then be undone");
    }

    /// `_savePages` writes one dict per page, in display order.
    #[test]
    fn pages_chunk_the_order_for_persistence() {
        let g = grid_n(5);
        assert_eq!(
            g.pages(2),
            vec![
                vec!["app00.desktop".to_owned(), "app01.desktop".to_owned()],
                vec!["app02.desktop".to_owned(), "app03.desktop".to_owned()],
                vec!["app04.desktop".to_owned()],
            ]
        );
    }

    /// The preview bands are only a target while a drag is in flight, and only in a
    /// direction that leads somewhere — except the *next* one, which stays live on the
    /// last page because dropping there is what makes a new page
    /// (`_syncPageIndicatorsVisibility`, `appDisplay.js:270-274`).
    #[test]
    fn the_preview_bands_are_targets_only_while_a_drag_is_in_flight() {
        let mut clock = Clock::with_time(Duration::ZERO);
        let mut g = grid_with_clock(30, clock.clone());
        let area = wide();
        let mid_y = area.loc.y + area.size.h / 2.;
        let band = indicators_w(area.size.w);
        let left = Point::from((area.loc.x + band / 2., mid_y));
        let right = Point::from((area.loc.x + area.size.w - band / 2., mid_y));

        assert_eq!(g.hint_at(left, area), None, "no drag, no bands");
        assert_eq!(g.hint_at(right, area), None);

        assert!(g.set_drag_active(true));
        assert!(g.are_animations_ongoing(), "the previews slide in");
        clock.set_unadjusted(Duration::from_millis(PAGE_PREVIEW_MS));
        assert!(!g.are_animations_ongoing());

        assert_eq!(
            g.hint_at(left, area),
            None,
            "page 0 has no previous page to preview"
        );
        assert_eq!(g.hint_at(right, area), Some(PageArrow::Next));
        assert_eq!(
            g.hint_at(Point::from((area.loc.x + area.size.w / 2., mid_y)), area),
            None,
            "the middle of the grid is not a band"
        );

        g.set_page(1, area);
        assert_eq!(g.hint_at(left, area), Some(PageArrow::Prev));
        assert_eq!(
            g.hint_at(right, area),
            Some(PageArrow::Next),
            "the last page still previews forward — that is how a page is created"
        );

        assert!(g.set_drag_active(false));
        clock.set_unadjusted(Duration::from_millis(2 * PAGE_PREVIEW_MS));
        assert_eq!(g.hint_at(right, area), None, "the drag ended");
    }

    /// `indicatorsPadding`: 10% of the band on each side, floored at an arrow's own
    /// width, reserved *outside* the grid's page padding and permanently — not only
    /// while something is being dragged (`appDisplay.js:162-171,405-430`). The grid
    /// content lays out inside the remainder and the arrows sit in the bands.
    #[test]
    fn the_page_preview_bands_are_reserved_out_of_the_grid() {
        let area = wide(); // 1920 wide
        let l = grid_n(30).layout(area);
        let band = 1920. * 0.20 / 2.;
        assert_eq!(indicators_w(area.size.w), band);
        let right_band_x = area.loc.x + area.size.w - band;
        // Nothing of the grid reaches into a band, and the page padding is on top of it.
        assert!(l.block.loc.x >= area.loc.x + band + PAGE_PAD_H);
        assert!(l.block.loc.x + l.block.size.w <= right_band_x - PAGE_PAD_H);
        // The next arrow is centered in its band, on both axes.
        let arrow = l.next_arrow.expect("30 apps paginate");
        assert_eq!(arrow.loc.x + ARROW_DISC / 2., right_band_x + band / 2.);
        let page_h = area.size.h - INDICATORS_STRIP_H;
        assert_eq!(
            arrow.loc.y + ARROW_DISC / 2.,
            (area.loc.y + page_h / 2.).round()
        );

        // On a narrow band the share would be thinner than an arrow, so the arrow wins.
        assert_eq!(indicators_w(500.), ARROW_DISC + 2. * ARROW_MARGIN);
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
    fn a_moving_tile_fade_does_not_bump_the_bake_revision() {
        // The folder dialog's source tile fades over 200ms, i.e. a new alpha every frame.
        // Which tile fades changes what the shared bakes contain (the faded one leaves
        // them), so that bumps the revision; the alpha alone must not, or the page's text
        // re-shapes on every frame of the animation — the per-frame-bake bug class.
        let mut g = grid_n(30);
        let rev = g.content_rev();

        assert!(g.set_tile_fade(Some(("app02.desktop".to_owned(), 1.))));
        assert_ne!(
            g.content_rev(),
            rev,
            "a tile joining the fade leaves the shared bakes, so they must re-bake"
        );

        let rev = g.content_rev();
        for alpha in [0.75, 0.5, 0.25, 0.] {
            assert!(
                g.set_tile_fade(Some(("app02.desktop".to_owned(), alpha))),
                "a new alpha still reports a change (→ redraw)"
            );
            assert_eq!(
                g.content_rev(),
                rev,
                "…but must not re-bake: it moves every frame"
            );
        }

        assert!(g.set_tile_fade(Some(("app03.desktop".to_owned(), 0.5))));
        assert_ne!(g.content_rev(), rev, "a different tile re-bakes");
        let rev = g.content_rev();
        assert!(g.set_tile_fade(None));
        assert_ne!(g.content_rev(), rev, "and so does clearing it");
        assert!(!g.set_tile_fade(None), "an unchanged fade is not a change");
    }

    #[test]
    fn hover_does_not_bump_the_bake_revision() {
        // The label bake is keyed on `content_rev`; a hover change must not touch it, or
        // every mouse move during the open animation re-shapes the page's labels (the
        // stutter). A *page* change no longer touches it either — each page has its own
        // bake now (a slide has two on screen at once), so switching back and forth
        // re-bakes nothing after the first visit. Only the entries changing does.
        let mut g = grid_n(30);
        let area = wide();
        let rev = g.content_rev();
        assert!(
            g.set_hovered(Some(2)),
            "a new hover still reports a change (→ redraw)"
        );
        assert!(g.set_arrow_hovered(Some(PageArrow::Next)));
        assert_eq!(
            g.content_rev(),
            rev,
            "tile/arrow hover must not invalidate the label bake"
        );
        assert!(g.set_page(1, area));
        assert_eq!(
            g.content_rev(),
            rev,
            "a page change must not invalidate the other page's bake — each page keeps \
             its own, which is what lets both be on screen during a slide"
        );
        assert!(g.set_entries(vec![entry("a", "a")]));
        assert_ne!(g.content_rev(), rev, "an entries change re-bakes");
    }

    #[test]
    fn stale_hover_is_dropped_on_shrink() {
        let mut g = grid_n(3);
        assert!(g.set_hovered(Some(2)));
        assert!(g.set_entries(vec![entry("a", "a")]));
        assert_eq!(g.hovered, None);
    }
}
