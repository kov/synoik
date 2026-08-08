// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

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
use synoik_config::CornerRadius;

use crate::animation::{Animation, Clock, Curve};
use crate::app_system::AppIconRef;
use crate::render_helpers::background_effect::RenderParams;
use crate::render_helpers::blur::{BlurOptions, Finish};
use crate::render_helpers::framebuffer_effect::{FramebufferEffect, FramebufferEffectElement};
use crate::render_helpers::icon::{AppIconCache, IconCache};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::synoik_render_elements;
use crate::ui::theme_node::{allocate_1d, Align1, Edges, ThemeNode};
use crate::ui::widget::{self, AppIcon, Painter, SharedAppIconUploads};

/// Dash icon size, logical px (`this.iconSize = 64`, `dash.js:321`) — the largest rung
/// of [`ICON_LADDER`], and what every canvas at or above the adaptive-chrome reference
/// gets.
pub(crate) const ICON_PX: f64 = 64.;

/// gnome-shell's `baseIconSizes` (`dash.js:19`): `_adjustIconSize` picks the largest rung
/// that fits the box it was allocated, rather than any size in between.
const ICON_LADDER: [f64; 6] = [16., 22., 24., 32., 48., 64.];

/// Per-item side margin (`margin: 0 $dash_spacing`, `_dash.scss:54`) at [`ICON_PX`].
const ITEM_MARGIN: f64 = 2.;
/// Pill horizontal padding: `$base_padding·2 − item margin` (`_dash.scss:22-25`).
const PILL_PAD_H: f64 = 10.;
/// Pill vertical padding above/below the tiles (`_dash.scss:22-25`).
const PILL_PAD_V: f64 = 12.;
/// Pill corner radius: `$modal_radius + $base_padding·2` (`_dash.scss:9,21`).
const PILL_RADIUS: f64 = 28.;
/// Gap from the screen bottom edge (`margin-bottom` = `$dash_edge_offset`,
/// `_dash.scss:8,95-99`).
const MARGIN_BOTTOM: f64 = 12.;

/// Every dash length, for one icon size.
///
/// **Adaptive chrome, rule 2 — ramped** (`docs/fork/adaptive-overview-chrome.md`). GNOME's
/// dash is a flat 64px icon and it only shrinks on *overflow* (`_adjustIconSize`,
/// `dash.js:321`); on a small canvas that leaves a pill wider than half the screen sitting
/// under an app grid whose own icons have laddered down to 32. Here the icon additionally
/// follows the canvas — and everything the pill is built from follows the icon, so the
/// dash keeps its proportions rather than growing fat padding around a small icon.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DashMetrics {
    /// Icon side, a rung of [`ICON_LADDER`].
    pub icon_px: f64,
    /// The `.overview-icon` tile side (icon + `%tile` padding, `_common.scss:86`).
    pub tile: f64,
    /// Per-item side margin.
    pub item_margin: f64,
    /// Per-item advance: tile + its margin on both sides.
    pub item_advance: f64,
    /// Pill padding, horizontal and vertical.
    pub pill_pad_h: f64,
    pub pill_pad_v: f64,
    /// Pill height: tile + vertical padding both sides.
    pub pill_h: f64,
    /// Pill corner radius.
    pub pill_radius: f64,
    /// Gap from the screen bottom edge.
    pub margin_bottom: f64,
}

impl DashMetrics {
    /// The lengths for `icon_px`, every one of them proportional to it — which is what
    /// keeps a shrunk dash looking like the dash.
    pub fn for_icon(icon_px: f64) -> Self {
        let k = icon_px / ICON_PX;
        let tile = icon_px + 2. * AppIcon::PADDING * k;
        let item_margin = ITEM_MARGIN * k;
        let pill_pad_v = PILL_PAD_V * k;
        Self {
            icon_px,
            tile,
            item_margin,
            item_advance: tile + 2. * item_margin,
            pill_pad_h: PILL_PAD_H * k,
            pill_pad_v,
            pill_h: tile + 2. * pill_pad_v,
            pill_radius: PILL_RADIUS * k,
            margin_bottom: MARGIN_BOTTOM * k,
        }
    }

    /// GNOME's own metrics — the 64px dash, unramped.
    pub fn gnome() -> Self {
        Self::for_icon(ICON_PX)
    }

    /// The largest rung whose pill plus bottom margin fits `height`.
    ///
    /// This is `_adjustIconSize`'s shape (`dash.js:321-372`) and it is why the ramp needs
    /// no new plumbing: [`crate::ui::overview_layout`] already reserves the dash the band
    /// it asked for, so sizing to the band it was *given* picks up the ramp and GNOME's
    /// own `DASH_MAX_HEIGHT_RATIO` overflow shrink at once.
    pub fn fitting(height: f64) -> Self {
        let icon = ICON_LADDER
            .iter()
            .rev()
            .copied()
            .find(|&icon| Self::for_icon(icon).preferred_height() <= height)
            .unwrap_or(ICON_LADDER[0]);
        Self::for_icon(icon)
    }

    /// What the dash asks [`crate::ui::overview_layout`] for: the pill plus the gap below
    /// it. gnome-shell caps this at `DASH_MAX_HEIGHT_RATIO` of the work area
    /// (`overviewControls.js:22`), which is the overflow half of the shrink.
    pub fn preferred_height(&self) -> f64 {
        self.pill_h + self.margin_bottom
    }

