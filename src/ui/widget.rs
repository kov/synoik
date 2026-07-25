//! Reusable widget-construction helpers shared by the popover/panel UIs.
//!
//! Every baking UI component (`input_source_menu`, `calendar`, `quick_settings`,
//! the dialogs, the panel bar, …) used to hand-roll the same offscreen-bake dance
//! — `create_buffer` → `bind` → `render` → `clear`/draw → `finish` →
//! `make_offscreen_sampleable` — behind its own subtly-different texture cache.
//! [`bake`] absorbs that dance once, keyed by `(scale, physical_size, revision)`
//! so a size change auto-invalidates (the calendar-height-freeze class of bug,
//! commit `128d112e`, cannot recur). See `docs/fork/widget-layer-design.md`.
//!
//! The `paint` closure draws through a [`Painter`] over the bound [`VulkanFrame`]:
//! logical/pt verbs ([`clear`](Painter::clear), [`fill_rounded`](Painter::fill_rounded),
//! [`text`](Painter::text)/[`text_clipped`](Painter::text_clipped)) fold the single
//! `× scale` conversion inside the toolkit (H2 in the design doc, the structural fix
//! for the HiDPI-glyph bug class); content-sized text blocks laid out in physical px
//! use the physical-coordinate verbs ([`paragraph`](Painter::paragraph),
//! [`paragraph_spans`](Painter::paragraph_spans), [`fill_rect_px`](Painter::fill_rect_px)).

use std::collections::HashMap;

use anyhow::Context as _;
use ordered_float::NotNan;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::{Bind, Color32F, ContextId, Frame as _, Offscreen, Renderer};
use smithay::utils::{Buffer as BufferCoord, Logical, Physical, Point, Rectangle, Size, Transform};

use crate::app_system::AppIconRef;
use crate::render_helpers::icon::{AppIconCache, IconCache};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{
    premultiply, GlyphRun, VkTexture, VulkanFrame, VulkanRenderer,
};
use crate::utils::to_physical_precise_round;

/// The upload cache for full-color app icons, keyed by
/// `(scale, descriptor, logical size)`. Lives on a baking widget (the dash, later
/// the grid/search) so a hover re-bake doesn't re-upload the icon textures.
pub type AppIconUploads = HashMap<(NotNan<f64>, AppIconRef, u16), TextureBuffer<VkTexture>>;

/// Straight-alpha RGBA, the color type every draw verb takes (glyph coverage /
/// SDF alpha modulates it). Matches the `[f32; 4]` the frame primitives want.
pub type Rgba = [f32; 4];

/// Shared visual tokens, so the same color/icon-set is not re-declared per widget
/// (they drifted before: `HOVER_WASH` in 3 files, `TEXT`/`CHECK_ICONS` in more).
/// Only genuinely-shared, identically-valued tokens live here; widget-specific or
/// divergent values (a menu bg vs a tile bg, the separator alphas) stay local until
/// a port reconciles them against `docs/fork/gnome-style-reference.md`.
pub mod style {
    use super::Rgba;

    /// Fully transparent — the clear color for a rounded (transparent-corner) surface.
    pub const TRANSPARENT: Rgba = [0., 0., 0., 0.];
    /// Primary foreground (opaque white); glyph coverage modulates the alpha.
    pub const TEXT: Rgba = [1., 1., 1., 1.];
    /// Dimmed foreground (secondary labels, e.g. a row's short code).
    pub const MUTED: Rgba = [0.6, 0.6, 0.6, 1.];
    /// The hover highlight wash over a row/tile (a subtle lighten). NOTE: the
    /// lighten-vs-darken *direction* is per-widget (read from the SCSS cascade); this
    /// is the standard lighten used by menu rows / QS tiles / calendar days.
    pub const HOVER_WASH: Rgba = [1., 1., 1., 0.1];
    /// `$borders_color` = `transparentize($fg_color, 0.9)` (white @ 10%, dark theme) — the 1px
    /// rule color for the calendar column separator (`.message-list` border-right,
    /// `_message-list.scss:8`) and the QS group separators
    /// (`.popup-separator-menu-item-separator`, `_popovers.scss:117`). The single source of truth
    /// so the two can't drift; draw both via [`Painter::hairline`](super::Painter::hairline).
    pub const BORDERS: Rgba = [1., 1., 1., 0.1];

    /// Source-over composite of `fg` (possibly translucent) onto the **opaque** `bg`, returning an
    /// opaque color — the color a translucent overlay *would* produce over `bg`. Use it to lay a
    /// translucent [`BORDERS`] hairline onto an opaque surface via the crisp (replacing) clear that
    /// [`Painter::hairline`](super::Painter::hairline) uses, so the line reads the same as it would
    /// blended (a raw translucent clear would instead punch a hole to whatever is behind).
    pub fn over(bg: Rgba, fg: Rgba) -> Rgba {
        let a = fg[3];
        [
            bg[0] * (1. - a) + fg[0] * a,
            bg[1] * (1. - a) + fg[1] * a,
            bg[2] * (1. - a) + fg[2] * a,
            1.,
        ]
    }
    /// Icon-name fallback chain for an "active/selected" check mark.
    pub const CHECK_ICONS: &[&str] = &["object-select-symbolic", "emblem-ok-symbolic"];

    /// Modal-dialog card background — GNOME `$bg_color` `#36363a` (`_dialogs.scss:4`,
    /// `_colors.scss:12`). Flat, borderless; corners rounded to `$alert_radius` (18px).
    pub const DIALOG_BG: Rgba = [0.212, 0.212, 0.227, 1.];
    /// `.popup-menu-content` background — GNOME `$bg_color` `#36363a` (`_popovers.scss:31`,
    /// `_colors.scss:12`). The single home for the panel-popover box fill (QS / date /
    /// input-source), drawn once by the shared popover chrome so the three surfaces can't drift
    /// (they each hand-rolled a *different, too-dark* value before). Same value as [`DIALOG_BG`]
    /// today — both are `$bg_color` — but cited to the menu surface so they may diverge.
    pub const MENU_BG: Rgba = [0.212, 0.212, 0.227, 1.];
    /// `%card` / `.message` base surface — GNOME `$card_bg_color` = `lighten($bg_color, 7%)` ≈
    /// `#47474c` (`_colors.scss:29`). The "one step lighter than the menu" fill used by the
    /// date popover's today card and the QS detail card.
    pub const CARD_BG: Rgba = [0.278, 0.278, 0.298, 1.];
    /// `.button` normal fill — the subtle raised gray `mix($fg, $bg, 9%)` over the
    /// dialog card (`_drawing.scss:171`, `$background_mix_factor` 9%).
    pub const BUTTON_BG: Rgba = [0.283, 0.283, 0.297, 1.];
    /// `.modal-dialog-button` fill — translucent white `rgba(255,255,255,0.1)`
    /// (`%dialog_button`, `_drawing.scss:218`). Neutral dialog action button.
    pub const DIALOG_BUTTON_BG: Rgba = [1., 1., 1., 0.1];
    /// `.destructive-action` fill — `$red_4 #c01c28` (`_default-colors.scss:11`).
    pub const DESTRUCTIVE_BG: Rgba = [0.753, 0.110, 0.157, 1.];
    /// `%system_entry` normal background — `mix($system_fg_color, $system_bg_color, 9%)`
    /// (`_drawing.scss` `entry()` mixin, `$background_mix_factor` 9%), with
    /// `$system_fg_color`=#fafafb and `$system_bg_color`=lighten(#222226, 5%). The
    /// overview `search-entry` pill fill; always-dark. (`_colors.scss:20-21,47`.)
    pub const ENTRY_BG: Rgba = [0.252, 0.252, 0.267, 1.];
    /// `$system_overlay_bg_color` = `mix($system_base_color, $system_fg_color, 90%)`
    /// ≈ `#38383b` (`_colors.scss:50`) — the non-transparent overlay surface used by
    /// the dash and the `.search-section-content` results card. Same value the dash
    /// bakes as its pill (kept in sync via this one constant).
    pub const OVERLAY_BG: Rgba = [0.218, 0.218, 0.233, 1.];

    /// Modal-dialog corner radius, logical px — GNOME `$alert_radius` (`_common.scss:43`,
    /// applied at `_dialogs.scss:6`). Note this is 18px, not `$modal_radius` (16px).
    pub const DIALOG_RADIUS: f64 = 18.;
}

