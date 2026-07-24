//! The overview dash — the favorites bar (`js/ui/dash.js`).
//!
//! A rounded background pill at the bottom-center of the overview holding the
//! user's favorite apps (`AppFavorites`, via [`crate::app_system::AppSystem`]),
//! each a full-color [`widget::AppIcon`] tile, followed by a trailing "show apps"
//! button. Clicking a favorite launches it and closes the overview.
//!
//! **Scope (S3, `docs/fork/overview-port.md`):** favorites only — running apps and
//! their running dots are S6, so the `dash-separator` (favorites↔running,
//! `dash.js:806`) never appears here. Dash icons have no label (`showLabel:false`,
//! `dash.js:26`); the hover tooltip is deferred. The show-apps button renders for
//! fidelity but its toggle (→ APP_GRID) is S8; its clicks are consumed inertly.
//!
//! **Input divergences (S3):** GNOME's dash icons are `St.Button`s that activate on
//! *release* (`clicked`), so a press-then-drag-off cancels; ours launches on *press*
//! (the house pattern shared with the panel intercepts) — simpler, but it forecloses
//! the later drag-a-favorite-out gesture and can't be canceled. A right-click on a
//! GNOME dash icon opens the app context menu (`AppIconMenu`); we consume it inertly
//! (the menu is a later slice). The dash is mouse-only for now: touch taps fall
//! through to the overview's touch grab (the panel has the same gap). All three are
//! revisited when the relevant gesture/menu slices land.
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
use crate::ui::widget::{self, AppIcon, AppIconUploads, Painter};

/// Dash icon size, logical px (`this.iconSize = 64`, `dash.js:321`).
const ICON_PX: f64 = 64.;
/// The `.overview-icon` tile side (icon + `%tile` padding, `_common.scss:86`).
const TILE: f64 = ICON_PX + 2. * AppIcon::PADDING; // 76
/// Per-item advance: tile + `0 2px` item margin (`$dash_spacing`, `_dash.scss:54`).
const ITEM_ADVANCE: f64 = TILE + 4.; // 80
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
/// The tile hover fill: `st-lighten($dash_background_color, 7%)` (flat + always-dark,
/// `_drawing.scss:186-189,270-274`). Lightens (the per-widget hover direction).
const TILE_HOVER: [f32; 4] = [0.286, 0.286, 0.305, 1.];
/// The show-apps glyph color: `$system_fg_color` ≈ `#fafafb` (`_dash.scss:57,62`).
const SHOW_APPS_FG: [f32; 4] = [0.980, 0.980, 0.984, 1.];
/// The show-apps button glyph (`view-app-grid-symbolic`, `dash.js:216`).
const SHOW_APPS_ICON: &str = "view-app-grid-symbolic";

/// One favorite in the dash — a plain-data snapshot (not a live catalog borrow).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashEntry {
    pub id: String,
    pub name: String,
    pub icon: AppIconRef,
}

/// What a point over the dash hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DashHit {
    /// Favorite at index `.0`.
    Favorite(usize),
    /// The trailing show-apps button.
    ShowApps,
    /// The pill background (padding / gaps) — consumes the click, no action.
    Background,
}

/// Computed geometry for one output size: pill box + per-item tile boxes (absolute,
/// logical). Item `favorites.len()` is the show-apps button. Feeds both drawing and
/// hit-testing from one place (the panel `items`/`hit_test`-agree invariant).
struct DashLayout {
    pill: Rectangle<f64, Logical>,
    /// Tile boxes; `[0, n)` favorites, `[n]` the show-apps button.
    tiles: Vec<Rectangle<f64, Logical>>,
    n_favorites: usize,
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
    /// The pill chrome (background + hover fill), keyed `(scale, phys, revision)`.
    bake: widget::BakeCache,
    /// Full-color favorite icon uploads.
    icons: AppIconUploads,
    /// The show-apps symbolic glyph upload (keyed by scale).
    show_apps: HashMap<NotNan<f64>, TextureBuffer<VkTexture>>,
}