    /// The `.dash-background` pill as a [`ThemeNode`] (`_dash.scss:19-25`) at this size:
    /// its padding wraps the icon run into the pill, and the box model derives the pill
    /// size ([`ThemeNode::allocation_for`]) and the run's origin
    /// ([`ThemeNode::content_box`]) so those numbers aren't hand-summed. The icon tile
    /// itself is [`AppIcon`] (the `.overview-icon` primitive); only the pill needed
    /// modelling.
    ///
    /// The fill here is the dark appearance's; the paint path overwrites `background` with the
    /// live one ([`widget::style::Appearance::plate`]). Every other caller wants the box model,
    /// not the colour.
    fn background(&self) -> ThemeNode {
        ThemeNode {
            padding: Edges::symmetric(self.pill_pad_v, self.pill_pad_h),
            border: Edges::ZERO,
            border_color: [0., 0., 0., 0.],
            border_radius: self.pill_radius,
            // A placeholder; the paint path overwrites it with the live plate.
            background: Some(dash_bg(widget::style::Appearance::default())),
            width: None,
            height: None,
        }
    }
}

/// What the dash asks for on a canvas of `view_size`: [`ICON_PX`] shrunk by the chrome
/// ramp, snapped down to a ladder rung.
pub fn preferred_height(view_size: Size<f64, Logical>) -> f64 {
    let ramp = crate::ui::overview_layout::chrome_ramp(view_size);
    let icon = ICON_LADDER
        .iter()
        .rev()
        .copied()
        .find(|&icon| icon <= ICON_PX * ramp)
        .unwrap_or(ICON_LADDER[0]);
    DashMetrics::for_icon(icon).preferred_height()
}

/// The `.dash-background` pill's fill, for the appearance the shell is drawn in. GNOME's is
/// opaque `$dash_background_color = mix(#222226, #fafafb, 90%)` ≈ `#38383B` (`_dash.scss:20`,
/// `_colors.scss:50`, `_default-colors.scss:4-5`); ours is the shared translucent plate, so the
/// blurred backdrop reads through it — see [`widget::style::Appearance::plate`] for why.
fn dash_bg(appearance: widget::style::Appearance) -> [f32; 4] {
    appearance.plate()
}

/// The dock pill's backdrop blur — the panel's, so the two read as the same material. See
/// [`Dash::render`] for why the overview's dash does not get one.
const PILL_BLUR: BlurOptions = crate::ui::panel::BAR_BLUR;

/// The tile hover fill. GNOME's is `st-lighten($dash_background_color, 7%)` (flat + always-dark,
/// `_drawing.scss:186-189,270-274`) — an *absolute* colour derived from an opaque pill, which over
/// a translucent plate would be an opaque patch on it. A relative wash is the same gesture and
/// composes: the shared [`widget::style::HOVER_WASH`], which is already what the app grid's tiles
/// use for this (10% white where GNOME lightens 4%), so the two hovers now match.
const TILE_HOVER: [f32; 4] = widget::style::HOVER_WASH;
/// The urgency glow behind an app icon demanding attention: one accent drop shadow, no offset.
/// Ours, not GNOME's — see the note in [`Dash::render`].
const GLOW_BLUR: f64 = 14.;
const GLOW_ALPHA: f32 = 0.8;
/// Transparent margin the glow bake needs on every side: a drop shadow bleeds ~1.5·blur (3σ)
/// past its box, so the buffer has to hold that fringe or the outermost icons' halos clip flat
/// against the edge.
const GLOW_PAD: f64 = GLOW_BLUR * 1.5;

/// The show-apps glyph color: `$system_fg_color` ≈ `#fafafb` (`_dash.scss:57,62`).
const SHOW_APPS_FG: [f32; 4] = [0.980, 0.980, 0.984, 1.];
/// The show-apps button glyph (`view-app-grid-symbolic`, `dash.js:216`).
const SHOW_APPS_ICON: &str = "view-app-grid-symbolic";

/// The empty-dash drop target's side (`$dash_placeholder_size`, `_dash.scss:6,42-45`).
/// Space only: `.empty-dash-drop-target` sets a width and a height and nothing else.
const EMPTY_DROP_TARGET_PX: f64 = 32.;

/// The drop gap's open width at [`ICON_PX`]. gnome-shell sizes the placeholder's child to
/// `iconSize` (`dash.js:927`) — narrower than the tile it makes room for, so the run parts
/// by a bare icon's width, not a whole advance. The animation runs in these units and the
/// layout scales it to the dash's actual icon size.
const PLACEHOLDER_W: f64 = ICON_PX;
/// `DASH_ANIMATION_TIME` (`dash.js:16`); the curve is `EASE_OUT_QUAD` (`dash.js:164`).
const GAP_ANIMATION_MS: u64 = 200;

/// An animation that is already over, resting at `value` — the gap at rest.
fn settled(clock: &Clock, value: f64) -> Animation {
    Animation::ease(clock.clone(), value, value, 0., 0, Curve::EaseOutQuad)
}