/// Composite a symbolic icon — the first of `candidates` that resolves — centered
/// at `center` (relative to the element `origin`), sized `logical_px`, tinted
/// `color`. The single home for the icon-compositing helper that was copy-pasted
/// across the popover/panel UIs (`icon_element` ×2 + 3 inline sequences). Returns
/// `None` (logging on a GPU upload error) if no candidate resolves or the upload
/// fails, so callers can `if let Some(el) = …` and simply skip a missing glyph.
#[allow(clippy::too_many_arguments)]
pub fn icon_element<S: AsRef<str>>(
    renderer: &mut VulkanRenderer,
    icons: &IconCache,
    candidates: &[S],
    logical_px: f64,
    scale: f64,
    color: Rgba,
    origin: Point<f64, Logical>,
    center: Point<f64, Logical>,
) -> Option<TextureRenderElement<VkTexture>> {
    let tb = candidates
        .iter()
        .find_map(|name| icons.texture(renderer, name.as_ref(), logical_px, scale, color))?;
    let logical = tb.logical_size();
    let loc = origin + center - Point::from((logical.w / 2., logical.h / 2.));
    Some(TextureRenderElement::from_texture_buffer(
        tb,
        loc,
        1.,
        None,
        None,
        Kind::Unspecified,
    ))
}

/// Composite a **full-color application icon** ([`AppIconRef`]), resolved +
/// decoded by the [`AppIconCache`], centered at `center` (relative to the element
/// `origin`), sized `logical_px`. The full-color sibling of [`icon_element`]: no
/// tint (the icon keeps its own colors), and uploads are cached in the caller's
/// [`AppIconUploads`] map (keyed by scale + descriptor + size) so a hover re-bake
/// reuses them. `alpha` multiplies the element (the overview fade). `None` if even
/// the fallback icon fails to resolve/upload.
#[allow(clippy::too_many_arguments)]
pub fn app_icon_element(
    renderer: &mut VulkanRenderer,
    uploads: &mut AppIconUploads,
    icons: &AppIconCache,
    icon: &AppIconRef,
    logical_px: f64,
    scale: f64,
    origin: Point<f64, Logical>,
    center: Point<f64, Logical>,
    alpha: f32,
) -> Option<TextureRenderElement<VkTexture>> {
    let scale_key = NotNan::new(scale).ok()?;
    let key = (scale_key, icon.clone(), (logical_px.round() as u16).max(1));
    #[allow(clippy::map_entry)]
    if !uploads.contains_key(&key) {
        let buffer = icons.buffer(icon, logical_px, scale)?;
        match TextureBuffer::from_memory_buffer(renderer, &buffer) {
            Ok(tb) => {
                uploads.insert(key.clone(), tb);
            }
            Err(err) => {
                tracing::error!("error uploading app icon: {err:#}");
                return None;
            }
        }
    }
    let tb = uploads.get(&key)?;
    let logical = tb.logical_size();
    let loc = origin + center - Point::from((logical.w / 2., logical.h / 2.));
    Some(TextureRenderElement::from_texture_buffer(
        tb.clone(),
        loc,
        alpha,
        None,
        None,
        Kind::Unspecified,
    ))
}

/// GNOME's `.overview-icon` tile (`%tile`, `_common.scss:84-90`): a rounded square
/// holding an icon, with a hover state. The reusable geometry + state shared by the
/// dash (S3) and, later, the app grid and grid search results — GNOME shares it the
/// same way (`DashIcon`/`GridSearchResult` extend `AppIcon`). The icon pixels ride
/// on top as an [`app_icon_element`]; this type owns the tile box, the hover fill,
/// and hit-testing.
#[derive(Debug, Clone, Copy)]
pub struct AppIcon {
    /// The tile box (logical), laid out by the owner.
    pub rect: Rectangle<f64, Logical>,
    pub hovered: bool,
    /// Corner radius of the hover/selection fill. Which one depends on where the
    /// tile lives — see [`AppIcon::RADIUS`] vs [`AppIcon::OVERVIEW_TILE_RADIUS`].
    pub radius: f64,
}

impl AppIcon {
    /// `%tile` padding around the icon (`_common.scss:86`).
    pub const PADDING: f64 = 6.;
    /// `%tile` corner radius (`_common.scss:85`).
    pub const RADIUS: f64 = 16.;

    /// `.overview-tile` padding (`$base_padding * 2`, `_app-grid.scss:26`).
    ///
    /// The app grid and the search results put the button — and so the
    /// hover/selection fill — on the *outer* `.overview-tile`, which overrides
    /// `%tile`'s padding and radius (`_app-grid.scss:24-26`) and wraps the label
    /// as well as the icon. The dash is the other case: it resets
    /// `.overview-tile` and styles the inner `.overview-icon` as a plain `%tile`
    /// instead (`_dash.scss:49-63`), so it keeps [`PADDING`]/[`RADIUS`].
    ///
    /// [`PADDING`]: Self::PADDING
    /// [`RADIUS`]: Self::RADIUS
    pub const OVERVIEW_TILE_PADDING: f64 = 12.;
    /// `.overview-tile` corner radius (`$base_border_radius * 3`, `_app-grid.scss:25`).
    pub const OVERVIEW_TILE_RADIUS: f64 = 24.;

    /// The tile side for a given icon size (icon + padding both sides).
    pub fn size(icon_px: f64) -> f64 {
        icon_px + 2. * Self::PADDING
    }

    pub fn icon_center(&self) -> Point<f64, Logical> {
        Point::from((
            self.rect.loc.x + self.rect.size.w / 2.,
            self.rect.loc.y + self.rect.size.h / 2.,
        ))
    }
}

/// Fixed geometry of a labelled `.overview-tile` — a full-color icon with a caption
/// line beneath it. Shared by the search results grid and the app grid, which GNOME
/// builds from the same `IconGrid.BaseIcon` (`search.js:144-146`, `appDisplay.js`),
/// so the two stay pixel-identical. The hover/selection wash and the label are baked
/// by [`Painter::labelled_tile`]; the icon pixels ride on top as an
/// [`app_icon_element`].
#[derive(Debug, Clone, Copy)]
pub struct TileMetrics {
    /// Full-color icon side (logical).
    pub icon_px: f64,
    /// `.overview-tile` padding around the icon+label (`_app-grid.scss:26`).
    pub pad: f64,
    /// Gap from the icon to the label (`.overview-icon-with-label` spacing,
    /// `$base_padding`, `_app-grid.scss:31-35`).
    pub label_gap: f64,
    /// Height of the single label line.
    pub label_h: f64,
    /// `.overview-tile` corner radius of the hover/selection fill.
    pub radius: f64,
}

impl TileMetrics {
    /// The 96 px labelled tile the app grid and search results share (`ICON_SIZE`=96,
    /// `iconGrid.js:11,83`; `.overview-tile` metrics `_app-grid.scss:24-35`).
    pub const OVERVIEW: Self = Self {
        icon_px: 96.,
        pad: AppIcon::OVERVIEW_TILE_PADDING,
        label_gap: 6.,
        label_h: 18.,
        radius: AppIcon::OVERVIEW_TILE_RADIUS,
    };

    /// The tile's outer size: pad+icon+pad wide, pad+icon+gap+label+pad tall.
    pub fn size(&self) -> Size<f64, Logical> {
        Size::from((
            self.pad + self.icon_px + self.pad,
            self.pad + self.icon_px + self.label_gap + self.label_h + self.pad,
        ))
    }

    /// The icon's center within a tile box `rect` (logical) — the icon sits at the
    /// top of the tile, the label below it.
    pub fn icon_center(&self, rect: Rectangle<f64, Logical>) -> Point<f64, Logical> {
        Point::from((
            rect.loc.x + rect.size.w / 2.,
            rect.loc.y + self.pad + self.icon_px / 2.,
        ))
    }
}

/// A rounded single-line text-entry chrome — the GNOME `St.Entry` used for the
/// overview `search-entry` (`_search-entry.scss`, `overviewControls.js:325`). A
/// **view + geometry** primitive: the caller owns the editable string (like
/// [`crate::ui::run_dialog`] does) and feeds it in. [`Entry::bake`] draws the pill
/// background plus the placeholder/typed text and a trailing caret; the primary
/// (find) and optional secondary (clear) symbolic glyphs ride on top as
/// [`icon_element`]s the caller composites (so they fade with the overview, like the
/// dash's show-apps glyph). run_dialog keeps its own bespoke centered mono field for
/// now — adopting this here is a follow-up.
pub struct Entry;

/// Where an [`Entry`]'s parts sit (all logical, absolute output coords).
#[derive(Debug, Clone, Copy)]
pub struct EntryLayout {
    /// The rounded pill box.
    pub pill: Rectangle<f64, Logical>,
    /// Center of the primary (`edit-find-symbolic`) glyph.
    pub primary_icon: Point<f64, Logical>,
    /// Center of the trailing (`edit-clear-symbolic`) glyph.
    pub secondary_icon: Point<f64, Logical>,
    /// Left x where the text/placeholder begins.
    pub text_x: f64,
}

/// What a point over an [`Entry`] hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryHit {
    /// The trailing clear (`edit-clear-symbolic`) glyph.
    Clear,
    /// Anywhere else in the pill.
    Field,
}

