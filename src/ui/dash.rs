//! The overview dash — the favorites bar (`js/ui/dash.js`).
//!
//! A rounded background pill at the bottom-center of the overview holding the
//! user's favorite apps (`AppFavorites`, via [`crate::app_system::AppSystem`]),
//! each a full-color [`widget::AppIcon`] tile, followed by a trailing "show apps"
//! button. Clicking a favorite launches it and closes the overview.
//!
//! **Scope (S3 + S6, `docs/fork/overview-port.md`):** favorites, then running
//! non-favorites behind a `.dash-separator` (`Dash._redisplay`, `dash.js:677-699`,
//! `806-808`), each flagged with a running dot. Dash icons have no label
//! (`showLabel:false`, `dash.js:26`); the hover tooltip is deferred. The show-apps
//! button renders for fidelity but its toggle (→ APP_GRID) is S8; its clicks are
//! consumed inertly.
//!
//! **Drag affordances.** The dash is a drop target for app-icon drags from the grid
//! and the search results: [`drop_slot_at`](Dash::drop_slot_at) opens a gap that
//! follows the pointer and a drop pins the app there (`Dash.handleDragOver` /
//! `acceptDrop`). The show-apps button doubles as the *unpin* target for the duration
//! of a drag ([`unpin_target_at`](Dash::unpin_target_at), `ShowAppsIcon.acceptDrop`) —
//! the only way to unpin by dragging. GNOME also relabels that button "Unpin" while it
//! is armed; we have no dash tooltip at all yet, so the arming shows only as its hover
//! fill.
//!
//! **S6 divergence — clicking a *running* app relaunches it.** GNOME's
//! `AppIcon.activate` calls `shell_app_activate`, which for a running app raises
//! its most recent window instead of spawning a second copy. We have no
//! window-activation path from a desktop id yet, so every dash tile launches. That
//! is deferred; it needs `RunningApp` to carry window ids and a focus action.
//!
//! **Input divergences (S3):** a right-click on a GNOME dash icon opens the app
//! context menu (`AppIconMenu`); we consume it inertly (the menu is a later slice).
//! The dash is mouse-only for now: touch taps fall through to the overview's touch
//! grab (the panel has the same gap). Both are revisited when the relevant
//! gesture/menu slices land. (Activation itself is GNOME's: like every St.Button
//! these act on the *release*, and only if it lands on the same icon — see
//! `State::activate_overview_hit`.)
//!
//! **Divergences from GNOME, revisited by S5's `ControlsManagerLayout` port:** the
//! placement is a hardcoded bottom-center anchor (S5 gives it an allocated box and a
//! `setMaxSize`-driven icon size); the icon size is fixed at 64 (`dash.js:321`, the
//! largest `baseIconSizes` — `_adjustIconSize` only shrinks under space pressure,
//! which no desktop monitor hits); the overview transition is an alpha fade only
//! (GNOME also slides the dash via the state adjustment); and, like our panel, it
//! draws on every output rather than the primary only. All geometry lives behind
//! [`Dash::layout`] so S5 swaps the allocator without touching hit-testing or the
//! tile primitive.
//!
//! Colors/sizes are cited to the 50.1 theme (`_dash.scss`, `_common.scss`,
//! `_drawing.scss`, `_colors.scss`); see the constants below.

use std::cell::RefCell;

use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::{ContextId, Renderer};
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Size, Transform};

use crate::app_system::AppIconRef;
use crate::render_helpers::icon::{AppIconCache, IconCache};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::ui::theme_node::{allocate_1d, Align1, Edges, ThemeNode};
use crate::ui::widget::{self, AppIcon, AppIconUploads, Painter};

/// Dash icon size, logical px (`this.iconSize = 64`, `dash.js:321`).
pub(crate) const ICON_PX: f64 = 64.;
/// The `.overview-icon` tile side (icon + `%tile` padding, `_common.scss:86`).
const TILE: f64 = ICON_PX + 2. * AppIcon::PADDING; // 76
/// Per-item side margin (`margin: 0 $dash_spacing`, `_dash.scss:54`).
const ITEM_MARGIN: f64 = 2.;
/// Per-item advance: tile + its margin on both sides.
const ITEM_ADVANCE: f64 = TILE + 2. * ITEM_MARGIN; // 80
/// Pill horizontal padding: `$base_padding·2 − item margin` (`_dash.scss:22-25`).
const PILL_PAD_H: f64 = 10.;
/// Pill vertical padding above/below the tiles (`_dash.scss:22-25`).
const PILL_PAD_V: f64 = 12.;
/// Pill height: tile + vertical padding both sides.
const PILL_H: f64 = TILE + 2. * PILL_PAD_V; // 100
/// Pill corner radius: `$modal_radius + $base_padding·2` (`_dash.scss:9,21`).
const PILL_RADIUS: f64 = 28.;
/// Gap from the screen bottom edge (`margin-bottom` = `$dash_edge_offset`,
/// `_dash.scss:8,95-99`).
const MARGIN_BOTTOM: f64 = 12.;

/// What the dash asks [`crate::ui::overview_layout`] for: the pill plus the
/// gap below it. gnome-shell caps this at `DASH_MAX_HEIGHT_RATIO` of the work
/// area and shrinks the icons to fit (`Dash._adjustIconSize`); we take the cap
/// but keep the icon size, so on a very short screen the pill overflows its
/// box upward rather than getting smaller.
pub const PREFERRED_HEIGHT: f64 = PILL_H + MARGIN_BOTTOM;

/// `$dash_background_color = mix(#222226, #fafafb, 90%)` (`_dash.scss:20`,
/// `_colors.scss:50`, `_default-colors.scss:4-5`) ≈ `#38383B`.
const DASH_BG: [f32; 4] = [0.218, 0.218, 0.233, 1.];