/// Separator line width (`.dash-separator`, `_dash.scss:84`).
const SEPARATOR_W: f64 = 1.;
/// Separator side margins (`$base_margin`, `_dash.scss:85-86`).
const SEPARATOR_MARGIN: f64 = 4.;
/// Horizontal space one separator takes from the item run.
const SEPARATOR_ADVANCE: f64 = SEPARATOR_W + 2. * SEPARATOR_MARGIN; // 9
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
    /// Whether one of its windows is demanding attention.
    ///
    /// **Divergence:** gnome-shell shows urgency only as a notification
    /// (`windowAttentionHandler.js`) and puts nothing on the dash. We poke the icon above the
    /// bottom edge instead — see `Dock`.
    pub urgent: bool,
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
    /// The lengths this layout was built from — the dash sizes itself to its band.
    metrics: DashMetrics,
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
    /// The poke highlights — the plate behind each urgent icon while the dock rests part-way
    /// out. Its own layer, and it must be: it draws *under* the icons where the dots draw over
    /// them, and reusing either cache for a second kind of content under the same revision key is
    /// how a bake goes stale.
    poke: widget::BakeCache,
    /// Full-color favorite icon uploads.
    icons: SharedAppIconUploads,
}

// The dash draws a texture (the baked pill chrome and the composited icons) and, when it is
// standing in for the dock, a `Backdrop` — the blurred capture of whatever is behind the pill,
// under everything else. See [`Dash::render`] for why only the dock gets one.
synoik_render_elements! {
    DashElement => {
        Texture = TextureRenderElement<VkTexture>,
        Backdrop = FramebufferEffectElement,
    }
}

/// The overview dash. Owned on `Synoik`; fed by `sync_dash_apps`.
pub struct Dash {
    /// Favorites first, then running non-favorites (`Dash._redisplay`,
    /// `dash.js:677-699`).
    items: Vec<DashEntry>,
    /// How many leading `items` are favorites — where the separator goes.
    n_favorites: usize,
    /// Bumped when `items` changes — the bake revision's content part.
    content_rev: u64,
    /// While a drag hovers the dash, the favourites index the drop would land at.
    /// The run opens a gap there so the icons part around the incoming app
    /// (gnome-shell's `_dragPlaceholder`, `dash.js:926-932`). The placeholder is a
    /// pure gap — 50.1's `.placeholder` has `background-image: none`
    /// (`_dash.scss:35-40`), so there is nothing to paint, only space to make.
    drop_slot: Option<usize>,
    /// The gap's width, eased 0 ⇄ [`PLACEHOLDER_W`] as it opens and closes. Moving an
    /// open gap between slots is *not* animated: gnome-shell destroys the placeholder
    /// and re-shows it with `fadeIn = false` (`dash.js:918-932`), so only the first
    /// appearance and the disappearance take time.
    ///
    /// This does re-bake the pill chrome on every frame it moves, because the pill's
    /// size is part of the bake key. That is *not* the animated-property-in-the-key bug
    /// class ([[animation-per-frame-bake]]) — the pill really is a different width each
    /// frame, so there is nothing to reuse. Making it free would mean drawing the pill
    /// as a rounded-rect render element instead of a baked texture, which is a bigger
    /// change than this gesture justifies.
    gap: Animation,
    /// Where the gap's space is inserted. Tracks `drop_slot` while one is open, and is
    /// *retained* while the gap collapses — gnome-shell's placeholder is still a child
    /// at its index while it animates out, so the icons after it stay parted until it
    /// is gone.
    gap_slot: usize,
    /// Whether an app-icon drag is in flight anywhere. Only the empty dash cares: with
    /// no icons there is nothing to aim at, so gnome-shell reserves a placeholder-sized
    /// target for the duration of the drag (`EmptyDropTargetItem`, inserted in
    /// `_onItemDragBegin` and dropped in `_endItemDrag`, `dash.js:410-414,429-434`).
    /// Like the gap it is pure space — `.empty-dash-drop-target` sets only a size.
    drag_active: bool,
    hovered: Option<DashHit>,
    clock: Clock,
    cache: RefCell<DashCache>,
    /// Identity of the pill's backdrop-blur element, the dock's only. Holds no pixels — the
    /// capture and blur chain live in the damage tracker's per-`Id` user data, like the panel's.
    backdrop: FramebufferEffect,
}

impl Dash {
    pub fn new(clock: Clock) -> Self {
        Self {
            items: Vec::new(),
            n_favorites: 0,
            content_rev: 0,
            drop_slot: None,
            gap: settled(&clock, 0.),
            gap_slot: 0,
            drag_active: false,
            hovered: None,
            clock,
            cache: RefCell::new(DashCache::default()),
            backdrop: FramebufferEffect::new(),
        }
    }