impl Entry {
    /// Pill height, logical: `%entry_common` 9px padding around a ~15px line, rounded
    /// up to a comfortable pill (`.search-entry` is `$forced_circular_radius`, so the
    /// radius is `height/2`). S5-tunable.
    pub const HEIGHT: f64 = 40.;
    /// The find/clear symbolic glyph side (`.search-entry-icon` `$scalable_icon_size`
    /// = 16px, `_search-entry.scss:10`).
    pub const ICON_PX: f64 = 16.;
    /// Icon center inset from the pill's near edge (icon half + `padding 0 $base_margin`).
    const ICON_INSET: f64 = 16.;
    /// Entry font (`%system_entry` inherits the 11pt base).
    const TEXT_PT: f64 = 11.;

    /// Lay out a pill of `width` centered horizontally on `center_x`, top edge `top_y`.
    pub fn layout(center_x: f64, top_y: f64, width: f64) -> EntryLayout {
        let x = (center_x - width / 2.).round();
        let pill = Rectangle::new(
            Point::from((x, top_y.round())),
            Size::from((width, Self::HEIGHT)),
        );
        let cy = pill.loc.y + Self::HEIGHT / 2.;
        EntryLayout {
            pill,
            primary_icon: Point::from((pill.loc.x + Self::ICON_INSET, cy)),
            secondary_icon: Point::from((pill.loc.x + width - Self::ICON_INSET, cy)),
            text_x: pill.loc.x + Self::ICON_INSET * 2.,
        }
    }

    /// Hit-test a point: the trailing clear disc (only when `has_clear`), else the
    /// field body, else `None`.
    pub fn hit(
        layout: &EntryLayout,
        pos: Point<f64, Logical>,
        has_clear: bool,
    ) -> Option<EntryHit> {
        if !layout.pill.contains(pos) {
            return None;
        }
        if has_clear {
            let d = pos - layout.secondary_icon;
            if d.x * d.x + d.y * d.y <= Self::ICON_PX * Self::ICON_PX {
                return Some(EntryHit::Clear);
            }
        }
        Some(EntryHit::Field)
    }

    /// Bake the pill + text/placeholder + trailing caret into a pill-sized texture
    /// (composited by the caller at `layout.pill.loc`, the two glyphs on top). When
    /// `text` is empty the `placeholder` shows muted with no caret (an unfocused hint);
    /// once typing starts the text shows in full white with a caret bar. Long text is
    /// clipped at the trailing-icon inset (no horizontal scroll yet — MVP).
    pub fn bake(
        renderer: &mut VulkanRenderer,
        cache: &mut BakeCache,
        scale: f64,
        width: f64,
        text: &str,
        placeholder: &str,
        revision: u64,
    ) -> anyhow::Result<VkTexture> {
        let size = Size::<f64, Logical>::from((width, Self::HEIGHT));
        let empty = text.is_empty();
        // The caret bar (U+258F), like run_dialog, only while typing.
        let display = if empty {
            placeholder.to_owned()
        } else {
            format!("{text}\u{258f}")
        };
        let text_x = Self::ICON_INSET * 2.;
        let clip = Rectangle::<f64, Logical>::new(
            Point::from((text_x, 0.)),
            Size::from((width - text_x - Self::ICON_INSET * 2., Self::HEIGHT)),
        );
        bake(
            renderer,
            cache,
            scale,
            size,
            revision,
            |r| {
                let mut shaper = TextShaper::new(r, scale);
                shaper.shape(&display, TextStyle::new(Self::TEXT_PT))
            },
            move |frame, phys, shaped| {
                let mut p = Painter::new(frame, scale, phys);
                p.clear(style::TRANSPARENT)?;
                p.fill_rounded_full(Self::HEIGHT / 2., style::ENTRY_BG)?;
                let color = if empty { style::MUTED } else { style::TEXT };
                p.text_band(shaped, text_x, HAlign::Left, 0., Self::HEIGHT, color, clip)?;
                Ok(())
            },
        )
    }
}

/// A per-widget offscreen-texture cache for [`bake`], keyed by `(scale,
/// physical_size, revision)`. One lives (behind a `RefCell`) on each baking
/// widget; it clears itself when the renderer context changes.
#[derive(Default)]
pub struct BakeCache {
    context: Option<ContextId<VkTexture>>,
    /// The renderer's glyph epoch when these were baked. It moves only when a glyph upload failed
    /// and the residency was thrown away — at which point anything baked from it drew blank text,
    /// under a key the widget has no reason to change. Without this the blank survives as long as
    /// the widget's content does. See `VulkanRenderer::text_epoch`.
    text_epoch: u64,
    // key: (scale, physical_w, physical_h) -> (revision, texture)
    textures: HashMap<(NotNan<f64>, i32, i32), (u64, VkTexture)>,
}