/// The `.dash-background` pill as a [`ThemeNode`] (`_dash.scss:19-25`): its padding
/// wraps the icon run into the pill, and the box model derives the pill size
/// ([`ThemeNode::allocation_for`]) and the run's origin ([`ThemeNode::content_box`])
/// so those numbers aren't hand-summed. The icon tile itself is [`AppIcon`] (the
/// `.overview-icon` primitive); only the pill needed modelling.
const DASH_BACKGROUND: ThemeNode = ThemeNode {
    padding: Edges::symmetric(PILL_PAD_V, PILL_PAD_H),
    border: Edges::ZERO,
    border_color: [0., 0., 0., 0.],
    border_radius: PILL_RADIUS,
    background: Some(DASH_BG),
    width: None,
    height: None,
};
/// The tile hover fill: `st-lighten($dash_background_color, 7%)` (flat + always-dark,
/// `_drawing.scss:186-189,270-274`). Lightens (the per-widget hover direction).
const TILE_HOVER: [f32; 4] = [0.286, 0.286, 0.305, 1.];
/// The show-apps glyph color: `$system_fg_color` ≈ `#fafafb` (`_dash.scss:57,62`).
const SHOW_APPS_FG: [f32; 4] = [0.980, 0.980, 0.984, 1.];
/// The show-apps button glyph (`view-app-grid-symbolic`, `dash.js:216`).
const SHOW_APPS_ICON: &str = "view-app-grid-symbolic";

/// The empty-dash drop target's side (`$dash_placeholder_size`, `_dash.scss:6,42-45`).
/// Space only: `.empty-dash-drop-target` sets a width and a height and nothing else.
const EMPTY_DROP_TARGET_PX: f64 = 32.;

/// Separator line width (`.dash-separator`, `_dash.scss:84`).
const SEPARATOR_W: f64 = 1.;
/// Separator side margins (`$base_margin`, `_dash.scss:85-86`).
const SEPARATOR_MARGIN: f64 = 4.;
/// Horizontal space one separator takes from the item run.
const SEPARATOR_ADVANCE: f64 = SEPARATOR_W + 2. * SEPARATOR_MARGIN; // 9
/// Separator height (`height: this.iconSize`, `dash.js:813`).
const SEPARATOR_H: f64 = ICON_PX;
/// `$system_borders_color = transparentize($system_fg_color, .9)` — white at 10%
/// (`_colors.scss:48`, `_dash.scss:87`).
const SEPARATOR_COLOR: [f32; 4] = [1., 1., 1., 0.1];

/// Running-dot side (`.app-grid-running-dot`, `_app-grid.scss:46-47`).
const DOT_PX: f64 = 5.;
/// The dot's `offset-y` in the dash — `-$dash_padding` (`_dash.scss:72-78`),
/// applied as `translationY` (`AppIcon._updateDotStyle`, `appDisplay.js:3002`).
/// The dot is `y_align: END` within the button, which `y_expand`s to fill the
/// whole dash-background, so this lifts it that far above the **pill's** bottom
/// edge (into the gap below the icon), not the icon tile's.
const DOT_OFFSET_Y: f64 = 12.;
/// The dot fill: `$system_fg_color` (`_app-grid.scss:49`).
const DOT_COLOR: [f32; 4] = [0.980, 0.980, 0.984, 1.];

/// One app in the dash — a plain-data snapshot (not a live catalog borrow).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashEntry {
    pub id: String,
    pub name: String,
    pub icon: AppIconRef,
    /// Whether the app has an open window — draws the running dot
    /// (`AppIcon._updateRunningStyle`, `appDisplay.js:3007`).
    pub running: bool,
}

/// What a point over the dash hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DashHit {
    /// App at index `.0` — favorites first, then running non-favorites.
    App(usize),
    /// The trailing show-apps button.
    ShowApps,
    /// The pill background (padding / gaps / separator) — consumes the click, no
    /// action.
    Background,
}

/// Computed geometry for one output size: pill box + per-item tile boxes (absolute,
/// logical). Item `favorites.len()` is the show-apps button. Feeds both drawing and
/// hit-testing from one place (the panel `items`/`hit_test`-agree invariant).
struct DashLayout {
    pill: Rectangle<f64, Logical>,
    /// Tile boxes; `[0, n)` apps, `[n]` the show-apps button.
    tiles: Vec<Rectangle<f64, Logical>>,
    n_items: usize,
    /// The favorites/running divider, when one is drawn.
    separator: Option<Rectangle<f64, Logical>>,
}

impl DashLayout {
    fn icon_center(&self, i: usize) -> Point<f64, Logical> {
        let r = self.tiles[i];
        Point::from((r.loc.x + r.size.w / 2., r.loc.y + r.size.h / 2.))
    }
}

#[derive(Default)]
struct DashCache {
    context: Option<ContextId<VkTexture>>,
    /// The pill chrome (background + separator + hover fill), keyed
    /// `(scale, phys, revision)`.
    bake: widget::BakeCache,
    /// The running dots, baked separately because they draw *over* the icons
    /// (`_dot` is added to `_iconContainer` after the icon, `appDisplay.js:2964`)
    /// while the pill chrome draws under them.
    dots: widget::BakeCache,
    /// Full-color favorite icon uploads.
    icons: AppIconUploads,
}