/// The overview dash. Owned on `Niri`; fed favorites by `sync_dash_favorites`.
pub struct Dash {
    favorites: Vec<DashEntry>,
    /// Bumped when `favorites` changes — the bake revision's content part.
    content_rev: u64,
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
            favorites: Vec::new(),
            content_rev: 0,
            hovered: None,
            cache: RefCell::new(DashCache::default()),
        }
    }

    /// Replace the favorites snapshot. Returns whether it changed (bumping the bake
    /// revision so the pill re-bakes).
    pub fn set_favorites(&mut self, favorites: Vec<DashEntry>) -> bool {
        if favorites == self.favorites {
            return false;
        }
        self.favorites = favorites;
        self.content_rev = self.content_rev.wrapping_add(1);
        // `hovered` is a positional index; a favorites change (pin/unpin/reorder from
        // gsettings) can make it point at a different app or past the end. Clear it —
        // the next pointer motion re-establishes it — so a stale index can't light the
        // wrong tile or an out-of-range one.
        self.hovered = None;
        true
    }

    /// The desktop id of favorite `i`, if present.
    pub fn favorite_id(&self, i: usize) -> Option<&str> {
        self.favorites.get(i).map(|e| e.id.as_str())
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
        cache.show_apps.clear();
    }

    /// Lay out the dash within its allocated `box` (logical, output coords;
    /// [`crate::ui::overview_layout`] bottom-anchors it to the work area): the
    /// centered pill and its tiles, with the pill's own gap below it. Always at
    /// least the show-apps button, so the pill is never empty (GNOME renders it
    /// unconditionally, `dash.js:352-356`).
    fn layout(&self, area: Rectangle<f64, Logical>) -> DashLayout {
        let n = self.favorites.len();
        let count = n + 1; // + show-apps
        let pill_w = ITEM_ADVANCE * count as f64 + 2. * PILL_PAD_H;
        let pill_x = (area.loc.x + (area.size.w - pill_w) / 2.).round();
        let pill_y = (area.loc.y + area.size.h - MARGIN_BOTTOM - PILL_H).round();
        let pill = Rectangle::new(Point::from((pill_x, pill_y)), Size::from((pill_w, PILL_H)));

        let tile_top = pill_y + PILL_PAD_V;
        let tiles = (0..count)
            .map(|k| {
                let tile_left = pill_x + PILL_PAD_H + ITEM_ADVANCE * k as f64 + 2.;
                Rectangle::new(Point::from((tile_left, tile_top)), Size::from((TILE, TILE)))
            })
            .collect();

        DashLayout {
            pill,
            tiles,
            n_favorites: n,
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
        let rel = pos.x - pill.loc.x - PILL_PAD_H;
        let count = layout.tiles.len();
        if rel < 0. || rel >= ITEM_ADVANCE * count as f64 {
            return Some(DashHit::Background); // side padding
        }
        let k = (rel / ITEM_ADVANCE) as usize;
        Some(if k < layout.n_favorites {
            DashHit::Favorite(k)
        } else {
            DashHit::ShowApps
        })
    }

    /// The logical center of tile `i` within `area` — favorites are
    /// `[0, n)`, the trailing show-apps button is `[n]`. A geometry probe for the
    /// conformance corpus, which clicks real pixels routed through
    /// [`hit_test`](Self::hit_test). `None` if out of range.
    #[cfg(test)]
    pub fn tile_center(
        &self,
        i: usize,
        area: Rectangle<f64, Logical>,
    ) -> Option<Point<f64, Logical>> {
        let layout = self.layout(area);
        (i < layout.tiles.len()).then(|| layout.icon_center(i))
    }

    /// The trailing show-apps button's index (= the favorite count).
    #[cfg(test)]
    pub fn show_apps_index(&self) -> usize {
        self.favorites.len()
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
        let Some(scale_key) = NotNan::new(scale).ok() else {
            return Vec::new();
        };
        let layout = self.layout(area);
        let alpha = progress as f32;

        let mut cache = self.cache.borrow_mut();
        // Cached uploads belong to one renderer context; drop them if it changed.
        let context = renderer.context_id();
        if cache.context.as_ref() != Some(&context) {
            cache.icons.clear();
            cache.show_apps.clear();
            cache.context = Some(context);
        }

        let mut elements = Vec::with_capacity(layout.tiles.len() + 1);

        // Favorite icons (topmost), on their tiles.
        for (i, entry) in self.favorites.iter().enumerate() {
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

        // The show-apps symbolic glyph (its own small upload cache so it fades with
        // `progress` — `icon_element` hardcodes alpha 1).
        if let Some(buffer) = sym_icons.buffer(SHOW_APPS_ICON, ICON_PX, scale, SHOW_APPS_FG) {
            #[allow(clippy::map_entry)]
            if !cache.show_apps.contains_key(&scale_key) {
                match TextureBuffer::from_memory_buffer(renderer, &buffer) {
                    Ok(tb) => {
                        cache.show_apps.insert(scale_key, tb);
                    }
                    Err(err) => tracing::error!("error uploading show-apps icon: {err:#}"),
                }
            }
            if let Some(tb) = cache.show_apps.get(&scale_key) {
                let logical = tb.logical_size();
                let center = layout.icon_center(layout.n_favorites);
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

        // The pill chrome (background + the hovered tile's fill), baked + cached.
        let hovered_tile = match self.hovered {
            Some(DashHit::Favorite(k)) if k < layout.n_favorites => Some(layout.tiles[k]),
            Some(DashHit::ShowApps) => layout.tiles.last().copied(),
            _ => None,
        };
        // revision = content | hover-tile index (None = 0, else index+1).
        let hover_code = hovered_tile
            .map(|_| match self.hovered {
                Some(DashHit::Favorite(k)) => k as u64 + 1,
                Some(DashHit::ShowApps) => layout.n_favorites as u64 + 1,
                _ => 0,
            })
            .unwrap_or(0);
        let revision = (self.content_rev << 20) | (hover_code & 0xf_ffff);

        let pill_origin = layout.pill.loc;
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
                p.fill_rounded_full(PILL_RADIUS, DASH_BG)?;
                if let Some(tile) = hovered_tile {
                    // Tile box relative to the pill origin.
                    let rel = Rectangle::new(tile.loc - pill_origin, tile.size);
                    p.app_tile(
                        &AppIcon {
                            rect: rel,
                            hovered: true,
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
        dash.set_favorites(
            (0..n)
                .map(|i| DashEntry {
                    id: format!("app{i}.desktop"),
                    name: format!("App {i}"),
                    icon: AppIconRef::Fallback,
                })
                .collect(),
        );
        dash
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
                Some(DashHit::Favorite(i))
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
        assert_eq!(dash.hit_test(tile_top, area), Some(DashHit::Favorite(0)));
    }

    /// A favorites change clears the (positional) hover so a stale index can't
    /// light the wrong tile.
    #[test]
    fn set_favorites_clears_hover() {
        let mut dash = dash_with(3);
        assert!(dash.set_hovered(Some(DashHit::Favorite(2))));
        assert_eq!(dash.hovered, Some(DashHit::Favorite(2)));
        // Shrinking to one favorite would leave index 2 dangling — must clear.
        dash.set_favorites(vec![DashEntry {
            id: "only.desktop".into(),
            name: "Only".into(),
            icon: AppIconRef::Fallback,
        }]);
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
            Some(DashHit::Favorite(0))
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
        assert!(dash.set_favorites(vec![DashEntry {
            id: "a.desktop".into(),
            name: "A".into(),
            icon: AppIconRef::Fallback,
        }]));
        let same = vec![DashEntry {
            id: "a.desktop".into(),
            name: "A".into(),
            icon: AppIconRef::Fallback,
        }];
        assert!(!dash.set_favorites(same));
    }
}