impl BakeCache {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Convert a widget's logical size to the physical buffer size at `scale`
/// (clamped to at least 1×1). The single home for that rounding.
pub fn physical_size(scale: f64, logical: Size<f64, Logical>) -> Size<i32, Physical> {
    Size::from((
        to_physical_precise_round::<i32>(scale, logical.w).max(1),
        to_physical_precise_round::<i32>(scale, logical.h).max(1),
    ))
}

/// Bake a widget's chrome into a scale-sized offscreen `VkTexture`, caching by
/// `(scale, physical_size, revision)`. On a cache hit the stored texture is
/// cloned (the GPU image is `Arc`-shared) and **neither** closure runs.
///
/// The physical buffer is `round(logical_size × scale)`. Two phases, run only on a
/// cache miss:
/// - `prepare(renderer)` shapes every `GlyphRun` and returns them (or any bake inputs). Glyph
///   shaping needs `&mut VulkanRenderer` and cannot run while the frame is alive, so it must happen
///   here, before the frame opens.
/// - `paint(frame, phys, prepared)` clears + draws everything into the bound [`VulkanFrame`] (the
///   full-buffer rect is `Rectangle::from_size(phys)`). Widgets clear with their own color —
///   transparent for rounded popovers, a border color for square dialogs.
pub fn bake<P>(
    renderer: &mut VulkanRenderer,
    cache: &mut BakeCache,
    scale: f64,
    logical_size: Size<f64, Logical>,
    revision: u64,
    prepare: impl FnOnce(&mut VulkanRenderer) -> anyhow::Result<P>,
    paint: impl FnOnce(&mut VulkanFrame, Size<i32, Physical>, &P) -> anyhow::Result<()>,
) -> anyhow::Result<VkTexture> {
    let scale_key = NotNan::new(scale).context("non-finite scale")?;
    let phys = physical_size(scale, logical_size);
    let key = (scale_key, phys.w, phys.h);

    // The renderer context changing invalidates every cached GPU texture, and so does the glyph
    // residency being rebuilt — a texture baked from glyphs that never reached the atlas holds
    // blank text.
    let context = renderer.context_id();
    let text_epoch = renderer.text_epoch();
    if cache.context.as_ref() != Some(&context) || cache.text_epoch != text_epoch {
        cache.textures.clear();
        cache.context = Some(context);
        cache.text_epoch = text_epoch;
    }

    let fresh = matches!(cache.textures.get(&key), Some((rev, _)) if *rev == revision);
    if !fresh {
        let prepared = prepare(renderer)?;
        let tex = bake_uncached(renderer, scale, logical_size, |frame, phys| {
            paint(frame, phys, &prepared)
        })?;
        cache.textures.insert(key, (revision, tex));
    }

    Ok(cache.textures.get(&key).map(|(_, t)| t.clone()).unwrap())
}

/// A GNOME `box-shadow` for a card, in logical px: gaussian `blur` radius (σ = blur/2), `offset`
/// `(dx, dy)`, `spread` (the shadow box grows by this before blurring, corners with it), and a
/// straight-alpha `color`. Consumed by [`bake_card_shadow`].
#[derive(Clone, Copy)]
pub struct DropShadowSpec {
    pub blur: f64,
    pub offset: (f64, f64),
    pub spread: f64,
    pub color: Rgba,
}

/// Bake (cached by `(scale, size, revision)`) a card's drop shadow into its own transparent
/// texture, and return it with the physical `(dx, dy)` to subtract from the card's on-screen
/// location so the shadow sits behind it. `card_size`/`radius` describe the card; `spec` is the
/// GNOME `box-shadow`. The buffer pads the card by the blur reach (~3σ) + `spread` all round (plus
/// the downward `offset` at the bottom), so the fringe never clips. The reusable form of the
/// screenshot-panel / notification-banner shadow — draws through [`Painter::drop_shadow`].
pub fn bake_card_shadow(
    renderer: &mut VulkanRenderer,
    cache: &mut BakeCache,
    scale: f64,
    revision: u64,
    card_size: Size<f64, Logical>,
    radius: f64,
    spec: DropShadowSpec,
) -> anyhow::Result<(VkTexture, Point<i32, Physical>)> {
    // Blur reach (~3σ) + a pixel of ceil headroom, plus the spread, is the pad on every side; the
    // downward offset only extends the bottom (top pad already covers a small upward offset).
    let reach = spec.blur * 1.5 + 1.;
    let pad = reach + spec.spread;
    let size = Size::<f64, Logical>::from((
        card_size.w + pad * 2.,
        card_size.h + pad * 2. + spec.offset.1.max(0.),
    ));
    // The (spread-inflated) shadow box, positioned so it has `reach` of blur room on top/left.
    let box_rect = Rectangle::new(
        Point::from((reach, reach)),
        Size::from((
            card_size.w + spec.spread * 2.,
            card_size.h + spec.spread * 2.,
        )),
    );
    let tex = bake(
        renderer,
        cache,
        scale,
        size,
        revision,
        |_| Ok(()),
        |frame, phys, ()| {
            let mut p = Painter::new(frame, scale, phys);
            p.clear(style::TRANSPARENT)?;
            p.drop_shadow(
                box_rect,
                radius + spec.spread,
                spec.blur,
                spec.offset,
                spec.color,
            )?;
            Ok(())
        },
    )?;
    let off = to_physical_precise_round::<i32>(scale, pad);
    Ok((tex, Point::from((off, off))))
}

/// Bake (cached by `(scale, size, revision)`) a card's 1px inset border into its own transparent
/// texture — a `stroke_rounded_full` ring at `radius`, `color` — to composite ON TOP of the card
/// (at the card's own origin, no offset). The `.popup-menu-content` border counterpart to
/// [`bake_card_shadow`]; a top overlay so it works for a multi-texture card (the calendar popover
/// stacks a column over its background box) without a seam. Width is GNOME's fixed 1px.
pub fn bake_card_border(
    renderer: &mut VulkanRenderer,
    cache: &mut BakeCache,
    scale: f64,
    revision: u64,
    card_size: Size<f64, Logical>,
    radius: f64,
    color: Rgba,
) -> anyhow::Result<VkTexture> {
    bake(
        renderer,
        cache,
        scale,
        card_size,
        revision,
        |_| Ok(()),
        |frame, phys, ()| {
            let mut p = Painter::new(frame, scale, phys);
            p.clear(style::TRANSPARENT)?;
            p.stroke_rounded_full(radius, 1., color)?;
            Ok(())
        },
    )
}

/// Bake (cached by `(scale, size, revision)`) a card's rounded `.popup-menu-content` background
/// fill into its own texture — a `fill_rounded_full` at `radius`, `color`. Composited BEHIND the
/// content and ABOVE the drop shadow, this is the single home for the panel-popover box bg so the
/// three popovers (QS / date / input-source) can't drift: each content bakes with a transparent
/// bg and the shared popover chrome draws this one cited fill. Counterpart to [`bake_card_border`]
/// and [`bake_card_shadow`].
pub fn bake_card_fill(
    renderer: &mut VulkanRenderer,
    cache: &mut BakeCache,
    scale: f64,
    revision: u64,
    card_size: Size<f64, Logical>,
    radius: f64,
    color: Rgba,
) -> anyhow::Result<VkTexture> {
    bake(
        renderer,
        cache,
        scale,
        card_size,
        revision,
        |_| Ok(()),
        |frame, phys, ()| {
            let mut p = Painter::new(frame, scale, phys);
            p.clear(style::TRANSPARENT)?;
            p.fill_rounded_full(radius, color)?;
            Ok(())
        },
    )
}

/// The opaque sub-region of a rounded-rect texture of physical `size` with corner radius
/// `radius_px` (physical px): two overlapping bands that exclude the four transparent corner
/// squares, so occlusion never treats a cut-away corner as opaque (which would drop whatever shows
/// through the rounding). Under-reporting the small arc slivers is harmless. The single home for
/// this band math (the popover chrome fill and any rounded opaque surface share it).
pub fn rounded_opaque_regions(
    size: Size<i32, BufferCoord>,
    radius_px: i32,
) -> Vec<Rectangle<i32, BufferCoord>> {
    if radius_px > 0 && size.w > 2 * radius_px && size.h > 2 * radius_px {
        vec![
            Rectangle::new(
                Point::from((0, radius_px)),
                Size::from((size.w, size.h - 2 * radius_px)),
            ),
            Rectangle::new(
                Point::from((radius_px, 0)),
                Size::from((size.w - 2 * radius_px, size.h)),
            ),
        ]
    } else {
        vec![Rectangle::from_size(size)]
    }
}

/// A cache for [`bake_content`] — a content-sized bake whose physical size is not
/// known until its text is shaped, so it is keyed by `(scale, revision)` alone
/// (the revision determines the content, hence the size). Clears on context change.
#[derive(Default)]
pub struct ContentCache {
    context: Option<ContextId<VkTexture>>,
    /// The renderer's glyph epoch when these were baked. It moves only when a glyph upload failed
    /// and the residency was thrown away — at which point anything baked from it drew blank text,
    /// under a key the widget has no reason to change. Without this the blank survives as long as
    /// the widget's content does. See `VulkanRenderer::text_epoch`.
    text_epoch: u64,
    textures: HashMap<NotNan<f64>, (u64, VkTexture)>,
}

impl ContentCache {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Bake a **content-sized** widget (a dialog/notification box whose size is derived
/// from its shaped text's ink, not known up front). Cached by `(scale, revision)`.
///
/// `prepare(renderer)` shapes the text and returns the physical buffer size plus a
/// layout value `P`; `paint(frame, phys, prepared)` draws it. Both run only on a
/// cache miss. The caller reads the returned texture's own size to place it (these
/// widgets center themselves on screen from the baked size).
pub fn bake_content<P>(
    renderer: &mut VulkanRenderer,
    cache: &mut ContentCache,
    scale: f64,
    revision: u64,
    prepare: impl FnOnce(&mut VulkanRenderer) -> anyhow::Result<(Size<i32, Physical>, P)>,
    paint: impl FnOnce(&mut VulkanFrame, Size<i32, Physical>, &P) -> anyhow::Result<()>,
) -> anyhow::Result<VkTexture> {
    let scale_key = NotNan::new(scale).context("non-finite scale")?;

    // Same two invalidations as `bake`: a new renderer context, and a rebuilt glyph residency
    // (which means anything cached here was baked with blank text).
    let context = renderer.context_id();
    let text_epoch = renderer.text_epoch();
    if cache.context.as_ref() != Some(&context) || cache.text_epoch != text_epoch {
        cache.textures.clear();
        cache.context = Some(context);
        cache.text_epoch = text_epoch;
    }

    let fresh = matches!(cache.textures.get(&scale_key), Some((rev, _)) if *rev == revision);
    if !fresh {
        let (phys, prepared) = prepare(renderer)?;
        let tex = bake_uncached_sized(renderer, phys, |frame| paint(frame, phys, &prepared))?;
        cache.textures.insert(scale_key, (revision, tex));
    }

    Ok(cache
        .textures
        .get(&scale_key)
        .map(|(_, t)| t.clone())
        .unwrap())
}

/// The offscreen dance for an already-known **physical** size (no logical→physical
/// step). Shared by [`bake_content`] and [`bake_uncached`]; also for a
/// content-sized widget with its own owner-driven cache that has already computed
/// its physical size (the screenshot help panel).
pub fn bake_uncached_sized(
    renderer: &mut VulkanRenderer,
    phys: Size<i32, Physical>,
    paint: impl FnOnce(&mut VulkanFrame) -> anyhow::Result<()>,
) -> anyhow::Result<VkTexture> {
    // Every offscreen bake funnels through here, and each one is a render pass,
    // a submit and a fence wait. Counting and timing them here (rather than at
    // each caller) is what lets a slow frame say how much of itself was
    // re-rasterization.
    let _timed = crate::frame_log::time_bake();

    let (w, h) = (phys.w.max(1), phys.h.max(1));
    let mut target =
        renderer.create_buffer(Fourcc::Abgr8888, Size::<i32, BufferCoord>::from((w, h)))?;
    {
        let mut fb = renderer.bind(&mut target)?;
        let mut frame = renderer.render(&mut fb, phys, Transform::Normal)?;
        paint(&mut frame)?;
        // No `make_offscreen_sampleable` afterwards: finishing a frame that targets
        // an offscreen leaves it sampleable, with the layout transition riding this
        // submit instead of costing a command buffer, submit and fence wait of its
        // own. A bake's GPU work is negligible, so the round trips were most of what
        // a bake cost.
        let _sync = frame.finish()?;
    }
    Ok(target)
}

/// Bake once with no caching — for widgets that re-draw every frame while
/// animating and bypass their cache (the panel workspace-dot morph, the QS pill
/// fill-fade). Same contract as [`bake`]'s `paint`.
pub fn bake_uncached(
    renderer: &mut VulkanRenderer,
    scale: f64,
    logical_size: Size<f64, Logical>,
    paint: impl FnOnce(&mut VulkanFrame, Size<i32, Physical>) -> anyhow::Result<()>,
) -> anyhow::Result<VkTexture> {
    let phys = physical_size(scale, logical_size);
    bake_uncached_sized(renderer, phys, |frame| paint(frame, phys))
}

// --- H2: logical/pt drawing --------------------------------------------------------------------
//
// A widget describes its chrome in LOGICAL units and GNOME points; `TextShaper`
// and `Painter` perform the one and only `× scale` conversion internally. No
// widget draw site multiplies by scale again — the multiply that got forgotten
// (the minuscule-text bug `3c7473be`) no longer exists at any call site.

/// A text style: a GNOME point size (routed through [`crate::ui::pt_to_px`]) and
/// weight. Color is chosen at draw time (the same shaped run can be drawn in more
/// than one color), so it is not part of the style.
#[derive(Debug, Clone, Copy)]
pub struct TextStyle {
    /// GNOME point size (e.g. 11 for `%heading`). NOT pixels.
    pub pt: f64,
    pub bold: bool,
}

impl TextStyle {
    pub fn new(pt: f64) -> Self {
        Self { pt, bold: false }
    }

    pub fn bold(mut self) -> Self {
        self.bold = true;
        self
    }
}

/// A shaped, rasterized run ready to draw — produced by [`TextShaper::shape`] at a
/// specific scale. Opaque wrapper over the physical [`GlyphRun`]. Drawn via
/// [`Painter::text`] (ink-box anchored + clipped to the buffer),
/// [`text_clipped`](Painter::text_clipped) (a custom clip rect — a header label
/// stopping short of a right-aligned time), or [`text_px`](Painter::text_px) (a
/// hand-computed physical origin — the advance-centered panel clock).
#[derive(Clone)]
pub struct ShapedText {
    run: GlyphRun,
}

impl ShapedText {
    /// Ink bounding box, physical px: `(x, y, w, h)`.
    pub fn ink_bounds(&self) -> (i32, i32, i32, i32) {
        self.run.ink_bounds()
    }

    /// Physical-px top y at which to draw this run so its font line-box (ascent+descent about the
    /// baseline) is vertically centered in a band of `height_px` — GNOME/Pango's centering, which
    /// reserves descent space so caps sit a hair higher than ink centering. Pair with an x from
    /// [`ink_bounds`](Self::ink_bounds) or an advance width. Falls back to ink centering for a
    /// glyph-less run (no line-box metrics).
    pub fn line_box_centered_y(&self, height_px: i32) -> i32 {
        let (baseline, ascent, descent) = self.run.line_box();
        if ascent + descent <= 0. {
            let (_ix, iy, _iw, ih) = self.run.ink_bounds();
            return (height_px - ih) / 2 - iy;
        }
        // Center [baseline - ascent, baseline + descent] in the band, then offset so the run-local
        // baseline lands there: top = band_center - box_height/2 - (baseline - ascent).
        let box_top = (height_px as f32 - (ascent + descent)) / 2.;
        (box_top - (baseline as f32 - ascent)).round() as i32
    }
}

/// One span of a styled paragraph: its text, family, weight, and GNOME point size.
/// The pt → physical conversion happens in [`TextShaper::paragraph`].
#[derive(Debug, Clone, Copy)]
pub struct ParagraphSpan<'a> {
    pub text: &'a str,
    pub mono: bool,
    pub bold: bool,
    /// GNOME point size for this span (spans may differ, e.g. a title vs body).
    pub pt: f64,
}

impl<'a> ParagraphSpan<'a> {
    /// A plain sans span at `pt`.
    pub fn new(text: &'a str, pt: f64) -> Self {
        Self {
            text,
            mono: false,
            bold: false,
            pt,
        }
    }