    /// The current app snapshot, favorites first.
    pub fn items(&self) -> &[DashEntry] {
        &self.items
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
    ///
    /// Opening and closing are eased over [`GAP_ANIMATION_MS`]; *moving* an open gap is
    /// instant, because gnome-shell recreates the placeholder at the new index with
    /// `fadeIn = false` when one is already up (`dash.js:918-932`).
    pub fn set_drop_slot(&mut self, slot: Option<usize>) -> bool {
        if self.drop_slot == slot {
            return false;
        }
        let was_open = self.drop_slot.is_some();
        self.drop_slot = slot;
        if let Some(slot) = slot {
            self.gap_slot = slot;
        }
        match (was_open, slot.is_some()) {
            // Re-targeted from wherever it is now, not from 0: a gap that reopens while
            // it is still collapsing must not restart. (gnome-shell instead refuses to
            // move a placeholder while one animates out — `_animatingPlaceholdersCount`,
            // `dash.js:843-846,906` — which it needs because its placeholders are
            // separate actors and ours is one width.)
            (false, true) => self.gap = self.ease_gap_to(PLACEHOLDER_W),
            (true, false) => self.gap = self.ease_gap_to(0.),
            _ => (),
        }
        true
    }

    fn ease_gap_to(&self, to: f64) -> Animation {
        Animation::ease(
            self.clock.clone(),
            self.gap_w(),
            to,
            0.,
            GAP_ANIMATION_MS,
            Curve::EaseOutQuad,
        )
    }

    /// The gap's current width. Read by the layout *and* by the drop math, which is
    /// what keeps the two consistent while it is still opening.
    fn gap_w(&self) -> f64 {
        self.gap.clamped_value()
    }

    /// Whether the gap is still moving — keeps the redraw loop alive, since nothing
    /// else generates damage while the pointer holds still mid-animation.
    pub fn are_animations_ongoing(&self) -> bool {
        !self.gap.is_done()
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

        let run = layout.metrics.background().content_box(pill);
        // gnome-shell's drop target is `_box`, which holds the icons, the separator and
        // the placeholder — but *not* the trailing show-apps button, a sibling inside
        // `_dashContainer` (`dash.js:338-356`). So the button's share of our run is not
        // a slot; it is the unpin target instead (see [`unpin_target_at`]).
        let box_w = run.size.w - layout.metrics.item_advance;
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
        // Taking out the gap's *current* width, not its target, keeps the share size
        // constant while it eases open — otherwise the answer would drift under a
        // motionless pointer and the gap would chase itself across slots.
        let measured_w = box_w - separator_space - self.gap_w();
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

    /// Whether app `i` draws its running dot (`AppIcon._updateRunningStyle`,
    /// `appDisplay.js:3007`) — for the corpus.
    pub fn item_shows_running_dot(&self, i: usize) -> Option<bool> {
        self.items.get(i).map(|e| e.running)
    }

    /// The icon of app `i`, if present (what a drag of that tile carries).
    pub fn item_icon(&self, i: usize) -> Option<&crate::app_system::AppIconRef> {
        self.items.get(i).map(|e| &e.icon)
    }

    /// Every item's icon — for the startup decode prewarm (`Synoik::prewarm_app_icons`).
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

    /// Draw from `shared` instead of this surface's own upload map, so an icon already
    /// on the GPU for another surface is not uploaded again (see [`SharedAppIconUploads`]).
    pub fn share_icon_uploads(&self, shared: &SharedAppIconUploads) {
        self.cache.borrow_mut().icons = shared.clone();
    }

    /// The map this surface draws from.
    pub fn icon_uploads(&self) -> SharedAppIconUploads {
        self.cache.borrow().icons.clone()
    }

    /// Drop cached icon uploads (e.g. on `installed-changed`, where an app's icon
    /// may now resolve differently).
    pub fn clear_icon_uploads(&self) {
        self.cache.borrow().icons.borrow_mut().clear();
    }

    /// Drop one icon's uploads, so the next frame re-uploads it from the freshly
    /// decoded pixels — see [`widget::drop_app_icon_upload`].
    pub fn drop_icon_upload(&self, icon: &crate::app_system::AppIconRef, logical_px: u16) {
        crate::ui::widget::drop_app_icon_upload(
            &mut self.cache.borrow_mut().icons.borrow_mut(),
            icon,
            logical_px,
        );
    }

    /// How many icon uploads are cached. Only the tests look at this: dropping them
    /// is invisible in a still frame (the worker re-decodes and they come back), but
    /// it blanks every tile in between, so a test needs to see the drop itself.
    #[cfg(test)]
    pub fn icon_upload_count(&self) -> usize {
        self.cache.borrow().icons.borrow().len()
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

    /// The lengths for the band the dash was allocated (see [`DashMetrics::fitting`]).
    pub fn metrics(area: Rectangle<f64, Logical>) -> DashMetrics {
        DashMetrics::fitting(area.size.h)
    }

    fn layout(&self, area: Rectangle<f64, Logical>) -> DashLayout {
        let m = Self::metrics(area);
        let n = self.items.len();
        let count = n + 1; // + show-apps
                           // The gap and the empty-dash target are modelled in GNOME's units (the animation
                           // runs 0 ⇄ `PLACEHOLDER_W`), so they follow the icon down with everything else.
        let k = m.icon_px / ICON_PX;
        let gap = self.gap_w() * k;
        let empty_target = self.empty_drop_target_w() * k;
        let separator_after = self.separator_after();
        let separator_space = if separator_after.is_some() {
            SEPARATOR_ADVANCE
        } else {
            0.
        };

        // The pill is the dash-background node wrapped around the icon run (its
        // content): width = the run, height = one tile; padding adds the rest.
        let run_w = m.item_advance * count as f64 + separator_space + gap + empty_target;
        let pill_size = m.background().allocation_for(Size::from((run_w, m.tile)));
        let pill_x = (area.loc.x + (area.size.w - pill_size.w) / 2.).round();
        let pill_y = (area.loc.y + area.size.h - m.margin_bottom - pill_size.h).round();
        let pill = Rectangle::new(Point::from((pill_x, pill_y)), pill_size);

        // The icon run occupies the pill's content box (pill minus padding).
        let run = m.background().content_box(pill);
        // Items after the separator are pushed right by its advance.
        let shift = |k: usize| match separator_after {
            Some(at) if k >= at => separator_space,
            _ => 0.,
        };
        // Items at or after the gap are pushed right by it.
        let gap_slot = self.gap_slot;
        let gap_shift = |k: usize| if k >= gap_slot { gap } else { 0. };
        // The empty drop target is inserted at the front of the box (`dash.js:412`), so
        // everything — which is only ever the show-apps button — follows it.
        let tiles = (0..count)
            .map(|k| {
                let tile_left = run.loc.x
                    + m.item_advance * k as f64
                    + shift(k)
                    + gap_shift(k)
                    + empty_target
                    + m.item_margin;
                Rectangle::new(
                    Point::from((tile_left, run.loc.y)),
                    Size::from((m.tile, m.tile)),
                )
            })
            .collect();

        let separator = separator_after.map(|at| {
            let x = run.loc.x + m.item_advance * at as f64 + gap_shift(at) + SEPARATOR_MARGIN;
            // `.dash-separator` is iconSize-tall, centred on the tile row.
            let (y, h) = allocate_1d(run.loc.y, m.tile, m.icon_px, Align1::Center);
            Rectangle::new(Point::from((x, y)), Size::from((SEPARATOR_W, h)))
        });

        DashLayout {
            metrics: m,
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

    /// Narrow a [`hit_test`](Self::hit_test) result to what a *poking* dock actually draws:
    /// the icons demanding attention, and nothing else.
    ///
    /// Without this the invisible tiles keep their click targets, so pushing the pointer into
    /// the bottom edge to reach the one glowing icon can activate a favorite you cannot see —
    /// or open the app grid, which is not even drawn. `poking` comes from the dock rather than
    /// from a mirrored flag here: it depends on the slide animation's state, so a copy on the
    /// dash would go stale between refreshes.
    pub fn filter_poke(&self, hit: DashHit, poking: bool) -> Option<DashHit> {
        if !poking {
            return Some(hit);
        }
        match hit {
            DashHit::App(k) if self.items.get(k).is_some_and(|entry| entry.urgent) => Some(hit),
            _ => None,
        }
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

    /// Tile `i`'s rect within `area` — what a context menu anchors on.
    pub fn tile_rect(
        &self,
        i: usize,
        area: Rectangle<f64, Logical>,
    ) -> Option<Rectangle<f64, Logical>> {
        self.layout(area).tiles.get(i).copied()
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
        metrics: &DashMetrics,
    ) -> Rectangle<f64, Logical> {
        // `x_align: CENTER` on the tile, `y_align: END` in the pill-filling button,
        // then the `-$dash_padding` `offset-y` translation lifts it off the bottom.
        // Both lengths ride the icon size, like everything else in the pill.
        let k = metrics.icon_px / ICON_PX;
        let (x, w) = allocate_1d(tile.loc.x, tile.size.w, DOT_PX * k, Align1::Center);
        let (y, h) = allocate_1d(pill.loc.y, pill.size.h, DOT_PX * k, Align1::End);
        Rectangle::new(Point::from((x, y - DOT_OFFSET_Y * k)), Size::from((w, h)))
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
            .map(|_| Self::dot_box(layout.tiles[i], layout.pill, &layout.metrics))
    }

    /// The currently-hovered element (for the conformance corpus).
    #[cfg(test)]
    pub fn hovered_for_test(&self) -> Option<DashHit> {
        self.hovered
    }

    /// The dash render elements for `output`, faded by overview `progress` (0..1).
    /// Icons are pushed first (topmost); the pill chrome last (below them) — the
    /// panel first-topmost order.
    ///
    /// `blur` puts a blurred capture of the scene under the pill. The dock wants it and the
    /// overview does not; the reasons are at the push site.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        app_icons: &AppIconCache,
        sym_icons: &IconCache,
        output: &Output,
        area: Rectangle<f64, Logical>,
        progress: f64,
        blur: bool,
        // `org.gnome.desktop.interface color-scheme` — the pill takes the shared plate.
        appearance: widget::style::Appearance,
        // `org.gnome.desktop.interface accent-color` — what an urgent app's icon glows in,
        // wherever the dash is drawn.
        accent: [u8; 3],
        // Drawing the *poke*: only the apps demanding attention, and no pill, blur, separator,
        // dots or show-apps button — the dock rests part-way out in this mode, so what shows
        // above the screen edge is icons rather than a slab of dash. They keep their normal
        // horizontal positions, so pushing into the edge lands the pointer on the one you are
        // about to click.
        poking: bool,
    ) -> Vec<DashElement> {
        let scale = output.current_scale().fractional_scale();
        let layout = self.layout(area);
        let metrics = layout.metrics;
        let alpha = progress as f32;

        let mut cache = self.cache.borrow_mut();
        // Cached uploads belong to one renderer context; drop them if it changed.
        let context = renderer.context_id();
        if cache.context.as_ref() != Some(&context) {
            cache.icons.borrow_mut().clear();
            cache.context = Some(context);
        }

        let mut elements = Vec::with_capacity(layout.tiles.len() + 2);

        // The running dots — topmost, because GNOME adds `_dot` to the icon
        // container *after* the icon (`appDisplay.js:2955-2964`) and the dash
        // `offset-y` lifts it onto the icon's lower edge. Its own bake layer: the
        // pill chrome underneath the icons cannot carry something that must draw
        // over them. Skipped entirely when nothing is running.
        if !poking && self.items.iter().any(|e| e.running) {
            let dots: Vec<Rectangle<f64, Logical>> = self
                .items
                .iter()
                .enumerate()
                .filter(|(_, e)| e.running)
                .map(|(i, _)| {
                    let d = Self::dot_box(layout.tiles[i], layout.pill, &layout.metrics);
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
                        p.fill_rounded(*dot, dot.size.w / 2., DOT_COLOR)?;
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
                    elements.push(DashElement::Texture(
                        TextureRenderElement::from_texture_buffer(
                            buffer,
                            layout.pill.loc,
                            alpha,
                            None,
                            None,
                            Kind::Unspecified,
                        ),
                    ));
                }
                Err(err) => tracing::error!("error baking the dash running dots: {err:#}"),
            }
        }

        // App icons, on their tiles. While poking, only the ones asking for attention.
        for (i, entry) in self
            .items
            .iter()
            .enumerate()
            .filter(|(_, entry)| !poking || entry.urgent)
        {
            if let Some(el) = widget::app_icon_element(
                renderer,
                &mut cache.icons.borrow_mut(),
                app_icons,
                &entry.icon,
                metrics.icon_px,
                scale,
                Point::from((0., 0.)),
                layout.icon_center(i),
                alpha,
            ) {
                elements.push(DashElement::Texture(el));
            }
        }

        // The accent glow behind each urgent icon. Pushed *after* the icons, because the first
        // element pushed is the topmost — an "under the icons" layer has to come later — and
        // *before* the pill, so the halo lies on the plate rather than under it.
        //
        // Drawn wherever the dash is, not only in a poke: an app demanding attention still is
        // once you pull the dock the rest of the way out, and losing the glow at that moment
        // would drop the only mark saying which icon you came for. There is no GNOME reference
        // — attention on the dash is ours (`dock-divergence.md`); GNOME posts a notification
        // and leaves the dash alone.
        if self.items.iter().any(|entry| entry.urgent) {
            let accent = widget::style::accent_rgba(accent);
            let tiles: Vec<Rectangle<f64, Logical>> = self
                .items
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.urgent)
                .map(|(i, _)| {
                    let t = layout.tiles[i];
                    Rectangle::new(
                        t.loc - layout.pill.loc + Point::from((GLOW_PAD, GLOW_PAD)),
                        t.size,
                    )
                })
                .collect();
            // The bake buffer carries `GLOW_PAD` of transparent margin on every side: a
            // drop shadow bleeds ~1.5·blur (3σ) past its box, and a pill-sized buffer would
            // clip the halo of the outermost icons flat.
            let size = Size::from((
                layout.pill.size.w + GLOW_PAD * 2.,
                layout.pill.size.h + GLOW_PAD * 2.,
            ));
            let radius = AppIcon::RADIUS;
            // The accent rides the revision, or changing it would keep serving the old glow
            // until the dash content next changed. Same rule as the pill's appearance below.
            let revision = (self.content_rev << 24)
                | ((u64::from(accent[0] as u8) << 16)
                    | (u64::from(accent[1] as u8) << 8)
                    | u64::from(accent[2] as u8));
            let texture = widget::bake(
                renderer,
                &mut cache.poke,
                scale,
                size,
                revision,
                |_| Ok(()),
                |frame, phys, ()| {
                    let mut p = Painter::new(frame, scale, phys);
                    p.clear(widget::style::TRANSPARENT)?;
                    let color = [accent[0], accent[1], accent[2], GLOW_ALPHA];
                    for tile in &tiles {
                        p.drop_shadow(*tile, radius, GLOW_BLUR, (0., 0.), color)?;
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
                    elements.push(DashElement::Texture(
                        TextureRenderElement::from_texture_buffer(
                            buffer,
                            layout.pill.loc - Point::from((GLOW_PAD, GLOW_PAD)),
                            alpha,
                            None,
                            None,
                            Kind::Unspecified,
                        ),
                    ));
                }
                Err(err) => tracing::error!("error baking the dash urgency glow: {err:#}"),
            }
        }

        // A poke is icons and their glow, and nothing else — every layer below this point is
        // dash chrome. One early return rather than a `!poking` guard per layer, because that
        // is what the first cut did and the show-apps button was left out of it, riding up the
        // screen edge alongside the urgent icon.
        if poking {
            return elements;
        }

        // The show-apps symbolic glyph. Built by hand rather than via `icon_element`
        // because it fades with `progress` and `icon_element` hardcodes alpha 1.
        {
            if let Some(tb) = sym_icons.texture(
                renderer,
                SHOW_APPS_ICON,
                metrics.icon_px,
                scale,
                SHOW_APPS_FG,
            ) {
                let logical = tb.logical_size();
                let center = layout.icon_center(layout.n_items);
                let loc = center - Point::from((logical.w / 2., logical.h / 2.));
                elements.push(DashElement::Texture(
                    TextureRenderElement::from_texture_buffer(
                        tb,
                        loc,
                        alpha,
                        None,
                        None,
                        Kind::Unspecified,
                    ),
                ));
            }
        }

        // The pill chrome (background + separator + running dots + the hovered
        // tile's fill), baked + cached. A poke draws none of it.
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
        // The appearance rides the revision, or a Dark Style toggle would leave the pill
        // serving the old plate until its content or hover next changed.
        let revision =
            (self.content_rev << 21) | (appearance.rev() << 20) | (hover_code & 0xf_ffff);

        let plate = dash_bg(appearance);
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
                let mut node = metrics.background();
                node.background = Some(plate);
                node.paint(&mut p, pill_local)?;

                // The favorites/running divider. `hairline` *clears* rather than
                // blends, so the translucent `$system_borders_color` has to be
                // pre-blended onto the pill or it would punch a hole in it.
                if let Some(sep) = layout.separator {
                    let rel = Rectangle::new(sep.loc - pill_origin, sep.size);
                    p.hairline(rel, widget::style::over(plate, SEPARATOR_COLOR))?;
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
                elements.push(DashElement::Texture(
                    TextureRenderElement::from_texture_buffer(
                        buffer,
                        layout.pill.loc,
                        alpha,
                        None,
                        None,
                        Kind::Unspecified,
                    ),
                ));
            }
            Err(err) => tracing::error!("error baking the dash: {err:#}"),
        }

        // The blurred capture under the pill — pushed last, so it is *under* everything above
        // (the first element pushed is the topmost).
        //
        // Only the dock asks for it, and the reason is not thrift: the overview's own backdrop is
        // already a blurred wallpaper (`overview-port.md` §13), so a second blur there would cost
        // a capture and a Kawase chain to re-blur something blurred. The dock hangs over the raw
        // desktop instead, where the wash alone reads as a flat grey slab. It also cannot fade —
        // `FramebufferEffectElement` carries no alpha — and the overview dash fades in with the
        // overview, which is the other reason it would not belong there.
        if blur {
            elements.push(DashElement::Backdrop(self.backdrop.render(
                None,
                RenderParams {
                    geometry: layout.pill,
                    subregion: None,
                    clip: Some((layout.pill, CornerRadius::from(metrics.pill_radius as f32))),
                    scale,
                },
                Some(PILL_BLUR),
                Finish::NONE,
            )));
        }

        elements
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dash_with(n: usize) -> Dash {
        let mut dash = Dash::new(Clock::with_time(std::time::Duration::ZERO));
        let items = (0..n).map(|i| entry(&format!("app{i}.desktop"))).collect();
        dash.set_items(items, n);
        dash
    }

    /// The drop gap eases open and shut over `DASH_ANIMATION_TIME` but *jumps* between
    /// slots: gnome-shell re-shows the placeholder with `fadeIn = false` when one is
    /// already up (`dash.js:918-932`).
    #[test]
    fn the_drop_gap_eases_open_and_shut_but_jumps_between_slots() {
        use std::time::Duration;

        let mut dash = dash_with(3);
        let mut clock = dash.clock.clone();
        let area = box_1080();
        let closed_w = dash.layout(area).pill.size.w;

        // Opening: nothing at t=0, part-way at t=100ms, all of it by t=200ms.
        assert!(dash.set_drop_slot(Some(1)));
        assert_eq!(dash.gap_w(), 0., "the gap must start closed and grow");
        clock.set_unadjusted(Duration::from_millis(100));
        let mid = dash.gap_w();
        assert!(
            mid > 0. && mid < DashMetrics::gnome().icon_px,
            "mid-animation the gap must be part-open, got {mid}"
        );
        assert!(
            dash.are_animations_ongoing(),
            "and must hold the redraw loop open while it moves"
        );
        clock.set_unadjusted(Duration::from_millis(200));
        assert_eq!(dash.gap_w(), PLACEHOLDER_W);
        assert!(!dash.are_animations_ongoing());
        assert_eq!(
            dash.layout(area).pill.size.w,
            closed_w + PLACEHOLDER_W,
            "the open gap widens the pill by exactly the placeholder"
        );

        // Moving: instant, and the space moves with it.
        let before = dash.layout(area).tiles[1].loc.x;
        assert!(dash.set_drop_slot(Some(2)));
        assert_eq!(dash.gap_w(), PLACEHOLDER_W, "moving must not re-animate");
        assert_eq!(
            dash.layout(area).tiles[1].loc.x,
            before - PLACEHOLDER_W,
            "tile 1 is no longer after the gap, so it comes back by its width"
        );

        // Closing: eased, and the icons stay parted around it until it is gone.
        assert!(dash.set_drop_slot(None));
        assert_eq!(
            dash.gap_w(),
            PLACEHOLDER_W,
            "the close starts from wide open"
        );
        clock.set_unadjusted(Duration::from_millis(300));
        let mid = dash.gap_w();
        assert!(
            mid > 0. && mid < PLACEHOLDER_W,
            "mid-close the gap must still hold space, got {mid}"
        );
        assert_eq!(
            dash.layout(area).tiles[2].loc.x - dash.layout(area).tiles[1].loc.x,
            DashMetrics::gnome().item_advance + mid,
            "the collapsing gap keeps parting the icons it was between"
        );
        clock.set_unadjusted(Duration::from_millis(400));
        assert_eq!(dash.gap_w(), 0.);
        assert_eq!(dash.layout(area).pill.size.w, closed_w);
    }

    /// A dash with `n_fav` favorites followed by `n_running` running non-favorites.
    fn dash_with_running(n_fav: usize, n_running: usize) -> Dash {
        let mut dash = Dash::new(Clock::with_time(std::time::Duration::ZERO));
        let mut items: Vec<DashEntry> = (0..n_fav)
            .map(|i| entry(&format!("fav{i}.desktop")))
            .collect();
        for i in 0..n_running {
            items.push(DashEntry {
                running: true,
                urgent: false,
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
            urgent: false,
        }
    }

    /// The box `overview_layout` allocates the dash on 1920×1080 with the 35px
    /// panel strut: bottom-anchored, the dash's preferred height tall. 1920×1080 is above
    /// the adaptive-chrome reference canvas, so these are GNOME's own lengths.
    fn box_1080() -> Rectangle<f64, Logical> {
        let h = preferred_height(Size::from((1920., 1080.)));
        Rectangle::new(Point::from((0., 1080. - h)), Size::from((1920., h)))
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
        let pad = Point::from((
            layout.pill.loc.x + 2.,
            layout.pill.loc.y + layout.metrics.pill_h / 2.,
        ));
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

    /// A poking dock draws only the urgent icons, so only those can be clicked — the tiles that
    /// are not drawn keep their geometry, and without the filter pushing the pointer into the
    /// bottom edge to reach the glowing icon would activate an invisible neighbour, or open the
    /// app grid, which the poke does not draw at all.
    #[test]
    fn a_poke_can_only_be_clicked_on_its_urgent_icons() {
        let mut dash = dash_with(3);
        let mut items: Vec<DashEntry> = (0..3).map(|i| entry(&format!("app{i}.desktop"))).collect();
        items[1].urgent = true;
        dash.set_items(items, 3);

        // Not poking: every hit passes through untouched.
        assert_eq!(
            dash.filter_poke(DashHit::App(0), false),
            Some(DashHit::App(0))
        );
        assert_eq!(
            dash.filter_poke(DashHit::ShowApps, false),
            Some(DashHit::ShowApps)
        );

        // Poking: only the urgent app survives.
        assert_eq!(
            dash.filter_poke(DashHit::App(1), true),
            Some(DashHit::App(1)),
            "the icon that is actually drawn must stay clickable"
        );
        assert_eq!(dash.filter_poke(DashHit::App(0), true), None);
        assert_eq!(dash.filter_poke(DashHit::App(2), true), None);
        assert_eq!(
            dash.filter_poke(DashHit::ShowApps, true),
            None,
            "the app grid is not an app demanding attention"
        );
        assert_eq!(dash.filter_poke(DashHit::Background, true), None);
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
        let mut dash = Dash::new(Clock::with_time(std::time::Duration::ZERO));
        assert!(dash.set_items(vec![entry("a.desktop")], 1));
        assert!(!dash.set_items(vec![entry("a.desktop")], 1));
        assert!(
            dash.set_items(
                vec![DashEntry {
                    running: true,
                    urgent: false,
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
        let icon_bottom = layout.icon_center(1).y + layout.metrics.icon_px / 2.;
        assert!(
            dot.loc.y >= icon_bottom,
            "dot top {} must be at/below the icon bottom {icon_bottom}",
            dot.loc.y
        );
    }

    /// The dash-background theme-node reproduces the pill's hand-summed lengths: height
    /// is padding-only (`tile + 2·pill_pad_v = pill_h`), width is the run plus horizontal
    /// padding, and its content box insets by exactly the padding. Pins the node ⇄
    /// metrics equivalence so a drift in either is caught.
    #[test]
    fn dash_background_node_matches_the_pill_constants() {
        let m = DashMetrics::gnome();
        let size = m.background().allocation_for(Size::from((100., m.tile)));
        assert_eq!(size.h, m.pill_h);
        assert_eq!(size.w, 100. + 2. * m.pill_pad_h);

        let pill = Rectangle::new(Point::from((0., 0.)), size);
        let run = m.background().content_box(pill);
        assert_eq!(run.loc, Point::from((m.pill_pad_h, m.pill_pad_v)));
        assert_eq!(run.size, Size::from((100., m.tile)));

        // GNOME's own numbers, so the ladder's top rung is still the reference dash.
        assert_eq!((m.icon_px, m.tile, m.pill_h), (64., 76., 100.));
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
        assert_eq!(
            sep.size,
            Size::from((SEPARATOR_W, DashMetrics::gnome().icon_px))
        );

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