/// The overview dash. Owned on `Niri`; fed by `sync_dash_apps`.
pub struct Dash {
    /// Favorites first, then running non-favorites (`Dash._redisplay`,
    /// `dash.js:677-699`).
    items: Vec<DashEntry>,
    /// How many leading `items` are favorites — where the separator goes.
    n_favorites: usize,
    /// Bumped when `items` changes — the bake revision's content part.
    content_rev: u64,
    /// While a drag hovers the dash, the favourites index the drop would land at.
    /// The run opens a one-tile gap there so the icons part around the incoming app
    /// (gnome-shell's `_dragPlaceholder`, `dash.js:926-932`). The placeholder is a
    /// pure gap — 50.1's `.placeholder` has `background-image: none`
    /// (`_dash.scss:35-40`), so there is nothing to paint, only space to make.
    drop_slot: Option<usize>,
    /// Whether an app-icon drag is in flight anywhere. Only the empty dash cares: with
    /// no icons there is nothing to aim at, so gnome-shell reserves a placeholder-sized
    /// target for the duration of the drag (`EmptyDropTargetItem`, inserted in
    /// `_onItemDragBegin` and dropped in `_endItemDrag`, `dash.js:410-414,429-434`).
    /// Like the gap it is pure space — `.empty-dash-drop-target` sets only a size.
    drag_active: bool,
    hovered: Option<DashHit>,
    cache: RefCell<DashCache>,
}

impl Default for Dash {
    fn default() -> Self {
        Self::new()
    }
}