    pub fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    pub fn mono(mut self) -> Self {
        self.mono = true;
        self
    }
}

/// A shaped, wrapped, multi-span paragraph — the dialog/notification text block.
/// Its ink metrics are **physical** (content-sized widgets lay out in physical px
/// directly); draw it with [`Painter::paragraph`]. Cheap to clone (the glyph
/// atlas is ref-counted), so a widget can shape into a `Vec` then move rows around.
#[derive(Clone)]
pub struct ShapedParagraph {
    run: GlyphRun,
}

impl ShapedParagraph {
    /// Ink bounding box of the whole block, physical px: `(x, y, w, h)`.
    pub fn ink_bounds(&self) -> (i32, i32, i32, i32) {
        self.run.ink_bounds()
    }

    /// Ink bounding box of span index `i` (physical px) — e.g. to draw a keycap
    /// patch behind a monospace command span.
    pub fn span_ink_bounds(&self, i: u32) -> (i32, i32, i32, i32) {
        self.run.span_ink_bounds(i)
    }
}

/// Shapes text at physical (`× scale`) pixels — the miss-only prepare phase (it
/// needs `&mut VulkanRenderer`, which the live frame holds, so shaping must happen
/// before the frame opens). Hand one to a widget's `prepare` closure.
pub struct TextShaper<'a> {
    renderer: &'a mut VulkanRenderer,
    scale: f64,
}

impl<'a> TextShaper<'a> {
    pub fn new(renderer: &'a mut VulkanRenderer, scale: f64) -> Self {
        Self { renderer, scale }
    }

    /// Shape one line. `style.pt` → logical px (`pt_to_px`) → physical px
    /// (`× scale`) — the single font-size conversion.
    pub fn shape(&mut self, text: &str, style: TextStyle) -> anyhow::Result<ShapedText> {
        let px = (crate::ui::pt_to_px(style.pt) * self.scale) as f32;
        let run = self
            .renderer
            .build_glyph_run_weighted(text, px, style.bold)?;
        Ok(ShapedText { run })
    }

    /// Shape a wrapped, center-aligned, multi-span paragraph. `wrap` is the wrap
    /// width in **logical** px; `base_pt` is the line-height reference point size.
    /// Every span's pt is converted the same way as [`Self::shape`] — no call site
    /// touches a physical font size.
    pub fn paragraph(
        &mut self,
        spans: &[ParagraphSpan],
        wrap: f64,
        base_pt: f64,
    ) -> anyhow::Result<ShapedParagraph> {
        use niri_vk::text::{SpanFamily, TextSpan};
        let to_px = |pt: f64| (crate::ui::pt_to_px(pt) * self.scale) as f32;
        let vk_spans: Vec<TextSpan> = spans
            .iter()
            .map(|s| TextSpan {
                text: s.text,
                family: if s.mono {
                    SpanFamily::Mono
                } else {
                    SpanFamily::Sans
                },
                bold: s.bold,
                px: to_px(s.pt),
            })
            .collect();
        let wrap_px = (wrap * self.scale) as f32;
        let run = self
            .renderer
            .build_glyph_paragraph(&vk_spans, wrap_px, to_px(base_pt))?;
        Ok(ShapedParagraph { run })
    }
}

/// Horizontal placement of a run's ink relative to the anchor point.
#[derive(Debug, Clone, Copy)]
pub enum HAlign {
    Left,
    Center,
    Right,
}

/// Vertical placement of a run's ink relative to the anchor point.
#[derive(Debug, Clone, Copy)]
pub enum VAlign {
    Top,
    Middle,
    Bottom,
}

/// How [`Painter::text`] anchors a run's ink box to its `at` point.
#[derive(Debug, Clone, Copy)]
pub struct Align {
    pub h: HAlign,
    pub v: VAlign,
}

impl Align {
    /// Left edge at `at.x`, vertically centered on `at.y` (a row label).
    pub const LEFT_MIDDLE: Align = Align {
        h: HAlign::Left,
        v: VAlign::Middle,
    };
    /// Right edge at `at.x`, vertically centered on `at.y` (a right-aligned label).
    pub const RIGHT_MIDDLE: Align = Align {
        h: HAlign::Right,
        v: VAlign::Middle,
    };
    /// Centered both ways on `at`.
    pub const CENTER: Align = Align {
        h: HAlign::Center,
        v: VAlign::Middle,
    };
    /// Ink top-left corner at `at` (a run baked into an exactly-ink-sized buffer).
    pub const TOP_LEFT: Align = Align {
        h: HAlign::Left,
        v: VAlign::Top,
    };
}

/// GNOME's button families (`_buttons.scss` / `_dialogs.scss`) — the styling a
/// [`Button`] renders with. All carry bold white text; they differ in fill + radius.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonStyle {
    /// `.button` (`%button`) — a subtle raised gray `mix($fg,$bg,9%)`, 8px radius.
    Normal,
    /// `.modal-dialog-button` (`%dialog_button`) — translucent white, 12px radius.
    /// The neutral dialog action button (Cancel / Log Out / …); accent shows only
    /// as the focus ring, matching GNOME 50.1's end-session dialog.
    Dialog,
    /// `.button.default` (`%default_button`) — solid accent fill, 8px radius. A
    /// suggested/primary action.
    Suggested,
    /// `.destructive-action` — solid red `#c01c28`, 8px radius.
    Destructive,
}

impl ButtonStyle {
    /// Corner radius, logical px (`$base_border_radius` 8; dialog buttons `*1.5` = 12).
    pub fn radius(self) -> f64 {
        match self {
            ButtonStyle::Dialog => 12.,
            _ => 8.,
        }
    }

    /// Base (non-hovered) fill; `accent` is the system accent, used by [`Suggested`].
    fn bg(self, accent: Rgba) -> Rgba {
        match self {
            ButtonStyle::Normal => style::BUTTON_BG,
            ButtonStyle::Dialog => style::DIALOG_BUTTON_BG,
            ButtonStyle::Suggested => accent,
            ButtonStyle::Destructive => style::DESTRUCTIVE_BG,
        }
    }
}

/// A clickable button: a rounded [`ButtonStyle`] fill with a centered bold-white
/// label, the toolkit's standard hover wash, and an optional inset accent focus ring.
/// The owner holds the logical `rect` (from its own layout) and the interaction flags;
/// [`Painter::button`] draws it so every button behaves identically wherever it
/// appears. The single higher-level widget in the otherwise-primitive toolkit.
#[derive(Debug, Clone, Copy)]
pub struct Button {
    pub rect: Rectangle<f64, Logical>,
    pub style: ButtonStyle,
    pub hovered: bool,
    /// Keyboard-focused / default action — draws the inset accent focus ring.
    pub focused: bool,
}

impl Button {
    pub fn new(rect: Rectangle<f64, Logical>, style: ButtonStyle) -> Self {
        Self {
            rect,
            style,
            hovered: false,
            focused: false,
        }
    }

    pub fn hovered(mut self, hovered: bool) -> Self {
        self.hovered = hovered;
        self
    }

    pub fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    /// Hit-test a point in the button's own logical coordinate space.
    pub fn contains(&self, p: Point<f64, Logical>) -> bool {
        self.rect.contains(p)
    }
}

/// A `%card`-styled `St.Button` — the dateMenu's launcher cards
/// (`.events-button` / `.world-clocks-button` / `.weather-button`, all
/// `@extend %card`, `_calendar.scss:153-157`): a rounded [`style::CARD_BG`] card
/// that lightens on hover (`%card:hover`, [`style::HOVER_WASH`]) and launches an
/// app on click. This owns only the hover state plus the two easy-to-get-wrong
/// shared bits — the texture-cache revision packing and the hover-wash paint — so
/// each section keeps its own content bake and geometry.
#[derive(Default)]
pub struct CardButton {
    hovered: bool,
}

impl CardButton {
    pub fn hovered(&self) -> bool {
        self.hovered
    }

    /// Set the hover state from whether the pointer is over the card; returns
    /// whether it changed (so the caller can request a redraw / re-bake).
    pub fn set_hovered(&mut self, over: bool) -> bool {
        let changed = self.hovered != over;
        self.hovered = over;
        changed
    }

    /// Pack a texture-cache revision: the content revision in the low 31 bits, the
    /// hover bit at bit 31, and a physical clip-height key in the high 32 — so a
    /// re-cap OR a hover toggle re-bakes with/without the wash (mirrors `bg_texture`).
    pub fn revision(&self, content_rev: u64, height_key: u64) -> u64 {
        (content_rev & 0x7FFF_FFFF) | ((self.hovered as u64) << 31) | (height_key << 32)
    }

    /// Paint the `%card:hover` lighten wash over the card when `hovered` — call
    /// right after filling the card background. Takes the bool explicitly (not
    /// `&self`) because the caller captures it into the `move` bake closure.
    pub fn paint_hover(
        painter: &mut Painter,
        hovered: bool,
        card: Rectangle<f64, Logical>,
        radius: f64,
    ) -> anyhow::Result<()> {
        if hovered {
            painter.fill_rounded(card, radius, style::HOVER_WASH)?;
        }
        Ok(())
    }
}

/// A scale-correct drawing surface over a bound [`VulkanFrame`]. Every verb takes
/// **logical** coordinates/sizes (and points, for text); the single `× scale`
/// conversion lives here. Construct one inside a [`bake`] `paint` closure.
pub struct Painter<'a, 'frame, 'buffer> {
    frame: &'a mut VulkanFrame<'frame, 'buffer>,
    scale: f64,
    full: Rectangle<i32, Physical>,
}

impl<'a, 'frame, 'buffer> Painter<'a, 'frame, 'buffer> {
    /// `phys` is the full baked-buffer size (as handed to the `paint` closure); it
    /// scopes every draw's damage.
    pub fn new(
        frame: &'a mut VulkanFrame<'frame, 'buffer>,
        scale: f64,
        phys: Size<i32, Physical>,
    ) -> Self {
        Self {
            frame,
            scale,
            full: Rectangle::from_size(phys),
        }
    }

    fn px(&self, v: f64) -> i32 {
        to_physical_precise_round::<i32>(self.scale, v)
    }

    fn rect_px(&self, r: Rectangle<f64, Logical>) -> Rectangle<i32, Physical> {
        Rectangle::new(
            Point::from((self.px(r.loc.x), self.px(r.loc.y))),
            Size::from((self.px(r.size.w), self.px(r.size.h))),
        )
    }

    /// Clear the whole buffer to `color` (a transparent clear for rounded popovers,
    /// a border color for square dialogs).
    pub fn clear(&mut self, color: Rgba) -> anyhow::Result<()> {
        // A clear writes its value into the buffer verbatim — no blend — so it has to arrive in the
        // buffer's own (premultiplied) convention, not the toolkit's straight one. Every clear
        // color in the tree today is opaque or fully transparent, where the two agree; this keeps a
        // future translucent clear from silently storing straight alpha.
        self.frame
            .clear(Color32F::from(premultiply(color)), &[self.full])?;
        Ok(())
    }

    /// Fill the whole buffer with `color`, corners cut by `radius` (logical). For a
    /// card whose size is content-derived (a dialog/notification), where the entire
    /// baked buffer *is* the rounded card.
    pub fn fill_rounded_full(&mut self, radius: f64, color: Rgba) -> anyhow::Result<()> {
        let r = (radius * self.scale) as f32;
        self.frame
            .render_rounded_rect(color, r, self.full, &[self.full])?;
        Ok(())
    }

    /// Stroke the whole buffer's edge: an inset ring of `width` logical px, corners cut by
    /// `radius` (logical). The stroke counterpart to [`fill_rounded_full`](Self::fill_rounded_full)
    /// — a 1px border on a card whose buffer *is* the rounded surface (an OSD panel).
    pub fn stroke_rounded_full(
        &mut self,
        radius: f64,
        width: f64,
        color: Rgba,
    ) -> anyhow::Result<()> {
        let r = (radius * self.scale) as f32;
        let w = (width * self.scale) as f32;
        self.frame
            .stroke_rounded_rect(color, r, w, self.full, &[self.full])?;
        Ok(())
    }

    /// Stroke `rect` (logical) with `color`: an inset ring of `width` logical px along the inside
    /// of the edge, corners cut by `radius` (logical; inner corners concentric). A focus ring or
    /// outline — the stroke counterpart to [`fill_rounded`](Self::fill_rounded).
    pub fn stroke_rounded(
        &mut self,
        rect: Rectangle<f64, Logical>,
        radius: f64,
        width: f64,
        color: Rgba,
    ) -> anyhow::Result<()> {
        let r = (radius * self.scale) as f32;
        let w = (width * self.scale) as f32;
        self.frame
            .stroke_rounded_rect(color, r, w, self.rect_px(rect), &[self.full])?;
        Ok(())
    }

    /// Fill `rect` (logical) with `color`, corners cut by `radius` (logical; 0 = a
    /// plain rectangle, e.g. a separator rule).
    pub fn fill_rounded(
        &mut self,
        rect: Rectangle<f64, Logical>,
        radius: f64,
        color: Rgba,
    ) -> anyhow::Result<()> {
        let r = (radius * self.scale) as f32;
        self.frame
            .render_rounded_rect(color, r, self.rect_px(rect), &[self.full])?;
        Ok(())
    }

    /// Draw an [`AppIcon`] tile's **state** layer (not the icon pixels — those ride
    /// on top as an [`app_icon_element`]). Normal is a no-op: a flat `.overview-icon`
    /// tile shares its parent's background (`_drawing.scss:175-177`), so nothing is
    /// drawn. Hovered fills the tile with `hover_bg`, the surface-specific hover color
    /// (GNOME's flat+`always_dark` `st-lighten`, `_drawing.scss:186-189,270-274` — the
    /// lighten *direction* is the caller's, read from the SCSS).
    pub fn app_tile(&mut self, tile: &AppIcon, hover_bg: Rgba) -> anyhow::Result<()> {
        if tile.hovered {
            self.fill_rounded(tile.rect, tile.radius, hover_bg)?;
        }
        Ok(())
    }