impl Dash {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            n_favorites: 0,
            content_rev: 0,
            drop_slot: None,
            drag_active: false,
            hovered: None,
            cache: RefCell::new(DashCache::default()),
        }
    }

    /// Replace the app snapshot: `items` is favorites (the first `n_favorites`)
    /// followed by running non-favorites. Returns whether it changed (bumping the
    /// bake revision so the pill re-bakes).
    pub fn set_items(&mut self, items: Vec<DashEntry>, n_favorites: usize) -> bool {
        if items == self.items && n_favorites == self.n_favorites {
            return false;
        }
        self.items = items;
        self.n_favorites = n_favorites;
        self.content_rev = self.content_rev.wrapping_add(1);
        // `hovered` is a positional index; a content change (pin/unpin/reorder from
        // gsettings, or an app starting/stopping) can make it point at a different
        // app or past the end. Clear it — the next pointer motion re-establishes it —
        // so a stale index can't light the wrong tile or an out-of-range one.
        self.hovered = None;
        true
    }

    /// Open (or move, or close) the drop gap. Returns whether it changed — the gap
    /// widens the pill, so the caller re-bakes and redraws.
    pub fn set_drop_slot(&mut self, slot: Option<usize>) -> bool {
        if self.drop_slot == slot {
            return false;
        }
        self.drop_slot = slot;
        true
    }

    /// The open drop gap, if any — what a drop lands on.
    pub fn drop_slot(&self) -> Option<usize> {
        self.drop_slot
    }

    /// Tell the dash an app-icon drag began or ended. Returns whether it changed: on an
    /// *empty* dash this reserves (or releases) the drop target that gives the drag
    /// something to aim at, which resizes the pill.
    pub fn set_drag_active(&mut self, active: bool) -> bool {
        if self.drag_active == active {
            return false;
        }
        self.drag_active = active;
        // Only an empty dash changes shape, but the flag is cheap and the caller
        // redraws for the drag anyway.
        self.items.is_empty()
    }

    /// Where a drag carrying `dragged_id` would drop, as an index into the favourites,
    /// or `None` for "not a drop" — the pointer is off the dash, or the position is a
    /// no-op for an app already pinned there.
    ///
    /// gnome-shell's `Dash.handleDragOver` (`dash.js:860-937`): the run is divided into
    /// equal shares and the pointer picks one. Two rules on top: a drop past the last
    /// favourite clamps back to it (the running-apps zone is not pinnable), and a
    /// favourite dropped immediately before or after itself is a no-op rather than a
    /// reorder.
    ///
    /// The share *width* is measured with the placeholder and the separator excluded
    /// (`dash.js:878-891`) so the gap the pointer just opened does not move the next
    /// answer — but the pointer still ranges over the whole, widened box. That
    /// asymmetry is not sloppiness, it is what makes the slot *past* the last favourite
    /// reachable: `_box` grows by the placeholder, and the strip that growth adds at the
    /// right end is the only place `pos == numFavorites` comes from. So appending to a
    /// dash with no running apps takes two moves — open a gap anywhere, then slide right
    /// into the strip — exactly as it does in GNOME.
    pub fn drop_slot_at(
        &self,
        pos: Point<f64, Logical>,
        area: Rectangle<f64, Logical>,
        dragged_id: &str,
    ) -> Option<usize> {
        // The *current* layout, gap included: this measures where the pointer is, and
        // the pointer sees the dash as it is drawn.
        let layout = self.layout(area);
        let pill = layout.pill;
        if pos.x < pill.loc.x || pos.x >= pill.loc.x + pill.size.w || pos.y < pill.loc.y {
            return None;
        }

        let run = DASH_BACKGROUND.content_box(pill);
        // gnome-shell's drop target is `_box`, which holds the icons, the separator and
        // the placeholder — but *not* the trailing show-apps button, a sibling inside
        // `_dashContainer` (`dash.js:338-356`). So the button's share of our run is not
        // a slot; it is the unpin target instead (see [`unpin_target_at`]).
        let box_w = run.size.w - ITEM_ADVANCE;
        let rel = pos.x - run.loc.x;
        if rel < 0. || rel >= box_w {
            return None;
        }

        // "Always insert at the start when dash is empty" (`dash.js:894-895`) — with no
        // icons the box is just the reserved target, and all of it is slot 0. (Not
        // reachable without one: an empty dash with no drag has `box_w == 0`.)
        if self.items.is_empty() {
            return Some(0);
        }

        // ...and `boxWidth`/`numChildren` then take out the placeholder and separator.
        let separator_space = if self.separator_after().is_some() {
            SEPARATOR_ADVANCE
        } else {
            0.
        };
        let gap_w = if self.drop_slot.is_some() {
            ITEM_ADVANCE
        } else {
            0.
        };
        let measured_w = box_w - separator_space - gap_w;
        let n_children = self.items.len();
        if measured_w <= 0. || n_children == 0 {
            return None;
        }

        // Past the favourites is not a pin target; clamp to the end of them.
        let slot = ((rel * n_children as f64 / measured_w).floor() as usize).min(self.n_favorites);

        // "Don't allow positioning before or after self" (`dash.js:909-913`).
        if let Some(from) = self.favorite_index(dragged_id) {
            if slot == from || slot == from + 1 {
                return None;
            }
        }
        Some(slot)
    }

    /// Whether a drag carrying `dragged_id` is over the *unpin* target.
    ///
    /// The show-apps button doubles as the remove target for the duration of an
    /// app-icon drag: gnome-shell relabels it "Unpin" and hovers it
    /// (`ShowAppsIcon.setDragApp`, `dash.js:236-247`), and its `acceptDrop` removes the
    /// favourite (`dash.js:256-270`). Only for an app that is actually pinned —
    /// `_canRemoveApp` (`dash.js:224-234`) requires `isFavorite`, so dragging a fresh
    /// app from the grid onto the button does nothing.
    ///
    /// Deliberately tested against the *current* (gap-included) layout: the button
    /// slides right as the gap opens, and gnome-shell's drag monitor likewise asks
    /// whether the pointer is inside the actor where it now sits (`dash.js:441-442`).
    pub fn unpin_target_at(
        &self,
        pos: Point<f64, Logical>,
        area: Rectangle<f64, Logical>,
        dragged_id: &str,
    ) -> bool {
        self.favorite_index(dragged_id).is_some()
            && self.hit_test(pos, area) == Some(DashHit::ShowApps)
    }

    /// Where `id` sits among the favourites, if it is one.
    pub fn favorite_index(&self, id: &str) -> Option<usize> {
        self.items[..self.n_favorites.min(self.items.len())]
            .iter()
            .position(|item| item.id == id)
    }

    /// The desktop id of app `i`, if present.
    pub fn item_id(&self, i: usize) -> Option<&str> {
        self.items.get(i).map(|e| e.id.as_str())
    }

    /// The icon of app `i`, if present (what a drag of that tile carries).
    pub fn item_icon(&self, i: usize) -> Option<&crate::app_system::AppIconRef> {
        self.items.get(i).map(|e| &e.icon)
    }

    /// Every item's icon — for the startup decode prewarm (`Niri::prewarm_app_icons`).
    pub fn icon_refs(&self) -> impl Iterator<Item = &crate::app_system::AppIconRef> {
        self.items.iter().map(|e| &e.icon)
    }

    /// Set the hovered element; returns whether it changed (→ redraw + re-bake).
    pub fn set_hovered(&mut self, hovered: Option<DashHit>) -> bool {
        if self.hovered == hovered {
            return false;
        }
        self.hovered = hovered;
        true
    }

    /// Drop cached icon uploads (e.g. on `installed-changed`, where an app's icon
    /// may now resolve differently).
    pub fn clear_icon_uploads(&self) {
        let mut cache = self.cache.borrow_mut();
        cache.icons.clear();
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

    /// How many icon uploads are cached. Only the tests look at this: dropping them
    /// is invisible in a still frame (the worker re-decodes and they come back), but
    /// it blanks every tile in between, so a test needs to see the drop itself.
    #[cfg(test)]
    pub fn icon_upload_count(&self) -> usize {
        self.cache.borrow().icons.len()
    }

    /// Lay out the dash within its allocated `box` (logical, output coords;
    /// [`crate::ui::overview_layout`] bottom-anchors it to the work area): the
    /// centered pill and its tiles, with the pill's own gap below it. Always at
    /// least the show-apps button, so the pill is never empty (GNOME renders it
    /// unconditionally, `dash.js:352-356`).
    /// Whether a separator is drawn, and after how many items: GNOME draws it iff
    /// there is at least one favorite *and* at least one non-favorite icon
    /// (`nFavorites > 0 && nFavorites < nIcons`, `dash.js:806-808`). `nIcons`
    /// counts app icons only — the show-apps button lives outside `_box`
    /// (`dash.js:350-356`), so it never triggers a separator.
    fn separator_after(&self) -> Option<usize> {
        (self.n_favorites > 0 && self.n_favorites < self.items.len()).then_some(self.n_favorites)
    }

    /// Width the empty-dash drop target reserves in the run, if it is up: only while a
    /// drag is in flight *and* there are no icons at all (`dash.js:410-414`). Without it
    /// an empty dash is a bare show-apps button with nowhere to drop the first
    /// favourite.
    fn empty_drop_target_w(&self) -> f64 {
        if self.drag_active && self.items.is_empty() {
            EMPTY_DROP_TARGET_PX
        } else {
            0.
        }
    }

    fn layout(&self, area: Rectangle<f64, Logical>) -> DashLayout {
        let n = self.items.len();
        let count = n + 1; // + show-apps
        let drop_slot = self.drop_slot;
        let gap = if drop_slot.is_some() {
            ITEM_ADVANCE
        } else {
            0.
        };
        let empty_target = self.empty_drop_target_w();
        let separator_after = self.separator_after();
        let separator_space = if separator_after.is_some() {
            SEPARATOR_ADVANCE
        } else {
            0.
        };

        // The pill is the dash-background node wrapped around the icon run (its
        // content): width = the run, height = one tile; padding adds the rest.
        let run_w = ITEM_ADVANCE * count as f64 + separator_space + gap + empty_target;
        let pill_size = DASH_BACKGROUND.allocation_for(Size::from((run_w, TILE)));
        let pill_x = (area.loc.x + (area.size.w - pill_size.w) / 2.).round();
        let pill_y = (area.loc.y + area.size.h - MARGIN_BOTTOM - pill_size.h).round();
        let pill = Rectangle::new(Point::from((pill_x, pill_y)), pill_size);

        // The icon run occupies the pill's content box (pill minus padding).
        let run = DASH_BACKGROUND.content_box(pill);
        // Items after the separator are pushed right by its advance.
        let shift = |k: usize| match separator_after {
            Some(at) if k >= at => separator_space,
            _ => 0.,
        };
        // Items at or after the drop slot are pushed right by the gap.
        let gap_shift = |k: usize| match drop_slot {
            Some(at) if k >= at => gap,
            _ => 0.,
        };
        // The empty drop target is inserted at the front of the box (`dash.js:412`), so
        // everything — which is only ever the show-apps button — follows it.
        let tiles = (0..count)
            .map(|k| {
                let tile_left = run.loc.x
                    + ITEM_ADVANCE * k as f64
                    + shift(k)
                    + gap_shift(k)
                    + empty_target
                    + ITEM_MARGIN;
                Rectangle::new(
                    Point::from((tile_left, run.loc.y)),
                    Size::from((TILE, TILE)),
                )
            })
            .collect();

        let separator = separator_after.map(|at| {
            let x = run.loc.x + ITEM_ADVANCE * at as f64 + gap_shift(at) + SEPARATOR_MARGIN;
            // `.dash-separator` is iconSize-tall, centred on the tile row.
            let (y, h) = allocate_1d(run.loc.y, TILE, SEPARATOR_H, Align1::Center);
            Rectangle::new(Point::from((x, y)), Size::from((SEPARATOR_W, h)))
        });

        DashLayout {
            pill,
            tiles,
            n_items: n,
            separator,
        }
    }

    /// Which element is under `pos` (logical, output coords), or `None`. Click
    /// targets extend down to the screen bottom edge (`padding-bottom`,
    /// `_dash.scss:47,55`); the pill's side pads are `Background`.
    pub fn hit_test(
        &self,
        pos: Point<f64, Logical>,
        area: Rectangle<f64, Logical>,
    ) -> Option<DashHit> {
        let layout = self.layout(area);
        let pill = layout.pill;
        // On the dash iff within the pill's x-range and at/below its top edge.
        if pos.x < pill.loc.x || pos.x >= pill.loc.x + pill.size.w || pos.y < pill.loc.y {
            return None;
        }
        // The reactive tile extends *down* to the screen edge (`padding-bottom`,
        // `_dash.scss:47,55`) but has no top extension: the pill's top padding band
        // (pill top → tile top) is non-reactive background, like GNOME's `#dash` pad.
        if pos.y < pill.loc.y + PILL_PAD_V {
            return Some(DashHit::Background);
        }
        // Indexed off the laid-out rects rather than by arithmetic on the run, so every
        // band the layout can insert — the separator, and the drop gap, which slides
        // everything after it by a whole advance — is inert here for free. Each tile
        // owns its advance slot: the rect plus the `0 2px` margin either side.
        layout
            .tiles
            .iter()
            .position(|tile| {
                pos.x >= tile.loc.x - ITEM_MARGIN && pos.x < tile.loc.x + tile.size.w + ITEM_MARGIN
            })
            .map_or(Some(DashHit::Background), |k| {
                Some(if k < layout.n_items {
                    DashHit::App(k)
                } else {
                    DashHit::ShowApps
                })
            })
    }

    /// The logical center of tile `i` within `area` — favorites are
    /// `[0, n)`, the trailing show-apps button is `[n]`. Where a drag of that tile
    /// picks the icon up from, and a geometry probe for the conformance corpus,
    /// which clicks real pixels routed through [`hit_test`](Self::hit_test).
    /// `None` if out of range.
    pub fn tile_center(
        &self,
        i: usize,
        area: Rectangle<f64, Logical>,
    ) -> Option<Point<f64, Logical>> {
        let layout = self.layout(area);
        (i < layout.tiles.len()).then(|| layout.icon_center(i))
    }

    /// The trailing show-apps button's index (= the app count).
    #[cfg(test)]
    pub fn show_apps_index(&self) -> usize {
        self.items.len()
    }

    /// The background pill's box within `area` (for the corpus).
    #[cfg(test)]
    pub fn pill_box(&self, area: Rectangle<f64, Logical>) -> Rectangle<f64, Logical> {
        self.layout(area).pill
    }

    /// The separator box within `area`, if one is drawn (for the corpus).
    #[cfg(test)]
    pub fn separator_box(&self, area: Rectangle<f64, Logical>) -> Option<Rectangle<f64, Logical>> {
        self.layout(area).separator
    }

    /// The running dot's box for tile `i` — centered on the tile horizontally,
    /// its bottom edge `DOT_OFFSET_Y` above the **pill's** bottom.
    ///
    /// GNOME's `.app-grid-running-dot` is `y_align: END` inside the icon button,
    /// which `y_expand`s to fill the whole dash-background (`appDisplay.js:2955-2961`,
    /// `dash.js:150`); its `offset-y: -$dash_padding` then lifts it that far off the
    /// bottom (`_dash.scss:72-78`). So the dot lands in the gap **below** the icon,
    /// not over it — the reference edge is the pill, not the 76px icon tile.
    fn dot_box(
        tile: Rectangle<f64, Logical>,
        pill: Rectangle<f64, Logical>,
    ) -> Rectangle<f64, Logical> {
        // `x_align: CENTER` on the tile, `y_align: END` in the pill-filling button,
        // then the `-$dash_padding` `offset-y` translation lifts it off the bottom.
        let (x, w) = allocate_1d(tile.loc.x, tile.size.w, DOT_PX, Align1::Center);
        let (y, h) = allocate_1d(pill.loc.y, pill.size.h, DOT_PX, Align1::End);
        Rectangle::new(Point::from((x, y - DOT_OFFSET_Y)), Size::from((w, h)))
    }

    /// The running dot's box for app `i` within `area`, if it is running.
    #[cfg(test)]
    pub fn dot_box_for(
        &self,
        i: usize,
        area: Rectangle<f64, Logical>,
    ) -> Option<Rectangle<f64, Logical>> {
        let layout = self.layout(area);
        self.items
            .get(i)
            .filter(|e| e.running)
            .map(|_| Self::dot_box(layout.tiles[i], layout.pill))
    }

    /// The currently-hovered element (for the conformance corpus).
    #[cfg(test)]
    pub fn hovered_for_test(&self) -> Option<DashHit> {
        self.hovered
    }

    /// The dash render elements for `output`, faded by overview `progress` (0..1).
    /// Icons are pushed first (topmost); the pill chrome last (below them) — the
    /// panel first-topmost order.
    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        app_icons: &AppIconCache,
        sym_icons: &IconCache,
        output: &Output,
        area: Rectangle<f64, Logical>,
        progress: f64,
    ) -> Vec<TextureRenderElement<VkTexture>> {
        let scale = output.current_scale().fractional_scale();
        let layout = self.layout(area);
        let alpha = progress as f32;

        let mut cache = self.cache.borrow_mut();
        // Cached uploads belong to one renderer context; drop them if it changed.
        let context = renderer.context_id();
        if cache.context.as_ref() != Some(&context) {
            cache.icons.clear();
            cache.context = Some(context);
        }

        let mut elements = Vec::with_capacity(layout.tiles.len() + 2);

        // The running dots — topmost, because GNOME adds `_dot` to the icon
        // container *after* the icon (`appDisplay.js:2955-2964`) and the dash
        // `offset-y` lifts it onto the icon's lower edge. Its own bake layer: the
        // pill chrome underneath the icons cannot carry something that must draw
        // over them. Skipped entirely when nothing is running.
        if self.items.iter().any(|e| e.running) {
            let dots: Vec<Rectangle<f64, Logical>> = self
                .items
                .iter()
                .enumerate()
                .filter(|(_, e)| e.running)
                .map(|(i, _)| {
                    let d = Self::dot_box(layout.tiles[i], layout.pill);
                    Rectangle::new(d.loc - layout.pill.loc, d.size)
                })
                .collect();
            let texture = widget::bake(
                renderer,
                &mut cache.dots,
                scale,
                layout.pill.size,
                self.content_rev,
                |_| Ok(()),
                |frame, phys, ()| {
                    let mut p = Painter::new(frame, scale, phys);
                    p.clear(widget::style::TRANSPARENT)?;
                    for dot in &dots {
                        p.fill_rounded(*dot, DOT_PX / 2., DOT_COLOR)?;
                    }
                    Ok(())
                },
            );
            match texture {
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
                        layout.pill.loc,
                        alpha,
                        None,
                        None,
                        Kind::Unspecified,
                    ));
                }
                Err(err) => tracing::error!("error baking the dash running dots: {err:#}"),
            }
        }

        // App icons, on their tiles.
        for (i, entry) in self.items.iter().enumerate() {
            if let Some(el) = widget::app_icon_element(
                renderer,
                &mut cache.icons,
                app_icons,
                &entry.icon,
                ICON_PX,
                scale,
                Point::from((0., 0.)),
                layout.icon_center(i),
                alpha,
            ) {
                elements.push(el);
            }
        }

        // The show-apps symbolic glyph. Built by hand rather than via `icon_element`
        // because it fades with `progress` and `icon_element` hardcodes alpha 1.
        {
            if let Some(tb) =
                sym_icons.texture(renderer, SHOW_APPS_ICON, ICON_PX, scale, SHOW_APPS_FG)
            {
                let logical = tb.logical_size();
                let center = layout.icon_center(layout.n_items);
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

        // The pill chrome (background + separator + running dots + the hovered
        // tile's fill), baked + cached.
        let hovered_tile = match self.hovered {
            Some(DashHit::App(k)) if k < layout.n_items => Some(layout.tiles[k]),
            Some(DashHit::ShowApps) => layout.tiles.last().copied(),
            _ => None,
        };
        // revision = content | hover-tile index (None = 0, else index+1).
        let hover_code = hovered_tile
            .map(|_| match self.hovered {
                Some(DashHit::App(k)) => k as u64 + 1,
                Some(DashHit::ShowApps) => layout.n_items as u64 + 1,
                _ => 0,
            })
            .unwrap_or(0);
        let revision = (self.content_rev << 20) | (hover_code & 0xf_ffff);

        let pill_origin = layout.pill.loc;
        // The bake buffer *is* the pill, so its local box is the pill at the origin.
        let pill_local = Rectangle::new(Point::from((0., 0.)), layout.pill.size);
        let texture = widget::bake(
            renderer,
            &mut cache.bake,
            scale,
            layout.pill.size,
            revision,
            |_| Ok(()),
            |frame, phys, ()| {
                let mut p = Painter::new(frame, scale, phys);
                p.clear(widget::style::TRANSPARENT)?;
                DASH_BACKGROUND.paint(&mut p, pill_local)?;

                // The favorites/running divider. `hairline` *clears* rather than
                // blends, so the translucent `$system_borders_color` has to be
                // pre-blended onto the pill or it would punch a hole in it.
                if let Some(sep) = layout.separator {
                    let rel = Rectangle::new(sep.loc - pill_origin, sep.size);
                    p.hairline(rel, widget::style::over(DASH_BG, SEPARATOR_COLOR))?;
                }

                if let Some(tile) = hovered_tile {
                    // Tile box relative to the pill origin.
                    let rel = Rectangle::new(tile.loc - pill_origin, tile.size);
                    p.app_tile(
                        &AppIcon {
                            rect: rel,
                            hovered: true,
                            // The dash styles the inner `.overview-icon` as a
                            // plain `%tile` (`_dash.scss:60-63`), not the outer
                            // `.overview-tile` the app grid uses.
                            radius: AppIcon::RADIUS,
                        },
                        TILE_HOVER,
                    )?;
                }
                Ok(())
            },
        );
        match texture {
            Ok(texture) => {
                // Rounded + faded: no opaque hint.
                let buffer = TextureBuffer::from_texture(
                    renderer,
                    texture,
                    scale,
                    Transform::Normal,
                    vec![],
                );
                elements.push(TextureRenderElement::from_texture_buffer(
                    buffer,
                    layout.pill.loc,
                    alpha,
                    None,
                    None,
                    Kind::Unspecified,
                ));
            }
            Err(err) => tracing::error!("error baking the dash: {err:#}"),
        }

        elements
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dash_with(n: usize) -> Dash {
        let mut dash = Dash::new();
        let items = (0..n).map(|i| entry(&format!("app{i}.desktop"))).collect();
        dash.set_items(items, n);
        dash
    }

    /// A dash with `n_fav` favorites followed by `n_running` running non-favorites.
    fn dash_with_running(n_fav: usize, n_running: usize) -> Dash {
        let mut dash = Dash::new();
        let mut items: Vec<DashEntry> = (0..n_fav)
            .map(|i| entry(&format!("fav{i}.desktop")))
            .collect();
        for i in 0..n_running {
            items.push(DashEntry {
                running: true,
                ..entry(&format!("run{i}.desktop"))
            });
        }
        dash.set_items(items, n_fav);
        dash
    }

    fn entry(id: &str) -> DashEntry {
        DashEntry {
            id: id.to_owned(),
            name: id.to_owned(),
            icon: AppIconRef::Fallback,
            running: false,
        }
    }

    /// The box `overview_layout` allocates the dash on 1920×1080 with the 35px
    /// panel strut: bottom-anchored, `PREFERRED_HEIGHT` tall.
    fn box_1080() -> Rectangle<f64, Logical> {
        Rectangle::new(
            Point::from((0., 1080. - PREFERRED_HEIGHT)),
            Size::from((1920., PREFERRED_HEIGHT)),
        )
    }

    /// Every tile's center hit-tests back to that tile; side pads are Background.
    #[test]
    fn hit_test_round_trips_layout() {
        let dash = dash_with(3);
        let area = box_1080();
        let layout = dash.layout(area);
        for i in 0..3 {
            assert_eq!(
                dash.hit_test(layout.icon_center(i), area),
                Some(DashHit::App(i))
            );
        }
        // The show-apps button is the trailing tile.
        assert_eq!(
            dash.hit_test(layout.icon_center(3), area),
            Some(DashHit::ShowApps)
        );
        // The pill's left padding is Background, not a favorite.
        let pad = Point::from((layout.pill.loc.x + 2., layout.pill.loc.y + PILL_H / 2.));
        assert_eq!(dash.hit_test(pad, area), Some(DashHit::Background));
        // Well outside the pill: no hit.
        assert_eq!(dash.hit_test(Point::from((10., 10.)), area), None);
    }

    /// The pill's top padding band (pill top → tile top) is non-reactive
    /// background, not the tile above it (GNOME's tile has no top extension).
    #[test]
    fn hit_test_top_padding_is_background() {
        let dash = dash_with(2);
        let area = box_1080();
        let layout = dash.layout(area);
        let cx = layout.icon_center(0).x;
        // 1px below the pill's top edge, over favorite 0's column: still padding.
        let top_band = Point::from((cx, layout.pill.loc.y + 1.));
        assert_eq!(dash.hit_test(top_band, area), Some(DashHit::Background));
        // Just inside the tile top it becomes the favorite.
        let tile_top = Point::from((cx, layout.pill.loc.y + PILL_PAD_V + 1.));
        assert_eq!(dash.hit_test(tile_top, area), Some(DashHit::App(0)));
    }

    /// A favorites change clears the (positional) hover so a stale index can't
    /// light the wrong tile.
    #[test]
    fn set_favorites_clears_hover() {
        let mut dash = dash_with(3);
        assert!(dash.set_hovered(Some(DashHit::App(2))));
        assert_eq!(dash.hovered, Some(DashHit::App(2)));
        // Shrinking to one favorite would leave index 2 dangling — must clear.
        dash.set_items(vec![entry("only.desktop")], 1);
        assert_eq!(dash.hovered, None, "a favorites change clears the hover");
    }

    /// Click targets extend to the bottom screen edge (`padding-bottom`).
    #[test]
    fn hit_test_extends_to_screen_bottom() {
        let dash = dash_with(2);
        let area = box_1080();
        let layout = dash.layout(area);
        let cx = layout.icon_center(0).x;
        // A click at the very bottom edge, under favorite 0, still hits it.
        assert_eq!(
            dash.hit_test(Point::from((cx, 1080. - 1.)), area),
            Some(DashHit::App(0))
        );
    }

    /// Even with no favorites the pill exists with just the show-apps button.
    #[test]
    fn empty_favorites_still_has_show_apps() {
        let dash = dash_with(0);
        let area = box_1080();
        let layout = dash.layout(area);
        assert_eq!(layout.tiles.len(), 1);
        assert_eq!(
            dash.hit_test(layout.icon_center(0), area),
            Some(DashHit::ShowApps)
        );
    }

    #[test]
    fn set_favorites_reports_change() {
        let mut dash = Dash::new();
        assert!(dash.set_items(vec![entry("a.desktop")], 1));
        assert!(!dash.set_items(vec![entry("a.desktop")], 1));
        assert!(
            dash.set_items(
                vec![DashEntry {
                    running: true,
                    ..entry("a.desktop")
                }],
                1
            ),
            "an app starting is a change — the running dot appears"
        );
    }

    /// The running dot lands in the gap *below* the icon, not over it: its bottom
    /// edge is `DOT_OFFSET_Y` above the **pill's** bottom (GNOME lifts the
    /// pill-filling icon button's `y_align: END` dot by `-$dash_padding`,
    /// `_dash.scss:72-78`, `appDisplay.js:2955-2961`). The prior bug referenced the
    /// 76px icon tile's bottom instead, drawing the dot on the icon's lower third.
    #[test]
    fn running_dot_sits_in_the_gap_below_the_icon() {
        let area = box_1080();
        let dash = dash_with_running(1, 1); // fav0, then the running app at index 1
        let layout = dash.layout(area);
        let pill = layout.pill;
        let dot = dash
            .dot_box_for(1, area)
            .expect("the running app has a dot");

        // Bottom edge is DOT_OFFSET_Y above the pill bottom — the pin.
        assert_eq!(
            dot.loc.y + dot.size.h,
            pill.loc.y + pill.size.h - DOT_OFFSET_Y
        );
        // Centered on its tile horizontally.
        let tile = layout.tiles[1];
        assert_eq!(dot.loc.x + dot.size.w / 2., tile.loc.x + tile.size.w / 2.);
        // ...and strictly below the icon's canvas — in the gap, not on the icon.
        let icon_bottom = layout.icon_center(1).y + ICON_PX / 2.;
        assert!(
            dot.loc.y >= icon_bottom,
            "dot top {} must be at/below the icon bottom {icon_bottom}",
            dot.loc.y
        );
    }

    /// The `DASH_BACKGROUND` theme-node reproduces the pill's hand-summed constants:
    /// height is padding-only (`TILE + 2·PILL_PAD_V = PILL_H`), width is the run plus
    /// horizontal padding, and its content box insets by exactly the padding. Pins
    /// the node ⇄ const equivalence so a drift in either is caught.
    #[test]
    fn dash_background_node_matches_the_pill_constants() {
        let size = DASH_BACKGROUND.allocation_for(Size::from((100., TILE)));
        assert_eq!(size.h, PILL_H);
        assert_eq!(size.w, 100. + 2. * PILL_PAD_H);

        let pill = Rectangle::new(Point::from((0., 0.)), size);
        let run = DASH_BACKGROUND.content_box(pill);
        assert_eq!(run.loc, Point::from((PILL_PAD_H, PILL_PAD_V)));
        assert_eq!(run.size, Size::from((100., TILE)));
    }

    /// The separator is drawn only when there is at least one favorite *and* at
    /// least one running non-favorite (`nFavorites > 0 && nFavorites < nIcons`,
    /// `dash.js:806-808`), and it takes its own horizontal space.
    #[test]
    fn separator_only_between_favorites_and_running() {
        let area = box_1080();

        let both = dash_with_running(2, 1);
        let with_sep = both.layout(area);
        let sep = with_sep
            .separator
            .expect("favorites + running draws a divider");
        assert_eq!(sep.size, Size::from((SEPARATOR_W, SEPARATOR_H)));

        // It sits between the last favorite and the first running app.
        assert!(sep.loc.x >= with_sep.tiles[1].loc.x + with_sep.tiles[1].size.w);
        assert!(sep.loc.x + sep.size.w <= with_sep.tiles[2].loc.x);
        // ...and is vertically centered on the tile row.
        let tile = with_sep.tiles[0];
        assert_eq!(
            sep.loc.y + sep.size.h / 2.,
            tile.loc.y + tile.size.h / 2.,
            "the divider is centered on the icon row"
        );

        // Favorites only, and running only, both draw none.
        assert!(dash_with_running(3, 0).layout(area).separator.is_none());
        assert!(dash_with_running(0, 2).layout(area).separator.is_none());
    }

    /// The divider widens the pill by exactly its advance — the same three app
    /// icons laid out with and without it differ by `SEPARATOR_ADVANCE`.
    #[test]
    fn separator_widens_the_pill_by_its_advance() {
        let area = box_1080();
        let without = dash_with_running(3, 0).layout(area).pill.size.w;
        let with = dash_with_running(2, 1).layout(area).pill.size.w;
        assert_eq!(with - without, SEPARATOR_ADVANCE);
    }

    /// Every tile still hit-tests back to itself across the divider, and the
    /// divider's own band is inert background.
    #[test]
    fn separator_band_is_inert_and_does_not_shift_hits() {
        let dash = dash_with_running(2, 2);
        let area = box_1080();
        let layout = dash.layout(area);

        for i in 0..4 {
            assert_eq!(
                dash.hit_test(layout.icon_center(i), area),
                Some(DashHit::App(i)),
                "tile {i} round-trips across the divider"
            );
        }
        assert_eq!(
            dash.hit_test(layout.icon_center(4), area),
            Some(DashHit::ShowApps)
        );

        let sep = layout.separator.unwrap();
        let on_sep = Point::from((sep.loc.x + sep.size.w / 2., layout.icon_center(0).y));
        assert_eq!(
            dash.hit_test(on_sep, area),
            Some(DashHit::Background),
            "the divider consumes its click but does nothing"
        );
    }
}