    /// Paint one labelled `.overview-tile` into a card bake: its selection/hover wash
    /// (when `active`) and its caption. `rel` is the tile box relative to the bake
    /// origin; the icon pixels are composited separately on top as an
    /// [`app_icon_element`]. Shared by the search results and the app grid.
    pub fn labelled_tile(
        &mut self,
        rel: Rectangle<f64, Logical>,
        label: &ShapedText,
        metrics: &TileMetrics,
        active: bool,
        text_color: Rgba,
    ) -> anyhow::Result<()> {
        if active {
            self.app_tile(
                &AppIcon {
                    rect: rel,
                    hovered: true,
                    radius: metrics.radius,
                },
                style::HOVER_WASH,
            )?;
        }
        // Label centered under the icon, clipped to the tile so a long name doesn't
        // widen the grid.
        let lx = rel.loc.x + rel.size.w / 2.;
        let ly = rel.loc.y + metrics.pad + metrics.icon_px + metrics.label_gap;
        self.text_band(
            label,
            lx,
            HAlign::Center,
            ly,
            metrics.label_h,
            text_color,
            rel,
        )?;
        Ok(())
    }

    /// Draw GNOME's `box-shadow`: a gaussian-blurred rounded rect behind a card.
    /// `rect`/`radius` are the casting box (logical); `blur` is the CSS blur radius
    /// (logical px; the gaussian σ = blur/2); `offset` shifts the shadow (logical —
    /// GNOME's panel shadows are `0 <dy>`); `color` is straight-alpha (premultiplied
    /// downstream). Draw this BEFORE the card fill so the card sits on top. The fringe
    /// bleeds ~`blur`·1.5 (3σ) beyond `rect` + `offset`, so the bake buffer must carry
    /// that much transparent padding around the card or the shadow clips at the edge
    /// (the OSD-panel callers size the buffer for it).
    pub fn drop_shadow(
        &mut self,
        rect: Rectangle<f64, Logical>,
        radius: f64,
        blur: f64,
        offset: (f64, f64),
        color: Rgba,
    ) -> anyhow::Result<()> {
        let sigma = (blur * self.scale / 2.) as f32;
        let mut box_dst = self.rect_px(rect);
        box_dst.loc.x += self.px(offset.0);
        box_dst.loc.y += self.px(offset.1);
        let r = (radius * self.scale) as f32;
        self.frame
            .render_drop_shadow(color, r, sigma, self.scale as f32, box_dst, &[self.full])?;
        Ok(())
    }

    /// Draw a shaped run, anchoring its ink box to `at` (logical) per `align`,
    /// tinted `color`. Clipped to the whole buffer.
    pub fn text(
        &mut self,
        shaped: &ShapedText,
        at: Point<f64, Logical>,
        align: Align,
        color: Rgba,
    ) -> anyhow::Result<()> {
        self.render_run(shaped, at, align, color, self.full)
    }

    /// Like [`text`](Self::text), but clips the glyphs to `clip` (logical) instead of
    /// the whole buffer — for a run that must not overrun a sibling (a header label
    /// stopping short of a right-aligned time, a body column, a button-width label).
    /// This is what lets content-driven widgets draw every run through the `Painter`
    /// rather than reaching for `VulkanFrame::render_glyphs`.
    pub fn text_clipped(
        &mut self,
        shaped: &ShapedText,
        at: Point<f64, Logical>,
        align: Align,
        color: Rgba,
        clip: Rectangle<f64, Logical>,
    ) -> anyhow::Result<()> {
        let clip = self.rect_px(clip);
        self.render_run(shaped, at, align, color, clip)
    }

    /// Shared origin math for [`text`](Self::text)/[`text_clipped`](Self::text_clipped):
    /// place the run's ink box at `at` per `align`, then draw clipped to `clip` (physical),
    /// damaging the whole buffer.
    fn render_run(
        &mut self,
        shaped: &ShapedText,
        at: Point<f64, Logical>,
        align: Align,
        color: Rgba,
        clip: Rectangle<i32, Physical>,
    ) -> anyhow::Result<()> {
        let (ix, iy, iw, ih) = shaped.run.ink_bounds();
        let ax = self.px(at.x);
        let ay = self.px(at.y);
        let ox = match align.h {
            HAlign::Left => ax - ix,
            HAlign::Center => ax - ix - iw / 2,
            HAlign::Right => ax - ix - iw,
        };
        let oy = match align.v {
            VAlign::Top => ay - iy,
            VAlign::Middle => ay - iy - ih / 2,
            VAlign::Bottom => ay - iy - ih,
        };
        self.frame.render_glyphs(
            &shaped.run,
            Point::from((ox, oy)),
            color,
            clip,
            &[self.full],
        )?;
        Ok(())
    }

    /// Draw `shaped` horizontally per `halign` at logical `x`, vertically centered by
    /// its font **line box** within the band `[band_top, band_top + band_h]` (logical),
    /// clipped to `clip` (logical). Unlike [`text`](Self::text)'s ink-box centering,
    /// line-box centering keeps baselines aligned across side-by-side cells regardless
    /// of which strings carry descenders (the grid-row idiom, as the panel does for its
    /// clock/labels) — see [`ShapedText::line_box_centered_y`].
    #[allow(clippy::too_many_arguments)]
    pub fn text_band(
        &mut self,
        shaped: &ShapedText,
        x: f64,
        halign: HAlign,
        band_top: f64,
        band_h: f64,
        color: Rgba,
        clip: Rectangle<f64, Logical>,
    ) -> anyhow::Result<()> {
        let (ix, _iy, iw, _ih) = shaped.run.ink_bounds();
        let ax = self.px(x);
        let ox = match halign {
            HAlign::Left => ax - ix,
            HAlign::Center => ax - ix - iw / 2,
            HAlign::Right => ax - ix - iw,
        };
        let oy = self.px(band_top) + shaped.line_box_centered_y(self.px(band_h));
        let clip = self.rect_px(clip);
        self.frame.render_glyphs(
            &shaped.run,
            Point::from((ox, oy)),
            color,
            clip,
            &[self.full],
        )?;
        Ok(())
    }

    /// Draw a shaped run at a precomputed **physical** glyph-layout `origin`, tinted
    /// `color`, clipped to the whole buffer. The physical-coordinate counterpart to
    /// [`text`](Self::text) for a run whose placement isn't a simple ink-box anchor —
    /// the panel clock is *advance-centered* (tabular figures keep it from jittering as
    /// the seconds tick), so its origin is computed by hand rather than via [`Align`].
    pub fn text_px(
        &mut self,
        shaped: &ShapedText,
        origin: Point<i32, Physical>,
        color: Rgba,
    ) -> anyhow::Result<()> {
        self.frame
            .render_glyphs(&shaped.run, origin, color, self.full, &[self.full])?;
        Ok(())
    }

    /// Fill a **physical** sub-rect with a solid `color` (no rounding). The
    /// physical-coordinate counterpart to [`fill_rounded`](Self::fill_rounded) for
    /// content-sized widgets whose chrome (a dialog's border/interior, a keycap
    /// patch) is laid out in physical px next to a [`ShapedParagraph`].
    pub fn fill_rect_px(
        &mut self,
        rect: Rectangle<i32, Physical>,
        color: Rgba,
    ) -> anyhow::Result<()> {
        // Premultiplied for the same reason as [`clear`](Self::clear) — this is a clear too.
        self.frame
            .clear(Color32F::from(premultiply(color)), &[rect])?;
        Ok(())
    }

    /// A crisp separator hairline filling the logical `rect` (one dimension is its 1px thickness).
    /// Snapped to device pixels and painted with [`fill_rect_px`](Self::fill_rect_px) — a *clear*,
    /// so a hairline keeps full coverage where an SDF `fill_rounded` would anti-alias both edges
    /// and halve a 1px line (`_message-list.scss` / `_popovers.scss` `$borders_color` all but
    /// vanished as a rounded fill). Because it clears (replaces, not blends), pass an **opaque**
    /// color when drawing over opaque content — use [`style::over`] to pre-blend
    /// [`style::BORDERS`] onto the surface; over a transparent bake layer the translucent
    /// `BORDERS` itself is correct (it blends when the layer composites). The single home for both
    /// the calendar column separator and the QS group separators.
    pub fn hairline(&mut self, rect: Rectangle<f64, Logical>, color: Rgba) -> anyhow::Result<()> {
        let mut px = self.rect_px(rect);
        px.size.w = px.size.w.max(1);
        px.size.h = px.size.h.max(1);
        self.fill_rect_px(px, color)
    }

    /// Draw a shaped paragraph block with its layout frame's top-left at `origin`
    /// (**physical** — paragraphs are physical-native; see [`ShapedParagraph`]),
    /// tinted `color`, clipped to the whole buffer. The physical-coordinate
    /// counterpart to [`text`](Self::text) for wrapped, multi-span text blocks.
    pub fn paragraph(
        &mut self,
        shaped: &ShapedParagraph,
        origin: Point<i32, Physical>,
        color: Rgba,
    ) -> anyhow::Result<()> {
        self.frame
            .render_glyphs(&shaped.run, origin, color, self.full, &[self.full])?;
        Ok(())
    }

    /// Draw a shaped paragraph with per-span colors: `colors[i]` tints span `i`
    /// (`origin` **physical**, clipped to the whole buffer). For runs whose spans
    /// carry distinct colors — the MRU scope panel's selected/unselected tokens.
    pub fn paragraph_spans(
        &mut self,
        shaped: &ShapedParagraph,
        origin: Point<i32, Physical>,
        colors: &[Rgba],
    ) -> anyhow::Result<()> {
        self.frame
            .render_glyphs_spans(&shaped.run, origin, colors, self.full, &[self.full])?;
        Ok(())
    }

    /// Draw a [`Button`]: the rounded style fill (+ hover wash), an accent focus ring
    /// when focused, then the centered bold-white `label`. `accent` is the system
    /// accent — the focus-ring color, and the fill for [`ButtonStyle::Suggested`].
    ///
    /// The focus ring is GNOME's inset 2px accent stroke ([`stroke_rounded`](Self::stroke_rounded))
    /// on the button's own rect, drawn over the fill — faithful, and correct over a translucent
    /// [`ButtonStyle::Dialog`] fill with no masking.
    pub fn button(&mut self, b: &Button, label: &ShapedText, accent: Rgba) -> anyhow::Result<()> {
        let radius = b.style.radius();
        self.fill_rounded(b.rect, radius, b.style.bg(accent))?;
        if b.hovered {
            self.fill_rounded(b.rect, radius, style::HOVER_WASH)?;
        }
        if b.focused {
            let ring_color = [accent[0], accent[1], accent[2], 0.8];
            self.stroke_rounded(b.rect, radius, 2., ring_color)?;
        }
        let center = Point::from((
            b.rect.loc.x + b.rect.size.w / 2.,
            b.rect.loc.y + b.rect.size.h / 2.,
        ));
        self.text_clipped(label, center, Align::CENTER, style::TEXT, b.rect)?;
        Ok(())
    }
}

/// Test-only scale-sweep harness (H4 in the design doc). Bakes a widget at scales
/// {1.0, 1.5, 2.0} and asserts the buffer is physically `round(logical × scale)`
/// and that the glyph **ink area grows with the square of the scale** — the
/// assertion that would have caught the input-source popover's minuscule text
/// (`3c7473be`), where text shaped at logical px kept a constant glyph size at
/// every scale.
///
/// Ink *area* (bright-pixel count), not the ink bounding box: a widget's ink bbox
/// spans its top row to its bottom row, so its height tracks the buffer (row
/// layout) regardless of per-glyph size and cannot see shrunk glyphs. Glyph ink
/// area scales as `font_px²`, i.e. `scale²` when the text is correctly sized — so
/// scale-1→2 area ≈ 4×; the bug (constant font_px) leaves it ≈ 1×.
///
/// `bake_at` bakes the widget at the given scale and returns its texture; pass the
/// widget's scale-independent `logical_size`.
#[cfg(test)]
pub fn assert_scale_correct(
    vk: &mut VulkanRenderer,
    logical_size: Size<f64, Logical>,
    mut bake_at: impl FnMut(&mut VulkanRenderer, f64) -> VkTexture,
) {
    use smithay::backend::renderer::{ExportMem, Texture};
    use smithay::utils::Rectangle;

    // (scale, bright-pixel count) collected across the sweep.
    let mut ink: Vec<(f64, u64)> = Vec::new();

    for scale in [1.0, 1.5, 2.0] {
        let expected = physical_size(scale, logical_size);
        let mut tex = bake_at(vk, scale);

        let size = tex.size();
        assert_eq!(
            (size.w, size.h),
            (expected.w, expected.h),
            "scale {scale}: buffer size {size:?} != round(logical × scale) {expected:?}",
        );

        // Read the baked pixels back and count "ink" — pixels clearly brighter than
        // the dark widget background (text is near-white; the dark rounded bg and
        // the low-alpha separator stay well under the threshold).
        let fb = vk.bind(&mut tex).expect("bind for readback");
        let region = Rectangle::<i32, BufferCoord>::from_size(size);
        let mapping = vk
            .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
            .expect("copy_framebuffer");
        let pixels = vk.map_texture(&mapping).expect("map_texture").to_vec();

        let count = pixels
            .chunks_exact(4)
            .filter(|p| p[0] > 150 && p[1] > 150 && p[2] > 150)
            .count() as u64;
        assert!(
            count > 20,
            "scale {scale}: expected visible glyph ink, got {count} bright pixels",
        );
        ink.push((scale, count));
    }

    // The regression pin: ink area must grow ~scale². Correct text quadruples from
    // scale 1 to 2; text shaped at logical px (the bug) stays ~flat (≈1×), far below
    // the band. A wide band absorbs glyph-hinting/anti-aliasing noise while leaving
    // the 4×-vs-1× gap unmissable (per the review — no reliance on exact linearity).
    let ratio = ink[2].1 as f64 / ink[0].1 as f64;
    assert!(
        (2.5..=6.0).contains(&ratio),
        "ink area should grow ~4× (scale²) from scale 1 to 2, got {ratio:.2} \
         (counts {ink:?}) — a ratio near 1 means text was shaped at logical px \
         instead of physical (the HiDPI bug class)",
    );
}

#[cfg(test)]
mod tests {
    use smithay::backend::allocator::Fourcc;
    use smithay::backend::renderer::{Bind, ExportMem, Texture};
    use smithay::utils::{Buffer as BufferCoord, Logical, Physical, Point, Rectangle, Size};

    use super::{bake_uncached_sized, Painter};
    use crate::render_helpers::vulkan::VulkanRenderer;

    /// The gaussian drop-shadow verb: a black shadow over a white buffer must darken the
    /// casting box to near-black, fade through mid-grey in the blur fringe just outside it,
    /// and leave the far corner (beyond ~3σ) untouched white. Pins `Painter::drop_shadow`'s
    /// SDF placement + blur falloff over the shared `render_shadow` material.
    #[test]
    fn drop_shadow_casts_a_fading_fringe() {
        let mut vk = match VulkanRenderer::new() {
            Ok(vk) => vk,
            Err(e) => {
                eprintln!("skipping drop_shadow_casts_a_fading_fringe: no Vulkan device ({e})");
                return;
            }
        };

        let scale = 1.0;
        let size = Size::<i32, Physical>::from((100, 100));
        // Box spans 30..70; blur 10 → σ=5, so the fringe reaches ~15px (15..85 shades) and
        // the (95,95) corner stays white.
        let mut tex = bake_uncached_sized(&mut vk, size, |frame| {
            let mut p = Painter::new(frame, scale, size);
            p.clear([1., 1., 1., 1.])?;
            let box_rect =
                Rectangle::<f64, Logical>::new(Point::from((30., 30.)), Size::from((40., 40.)));
            p.drop_shadow(box_rect, 12., 10., (0., 0.), [0., 0., 0., 1.])?;
            Ok(())
        })
        .expect("bake");

        let tex_size = tex.size();
        let fb = vk.bind(&mut tex).expect("bind");
        let region = Rectangle::<i32, BufferCoord>::from_size(tex_size);
        let mapping = vk
            .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
            .expect("copy_framebuffer");
        let pixels = vk.map_texture(&mapping).expect("map_texture").to_vec();

        // Opaque white base + premultiplied-black shadow → each channel reads (1 − α)·255,
        // so a plain channel average is the shadow strength (0 = full shadow, 255 = none).
        let lum = |x: i32, y: i32| -> i32 {
            let i = ((y * 100 + x) * 4) as usize;
            (pixels[i] as i32 + pixels[i + 1] as i32 + pixels[i + 2] as i32) / 3
        };
        let center = lum(50, 50);
        let fringe = lum(72, 50);
        let corner = lum(95, 95);

        assert!(
            center < 60,
            "box center should be near-black shadow, got {center}"
        );
        assert!(corner > 200, "far corner should stay white, got {corner}");
        assert!(
            fringe > center + 20 && fringe < corner - 20,
            "fringe just outside the box should be mid-grey (blur falloff): \
             center {center}, fringe {fringe}, corner {corner}",
        );
    }
}
