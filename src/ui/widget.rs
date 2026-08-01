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
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::{Bind, Color32F, ContextId, Frame as _, Offscreen, Renderer};
use smithay::utils::{Buffer as BufferCoord, Logical, Physical, Point, Rectangle, Size, Transform};

use crate::app_system::AppIconRef;
use crate::image_source::ImageSource;
use crate::render_helpers::icon::{AppIconCache, IconCache, ImageCache};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{
    premultiply, GlyphRun, VkTexture, VulkanFrame, VulkanRenderer, NATIVE_FOURCC,
};
use crate::utils::to_physical_precise_round;

/// The upload cache for full-color app icons, keyed by
/// `(scale, descriptor, logical size)`. Lives on a baking widget (the dash, later
/// the grid/search) so a hover re-bake doesn't re-upload the icon textures.
pub type AppIconUploads = HashMap<(NotNan<f64>, AppIconRef, u16), TextureBuffer<VkTexture>>;

/// One [`AppIconUploads`] shared by every surface that draws app icons.
///
/// gnome-shell keeps **one** Cogl texture per gicon+size for the whole shell
/// (`st-texture-cache.c:998`, `POLICY_FOREVER`), so an app that appears in the dash, the
/// grid and a search result is uploaded once. Ours were per surface, which meant typing a
/// query re-uploaded, at the same size, icons the grid already had on the GPU.
///
/// Each surface still keeps its own renderer-context check and clears this when *it* sees
/// a new context; the first one through does the work and the rest find it already empty.
pub type SharedAppIconUploads = std::rc::Rc<std::cell::RefCell<AppIconUploads>>;

/// Drop the uploads of one icon at one logical size, across every output scale.
///
/// The upload key carries no notion of *which* decode produced the pixels, so a
/// texture uploaded from a stale buffer would otherwise be served forever once the
/// fresh decode landed. Surfaces call this as each decode arrives, which is also why
/// nothing needs to clear the whole map on a theme or catalog change.
pub fn drop_app_icon_upload(uploads: &mut AppIconUploads, icon: &AppIconRef, logical_px: u16) {
    uploads.retain(|(_, cached, px), _| cached != icon || *px != logical_px);
}

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

    /// `%osd_panel` background — `$osd_bg_color` = `lighten(#222226, 5%)` ≈ `#2e2e33`
    /// (`_colors.scss:17`). Shared: the OSD pill and the Alt-Tab `.switcher-list` both extend
    /// `%osd_panel` (`_common.scss:294`, `_switcher-popup.scss:11`), so they are one value.
    pub const OSD_BG: Rgba = [0.180, 0.180, 0.200, 1.];
    /// `%osd_panel` hairline — `$osd_outer_borders_color` =
    /// `transparentize($osd_fg_color, 0.98)` (`_colors.scss:44`).
    pub const OSD_BORDER: Rgba = [1., 1., 1., 0.02];
    /// `$osd_fg_color` = `$light_1` (`_colors.scss:16`) — the foreground on an OSD panel.
    pub const OSD_FG: Rgba = TEXT;

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
    /// A `.button` / `.icon-button` sitting **on** [`OVERLAY_BG`] — `button(normal)`'s
    /// `st-mix($system_fg_color, $system_overlay_bg_color, 9%)` (`_drawing.scss:171`,
    /// `$background_mix_factor` 9%), i.e. 9% of #fafafb over the overlay surface ≈ `#49494d`.
    /// NOT the surface itself: `$c` is the background the button sits on, not its fill, and
    /// using it directly makes the button invisible against its own container.
    pub const OVERLAY_BUTTON_BG: Rgba = [0.287, 0.287, 0.301, 1.];

    /// `.app-folder` fill — the one tile in the grid that is **raised** rather than
    /// flat: `tile_button($bg:$system_base_color, $raised:true)` (`_app-grid.scss:41`)
    /// resolves to `button(normal)`'s `st-mix($system_fg_color, $system_base_color, 9%)`
    /// = `mix(#fafafb, #222226, 9%)` ≈ `#353539` (`_drawing.scss:353-354`,
    /// `$background_mix_factor` `_default-colors.scss:33`). An app tile in the same grid
    /// is `$style: flat` and forced transparent at rest, which is why only folders show
    /// a resting background.
    pub const FOLDER_BG: Rgba = [0.210, 0.210, 0.224, 1.];

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
    icon_element_alpha(
        renderer, icons, candidates, logical_px, scale, color, origin, center, 1.,
    )
}

/// [`icon_element`] with an explicit element `alpha` — for a surface that fades as a
/// whole (the OSD), where the icon rides on top of the fading bake rather than
/// inside it and so must fade with it.
#[allow(clippy::too_many_arguments)]
pub fn icon_element_alpha<S: AsRef<str>>(
    renderer: &mut VulkanRenderer,
    icons: &IconCache,
    candidates: &[S],
    logical_px: f64,
    scale: f64,
    color: Rgba,
    origin: Point<f64, Logical>,
    center: Point<f64, Logical>,
    alpha: f32,
) -> Option<TextureRenderElement<VkTexture>> {
    let tb = candidates
        .iter()
        .find_map(|name| icons.texture(renderer, name.as_ref(), logical_px, scale, color))?;
    let logical = tb.logical_size();
    let loc = origin + center - Point::from((logical.w / 2., logical.h / 2.));
    Some(TextureRenderElement::from_texture_buffer(
        tb,
        loc,
        alpha,
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

/// Uploaded textures for images an app pointed us at, keyed by an owner-chosen slot id and the
/// source. See [`image_element`]; owners prune on the slot id.
pub type ImageUploads = HashMap<(u64, ImageSource), TextureBuffer<VkTexture>>;

/// Composite an image an app pointed us at (album art today), loaded by the [`ImageCache`],
/// centered at `center` (relative to the element `origin`), fitted into a `logical_px` square.
///
/// The full-color, app-content sibling of [`app_icon_element`]. Two differences, both because the
/// source is content some *app* chose rather than an installed asset:
///
/// - a source that will not load yields `None` rather than GNOME's executable glyph — the caller
///   draws its own fallback (a media card's `audio-x-generic-symbolic`);
/// - the upload slot is keyed by source as well as id, so an owner that reuses slots cannot serve
///   the previous image for a new one.
///
/// The load itself is aspect-fit and centered on a transparent square (`decode_image_bytes`), which
/// is what makes placing it like any other centered icon reproduce St's `RESIZE_ASPECT` gravity.
/// `None` while an async load is still in flight — the caller draws its fallback until it lands, so
/// the arrival has to invalidate whatever cached it (see `media_card`).
#[allow(clippy::too_many_arguments)]
pub fn image_element(
    renderer: &mut VulkanRenderer,
    uploads: &mut ImageUploads,
    images: &ImageCache,
    source: &ImageSource,
    slot: u64,
    logical_px: f64,
    scale: f64,
    origin: Point<f64, Logical>,
    center: Point<f64, Logical>,
    alpha: f32,
) -> Option<TextureRenderElement<VkTexture>> {
    let key = (slot, source.clone());
    #[allow(clippy::map_entry)]
    if !uploads.contains_key(&key) {
        let buffer = images.buffer(source, logical_px, scale)?;
        match TextureBuffer::from_memory_buffer(renderer, &buffer) {
            Ok(tb) => {
                uploads.insert(key.clone(), tb);
            }
            Err(err) => {
                tracing::error!("error uploading image {source:?}: {err:#}");
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

/// GNOME's `%tooltip` (`_common.scss:225-238`) — the black pill behind a short
/// label. Shared by `.window-caption` (the overview preview title,
/// `_window-picker.scss:24-26`), `.dash-label` (`_dash.scss:103-106`) and the
/// screenshot UI's tip (`_screenshot.scss:200`), so it lands here rather than in
/// the first caller.
///
/// The widget owns its box model and its paint; the caller owns placement and the
/// bake cache (captions differ per instance, so one cache per *text* is the
/// natural key).
#[derive(Debug, Clone, Copy)]
pub struct Tooltip;

impl Tooltip {
    /// `padding: $base_padding $base_padding * 2` (`_common.scss:231`).
    pub const PAD_V: f64 = 6.;
    pub const PAD_H: f64 = 12.;
    /// `border: 1px solid transparentize($light_1, 0.9)` (`:227`).
    pub const BORDER: f64 = 1.;
    /// `background-color: transparentize(black, 0.1)` (`:226`).
    pub const BG: Rgba = [0., 0., 0., 0.9];
    /// The 1px border — white at 10%.
    pub const BORDER_COLOR: Rgba = style::BORDERS;
    /// The label inherits the stage's 11pt body size; `%tooltip` sets no font.
    pub const TEXT_PT: f64 = 11.;

    /// The pill's box for `text`, at the shared body size. Height is fixed by the
    /// box model (it is a single line), width is the label plus padding.
    pub fn size(text: &str) -> Size<f64, Logical> {
        let px = crate::ui::pt_to_px(Self::TEXT_PT);
        let w = niri_vk::text::measure_line_width_weighted(text, px as f32, false);
        Size::from((w + 2. * Self::PAD_H, px.ceil() + 2. * Self::PAD_V))
    }

    /// `border-radius: $forced_circular_radius` — a pill, so half the height.
    pub fn radius(size: Size<f64, Logical>) -> f64 {
        size.h / 2.
    }
}

impl Painter<'_, '_, '_> {
    /// Paint a [`Tooltip`] filling the whole buffer, with `label` centred in it.
    /// `text-align: center` (`_common.scss:232`).
    pub fn tooltip(&mut self, size: Size<f64, Logical>, label: &ShapedText) -> anyhow::Result<()> {
        let radius = Tooltip::radius(size);
        self.clear(style::TRANSPARENT)?;
        self.fill_rounded_full(radius, Tooltip::BG)?;
        self.stroke_rounded_full(radius, Tooltip::BORDER, Tooltip::BORDER_COLOR)?;
        self.text(
            label,
            Point::from((size.w / 2., size.h / 2.)),
            Align::CENTER,
            style::TEXT,
        )
    }
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
    ///
    /// A function rather than a const because `label_h` is a *line box*, which depends on the
    /// realized font — it was pinned at 18 while the caption draws at `LABEL_PT` (the base 11pt),
    /// whose box is 19. That one pixel is load-bearing: a `BaseIcon` is a `SquareBin`, so the
    /// tile's width follows its content height and the whole tile was a pixel narrow with it.
    pub fn overview() -> Self {
        Self {
            icon_px: 96.,
            pad: AppIcon::OVERVIEW_TILE_PADDING,
            label_gap: 6.,
            label_h: crate::ui::line_height_px(crate::ui::BASE_FONT_PT),
            radius: AppIcon::OVERVIEW_TILE_RADIUS,
        }
    }

    /// The tile's outer size — **square**: [`Self::label_w`] plus padding on each side.
    ///
    /// A tile is `.overview-tile` padding around a `BaseIcon`, and a `BaseIcon` is a
    /// `Shell.SquareBin` (`iconGrid.js:62`) whose preferred *width* is its preferred
    /// *height* (`shell-square-bin.c:14-30`). So the width follows the content height —
    /// icon + spacing + one caption line — and not, as this used to say, the icon alone.
    /// At GNOME's metrics that is 144, not 120: the tile was 24 px too narrow, which
    /// showed as a hover wash narrower than GNOME's.
    pub fn size(&self) -> Size<f64, Logical> {
        let side = self.pad + self.label_w() + self.pad;
        Size::from((side, side))
    }

    /// How far a caption of `lines` hangs below the tile box.
    ///
    /// The tile is sized for **one** caption line — that is the `Shell.SquareBin` rule
    /// and it is what keeps the cell square, so reserving our resting
    /// [`TILE_LABEL_LINES`] in the box instead would cost a rung of the icon ladder on
    /// exactly the small canvases where the second line was supposed to help. So the
    /// extra line hangs into the row gap (6px of it, the rest is the tile's own bottom
    /// padding) and whatever is *drawn* around the caption — a hover wash, a focus ring,
    /// a folder tile's bubble — grows by this much instead.
    pub fn caption_overhang(&self, lines: usize) -> f64 {
        self.label_h * (lines.max(1) as f64 - 1.)
    }

    /// The width the caption is laid out in: the `Shell.SquareBin` box, i.e. the tile
    /// minus its padding (see [`Self::size`] for why that box is square).
    pub fn label_w(&self) -> f64 {
        self.icon_px + self.label_gap + self.label_h
    }

    /// Top of the caption band within a tile box `rect` (logical).
    pub fn label_top(&self, rect: Rectangle<f64, Logical>) -> f64 {
        rect.loc.y + self.pad + self.icon_px + self.label_gap
    }

    /// The icon's center within a tile box `rect` (logical) — the icon sits at the
    /// top of the tile, the label below it.
    pub fn icon_center(&self, rect: Rectangle<f64, Logical>) -> Point<f64, Logical> {
        Point::from((
            rect.loc.x + rect.size.w / 2.,
            rect.loc.y + self.pad + self.icon_px / 2.,
        ))
    }

    /// One folder sub-icon's side, in logical px — [`FOLDER_SUBICON_FRACTION`] of the
    /// tile's icon box (`createFolderIcon`, `appDisplay.js:2149`).
    pub fn folder_subicon_px(&self) -> f64 {
        (FOLDER_SUBICON_FRACTION * self.icon_px).floor()
    }

    /// The center of folder sub-icon `i` (`0..4`, filled left-to-right then top-to-
    /// bottom) within a tile box `rect`.
    ///
    /// `createFolderIcon` (`appDisplay.js:2138-2162`) composes the folder's icon as a
    /// **homogeneous** 2×2 `Clutter.GridLayout` over an icon-box-sized widget, one
    /// member per cell at `(i % 2, i / 2)`. Homogeneous means each cell is half the
    /// box; the member icon is smaller than its cell and paints centered in it
    /// (`st-icon.c:478-479` center-aligns the texture inside the actor), which is what
    /// leaves the gap down the middle of a folder tile.
    pub fn folder_subicon_center(
        &self,
        rect: Rectangle<f64, Logical>,
        i: usize,
    ) -> Point<f64, Logical> {
        let center = self.icon_center(rect);
        let quarter = self.icon_px / 4.;
        let dx = if i % 2 == 0 { -quarter } else { quarter };
        let dy = if i / 2 == 0 { -quarter } else { quarter };
        Point::from((center.x + dx, center.y + dy))
    }
}

/// The share of a folder tile's icon box that one member sub-icon takes
/// (`FOLDER_SUBICON_FRACTION`, `appDisplay.js:31`).
pub const FOLDER_SUBICON_FRACTION: f64 = 0.4;

/// How many lines an *expanded* tile caption may use.
///
/// GNOME does not cap it — `line_wrap: true` with `ellipsize: NONE` lets the label
/// grow and the tile's allocation follow it. A bake needs a size up front, so we cap;
/// three lines at the caption's wrap width clears every name in a default install,
/// and [`tile_label_lines`] ellipsizes past it rather than dropping text silently.
pub const TILE_LABEL_EXPAND_LINES: usize = 3;

/// How many lines a *resting* app-grid caption may use.
///
/// **Divergence (chosen).** GNOME's is one: `StLabel` puts `PANGO_ELLIPSIZE_END` on its
/// `ClutterText` (`st-label.c:331`) and the collapsed state turns wrapping off outright
/// (`_updateMultiline`, `appDisplay.js:1891-1924`), so a two-word name is cut at rest and
/// only readable on hover. Two lines read most names without hovering, and the tile has
/// the room: a second line takes the tile's bottom padding and 6 px of the row gap, which
/// is at minimum spacing still 18 px clear of the icon below.
///
/// Search results are **not** this: they keep GNOME's single line (see the call site).
pub const TILE_LABEL_LINES: usize = 2;

/// The caption lines of a `.overview-tile`, top to bottom, for a label box `wrap_w`
/// wide (see [`TileMetrics::label_w`]).
///
/// GNOME's two states (`AppViewItem._updateMultiline`, `appDisplay.js:1891-1924`):
///
/// * **collapsed** — ellipsized at the end. That is not something the app grid opts into: `StLabel`
///   sets `PANGO_ELLIPSIZE_END` on its `ClutterText` (`st-label.c:331`), so it is what *every*
///   caption does, search results included (they pass `expandTitleOnHover: false`,
///   `appDisplay.js:1837-1841`, and so never leave this state). GNOME's collapsed label is one
///   line; the grid passes [`TILE_LABEL_LINES`], which is the divergence recorded there.
/// * **expanded** — hover, key focus, or the forced highlight of an open context menu
///   (`appDisplay.js:1901`): wrapping on (`Pango.WrapMode.WORD_CHAR`), ellipsis off, so the whole
///   name is readable.
///
/// Break points are computed in **logical** px, so they do not move with the output
/// scale; the result is memoized in [`niri_vk::text::wrap_lines_weighted`].
/// `max_lines` is which of the two states this is: [`TILE_LABEL_LINES`] collapsed,
/// [`TILE_LABEL_EXPAND_LINES`] expanded — or 1 for a caption that is neither, like a
/// search result's.
///
/// `break_words` is Pango's `WORD_CHAR` vs `WORD`, and it goes with the state: **expanded**
/// breaks words, because the point of expanding is to show the whole name and a word wider
/// than the tile has nowhere else to go. **At rest** it must not — a resting caption that
/// splits "Graphics" into "Graphi/cs" reads as broken, where "Graphic…" reads as a name
/// that did not fit. (GNOME never faces this: its resting label is one line, so there is
/// no wrap to choose a mode for.)
pub fn tile_label_lines(
    name: &str,
    pt: f64,
    wrap_w: f64,
    max_lines: usize,
    break_words: bool,
) -> Vec<String> {
    let px = crate::ui::pt_to_px(pt) as f32;
    niri_vk::text::wrap_lines_weighted(name, px, false, wrap_w, max_lines.max(1), break_words)
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
    #[track_caller]
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

/// A bake's `revision`, **derived from the values its paint closure reads** instead of maintained
/// beside them.
///
/// ```ignore
/// let revision = Revision::new()
///     .of(self.entries.len())
///     .of(&self.title)
///     .of(self.expanded)
///     .px(self.height)
///     .color(self.accent)
///     .done();
/// ```
///
/// # Why this exists
///
/// A hand-bumped `u64` is a *proxy* for "what this bake reads", kept somewhere else, with nothing
/// checking that the two agree. Every cache bug this codebase has had is that disagreement, in one
/// direction or the other:
///
/// - **Bumped when nothing baked changed** → a full GPU round trip on every frame of an animation.
///   The panel re-baked its whole bar for the length of every overview animation because opening
///   the overview checks the Activities button, whose *fade* made `are_animations_ongoing()` true
///   (`009213dd`); the app grid and the overview search re-shaped every label on every pointer move
///   because hover bumped `content_rev` (`c5336421`, `d396bd30`).
/// - **Not bumped when something did** → a stale texture that survives as long as the content does.
///   The calendar popover's background froze at its open-time height while the list kept growing,
///   because the height was not in the key (`128d112e`).
///
/// Deriving the key does not make either impossible, but it moves the mistake to where it can be
/// seen: the inputs are listed at the call site, immediately above the closure that reads them, so
/// adding one to the paint and adding one to the key are the same edit.
/// `docs/fork/widget-layer-design.md` §H1 prescribed this when `bake()` landed — "revision should
/// be **derived** … rather than hand-bumped, so it is correct on its own and size is pure
/// insurance".
///
/// Hashing, not equality, because `bake`'s cache key is a `u64`. A collision would serve a stale
/// texture, which at 64 bits over a handful of live variants is not a risk worth structuring
/// against — but it *is* the reason to prefer a signature tuple where a widget already has one
/// (`end_session`'s `Sig`), rather than converting it to this.
#[derive(Debug, Clone)]
pub struct Revision(std::collections::hash_map::DefaultHasher);

impl Default for Revision {
    fn default() -> Self {
        Self::new()
    }
}

impl Revision {
    pub fn new() -> Self {
        Self(std::collections::hash_map::DefaultHasher::new())
    }

    /// Fold in any hashable input. Order matters, which is what keeps `("a", "bc")` apart from
    /// `("ab", "c")`.
    #[must_use]
    pub fn of(mut self, value: impl std::hash::Hash) -> Self {
        std::hash::Hash::hash(&value, &mut self.0);
        self
    }

    /// Fold in a float — a size, a position, an animation progress. Floats are not `Hash`, and the
    /// two values that would otherwise misbehave are normalized: every NaN folds to one bit
    /// pattern (or a NaN input would miss the cache on **every** frame, which is the expensive
    /// failure this whole type exists to prevent), and `-0.0` folds to `0.0`, which is what the
    /// bake would draw anyway.
    #[must_use]
    pub fn px(self, value: f64) -> Self {
        let value = if value.is_nan() {
            f64::NAN
        } else {
            value + 0.0
        };
        self.of(value.to_bits())
    }

    /// Fold in a premultiplied or straight RGBA color.
    #[must_use]
    pub fn color(self, rgba: [f32; 4]) -> Self {
        rgba.iter().fold(self, |rev, c| rev.px(f64::from(*c)))
    }

    /// Fold in each item of an iterator, in order.
    #[must_use]
    pub fn each<T: std::hash::Hash>(self, items: impl IntoIterator<Item = T>) -> Self {
        items.into_iter().fold(self, Self::of)
    }

    /// The `revision` to hand [`bake`].
    pub fn done(&self) -> u64 {
        std::hash::Hasher::finish(&self.0)
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
#[track_caller]
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

    // Fold the style generation into every widget's revision, so a change to the base
    // font size re-bakes all of them. It cannot be left to the per-widget keys: a
    // fixed-size surface with text inside it (the panel bar) has an unchanged
    // `(scale, size, revision)` and would keep serving the old text.
    let revision = revision ^ crate::ui::style_generation();

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
#[track_caller]
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
#[track_caller]
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
#[track_caller]
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
#[track_caller]
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
#[track_caller]
pub fn bake_uncached_sized(
    renderer: &mut VulkanRenderer,
    phys: Size<i32, Physical>,
    paint: impl FnOnce(&mut VulkanFrame) -> anyhow::Result<()>,
) -> anyhow::Result<VkTexture> {
    // Every offscreen bake funnels through here, and each one is a render pass,
    // a submit and a fence wait. Counting and timing them here (rather than at
    // each caller) is what lets a slow frame say how much of itself was
    // re-rasterization.
    //
    // `time_bake` is `#[track_caller]`, and so is every helper in this file between
    // a widget and this line, so the site it records is the *widget's* — not
    // whichever of `bake`/`bake_content`/`bake_uncached` it came through. Dropping
    // the attribute from any one of them collapses every widget that routes through
    // it onto a single line in this file, which reads like one very busy widget.
    let _timed = crate::frame_log::time_bake();

    let (w, h) = (phys.w.max(1), phys.h.max(1));
    let mut target =
        renderer.create_buffer(NATIVE_FOURCC, Size::<i32, BufferCoord>::from((w, h)))?;
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
#[track_caller]
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

/// GNOME's `.toggle-switch` (`_switches.scss:6-52`) — the pill-and-handle control a
/// `PopupSwitchMenuItem` puts at the right end of a menu row (`popupMenu.js:501-524`).
///
/// A geometry-and-paint primitive, not a stateful widget: the owner holds the on/off
/// state (it lives in a settings model) and the rect (it comes from the owner's row
/// layout), and calls [`Painter::toggle_switch`] so every switch looks the same
/// wherever one appears.
///
/// No slide animation yet: the only switch so far lives in the accessibility menu,
/// which closes on click (`popupMenu.js:539-550`), so the travel is never seen.
pub struct Switch;

impl Switch {
    /// `$switch_width` (`_switches.scss:3`).
    pub const WIDTH: f64 = 46.;
    /// `$switch_handle_size` (`_switches.scss:4`).
    pub const HANDLE: f64 = 20.;
    /// `.handle { margin: 3px }` (`_switches.scss:31`) — so the track is
    /// `HANDLE + 2 * MARGIN` = 26px tall.
    pub const MARGIN: f64 = 3.;
    /// Track height: the handle plus its margin on both sides.
    pub const HEIGHT: f64 = Self::HANDLE + 2. * Self::MARGIN;

    /// The control's logical size.
    pub fn size() -> Size<f64, Logical> {
        Size::from((Self::WIDTH, Self::HEIGHT))
    }
}

/// Off-state track fill: `transparentize(white, .85)` (`_switches.scss:19`).
const SWITCH_OFF_BG: Rgba = [1., 1., 1., 0.15];
/// Off-state track fill while hovered: `transparentize(white, .8)` (`_switches.scss:22`).
const SWITCH_OFF_BG_HOVER: Rgba = [1., 1., 1., 0.2];
/// Handle fill when off: `mix(white, $bg_color, 80%)` over `$bg_color` #36363a
/// (`_switches.scss:35`).
const SWITCH_HANDLE_OFF: Rgba = [0.96, 0.96, 0.96, 1.];
/// Handle fill when on: plain white (`_switches.scss:50`).
const SWITCH_HANDLE_ON: Rgba = [1., 1., 1., 1.];
/// `box-shadow: 0 2px 4px transparentize(black, .8)` under the handle
/// (`_switches.scss:36`), approximated as a single offset dark disc behind it.
const SWITCH_HANDLE_SHADOW: Rgba = [0., 0., 0., 0.2];

/// GNOME's shared `BarLevel` (`js/ui/barLevel.js`) — the rounded progress bar the
/// OSD shows under its icon, and the same drawing the quick-settings sliders use.
///
/// A geometry-and-paint primitive like [`Switch`]: the owner holds the value and the
/// rect and calls [`Painter::bar_level`]. The metrics below are the `.level` node
/// inside `.osd-window` (`_osd.scss:22-34`); the colors are per-call
/// ([`BarLevelStyle`]) because each host node re-declares them.
pub struct BarLevel;

impl BarLevel {
    /// `-barlevel-height: $osd_levelbar_height` (`_osd.scss:3,26`).
    pub const HEIGHT: f64 = 6.;
    /// `min-width: 160px` (`_osd.scss:25`).
    pub const MIN_WIDTH: f64 = 160.;
    /// `-barlevel-overdrive-separator-width: $base_padding * 0.5` (`_osd.scss:31`).
    pub const OVERDRIVE_SEPARATOR: f64 = 3.;
}

/// The four theme colors a [`BarLevel`] draws with (`barLevel.js:110-113,155`).
#[derive(Debug, Clone, Copy)]
pub struct BarLevelStyle {
    /// `-barlevel-background-color` — the unfilled track.
    pub track: Rgba,
    /// `-barlevel-active-background-color` — the filled part below `overdrive_start`.
    pub fill: Rgba,
    /// `-barlevel-overdrive-color` — the filled part above it.
    pub overdrive: Rgba,
    /// The node's foreground color, which is what the separator is drawn in while
    /// the value has not reached overdrive (`barLevel.js:215-218`).
    pub separator: Rgba,
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
/// An edge of a box — `St.Side` (`st-types.h`), in its order, so a port reads like the JS it came
/// from. Today only [`Painter::triangle`] takes one, naming the edge an arrow's apex points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Side {
    Top = 0,
    Right = 1,
    Bottom = 2,
    Left = 3,
}

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

    /// Fill the isoceles triangle inscribed in `rect` (logical) with `color`: its base spans one
    /// edge, its apex the midpoint of the opposite one, `side` naming the edge it points at.
    ///
    /// GNOME's `SwitcherPopup.drawArrow` (`js/ui/switcherPopup.js:661-704`) — the app switcher's
    /// multi-window chevron and the switcher list's scroll arrows. That function strokes the path
    /// in `border-color` and then fills it in `color`; `.switcher-arrow`
    /// (`_switcher-popup.scss:62-70`) sets both to the same value in both states, so one fill is
    /// exact. If a caller ever needs the two to differ, this grows a stroke arm rather than the
    /// caller painting a second triangle on top.
    pub fn triangle(
        &mut self,
        rect: Rectangle<f64, Logical>,
        side: Side,
        color: Rgba,
    ) -> anyhow::Result<()> {
        self.frame
            .render_triangle(color, side as u8, self.rect_px(rect), &[self.full])?;
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

    /// [`fill_rounded`](Self::fill_rounded) with a horizontal alpha ramp: full `color` at
    /// `from`, transparent at `to`, both fractions of `rect`'s width (0 = its left edge) and in
    /// either order. GNOME's `background-gradient-direction: horizontal` where the two stops
    /// share an RGB and differ only in alpha, which is every gradient in the theme so far.
    ///
    /// Rounding is all four corners, as everywhere else. GNOME's per-corner `border-radius`
    /// (the page hints round only their inner pair) is expressed by letting the rect run past
    /// the bake buffer on the side that should stay square — the corners fall outside and are
    /// clipped, which is a real rounded rect rather than a painted-on curve.
    pub fn fill_rounded_faded(
        &mut self,
        rect: Rectangle<f64, Logical>,
        radius: f64,
        color: Rgba,
        from: f64,
        to: f64,
    ) -> anyhow::Result<()> {
        let r = (radius * self.scale) as f32;
        self.frame.render_rounded_rect_faded(
            color,
            r,
            self.rect_px(rect),
            &[self.full],
            (from as f32, to as f32),
        )?;
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
        label: &[ShapedText],
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
        // Label centered under the icon. A second line goes below the tile box, so the
        // clip is the tile widened downward by the lines past the first — clipping to
        // the box itself would cut a two-line caption in half.
        let lx = rel.loc.x + rel.size.w / 2.;
        let ly = rel.loc.y + metrics.pad + metrics.icon_px + metrics.label_gap;
        let extra = metrics.label_h * (label.len().max(1) as f64 - 1.);
        let clip = Rectangle::new(rel.loc, Size::from((rel.size.w, rel.size.h + extra)));
        for (i, line) in label.iter().enumerate() {
            self.text_band(
                line,
                lx,
                HAlign::Center,
                ly + i as f64 * metrics.label_h,
                metrics.label_h,
                text_color,
                clip,
            )?;
        }
        Ok(())
    }

    /// Draw a tile caption — one band of `line_h` per line, each centered in a box
    /// `w` wide starting at the bake's origin. The lines come from
    /// [`tile_label_lines`]; a collapsed caption is one of them, an expanded one is
    /// several. Meant for a bake sized exactly `w × lines.len()*line_h`, which is why
    /// it anchors at the origin rather than taking a tile rect.
    pub fn caption(
        &mut self,
        lines: &[ShapedText],
        w: f64,
        line_h: f64,
        color: Rgba,
    ) -> anyhow::Result<()> {
        for (i, line) in lines.iter().enumerate() {
            let top = i as f64 * line_h;
            let band = Rectangle::new(Point::from((0., top)), Size::from((w, line_h)));
            self.text_band(line, w / 2., HAlign::Center, top, line_h, color, band)?;
        }
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

    /// Draw a [`Switch`] filling `rect` (use [`Switch::size`] for it): the fully-rounded
    /// track, then the handle at whichever end `on` selects (`_switches.scss:6-52`).
    ///
    /// `accent` is the system accent — the on-state track fill (`background:
    /// -st-accent-color`, `_switches.scss:41`). `hovered` only lightens the *off* track,
    /// which is the direction the SCSS gives it (`:15-23`); the on state's hover is a
    /// 5% accent lighten we skip, since our switch has no hover state of its own yet
    /// (the whole row is the hover target).
    pub fn toggle_switch(
        &mut self,
        rect: Rectangle<f64, Logical>,
        on: bool,
        hovered: bool,
        accent: Rgba,
    ) -> anyhow::Result<()> {
        // `border-radius: $forced_circular_radius` — a pill, so half the height.
        let track_radius = rect.size.h / 2.;
        let track = if on {
            accent
        } else if hovered {
            SWITCH_OFF_BG_HOVER
        } else {
            SWITCH_OFF_BG
        };
        self.fill_rounded(rect, track_radius, track)?;

        // The handle sits `MARGIN` from the track's near end, sized square.
        let size = rect.size.h - 2. * Switch::MARGIN;
        let x = if on {
            rect.loc.x + rect.size.w - Switch::MARGIN - size
        } else {
            rect.loc.x + Switch::MARGIN
        };
        let handle = Rectangle::new(
            Point::from((x, rect.loc.y + Switch::MARGIN)),
            Size::from((size, size)),
        );
        // `box-shadow: 0 2px 4px …` — the same disc, nudged down, behind the handle.
        let shadow = Rectangle::new(handle.loc + Point::from((0., 2.)), handle.size);
        self.fill_rounded(shadow, size / 2., SWITCH_HANDLE_SHADOW)?;
        let fill = if on {
            SWITCH_HANDLE_ON
        } else {
            SWITCH_HANDLE_OFF
        };
        self.fill_rounded(handle, size / 2., fill)?;
        Ok(())
    }

    /// Draw a [`BarLevel`] filling `rect`: the track, the fill up to `value`, and —
    /// when `overdrive_start < max` — the overdrive segment past it, separated by a
    /// [`BarLevel::OVERDRIVE_SEPARATOR`]-wide gap (`barLevel.js:117-220`).
    ///
    /// `value` is clamped to `0..=max` and `max` to at least 1, exactly like the
    /// property setters (`barLevel.js:57-80`). The cap radius is
    /// `min(width, height) / 2` (`:122`), and — this is the part that is easy to get
    /// wrong — the fill's end cap is a **full circle centered on** the progress
    /// position (`:194-210`), so the fill actually reaches `end_x + radius`, which is
    /// what makes a full bar reach the right edge.
    ///
    /// We differ from the cairo original in one harmless way: GNOME paints the track
    /// only over the *unfilled* remainder, we paint the whole pill and cover it. That
    /// is identical for opaque fills (every current caller) and it is also what makes
    /// the overdrive gap correct for free — the gap shows the track through, which is
    /// precisely the color GNOME paints the separator with once the value is in
    /// overdrive (`:215-218`), so that branch draws nothing.
    pub fn bar_level(
        &mut self,
        rect: Rectangle<f64, Logical>,
        value: f64,
        max: f64,
        overdrive_start: f64,
        style: BarLevelStyle,
    ) -> anyhow::Result<()> {
        let max = max.max(1.);
        let value = value.clamp(0., max);
        let overdrive_start = overdrive_start.clamp(1., max);
        let radius = rect.size.w.min(rect.size.h) / 2.;
        // The span the progress position travels across, inset by a cap at each end.
        let travel = (rect.size.w - 2. * radius).max(0.);

        self.fill_rounded(rect, radius, style.track)?;

        let seg = |from: f64, to: f64| {
            Rectangle::new(
                Point::from((rect.loc.x + from, rect.loc.y)),
                Size::from((to - from, rect.size.h)),
            )
        };
        let end_x = radius + travel * (value / max);
        let sep_x = radius + travel * (overdrive_start / max);
        let overdrive_active = overdrive_start < max;
        let half_sep = if overdrive_active {
            BarLevel::OVERDRIVE_SEPARATOR / 2.
        } else {
            0.
        };

        if value > 0. {
            if !overdrive_active || value <= overdrive_start {
                self.fill_rounded(seg(0., end_x + radius), radius, style.fill)?;
            } else {
                // Only the *outer* ends of these two segments are round: the edges
                // facing the separator are straight `lineTo`s in cairo
                // (`barLevel.js:163-172,177-190`). Ours come from `fill_rounded`,
                // which rounds all four corners, so each inner edge is squared off
                // again by a plain rect over its corner band — at 6px tall the
                // corners are full semicircles, and leaving them would make the 3px
                // separator read as a wide notch exactly when overdrive is showing.
                let fill_end = sep_x - half_sep;
                let over_start = sep_x + half_sep;
                let over_end = end_x + radius;
                self.fill_rounded(seg(0., fill_end), radius, style.fill)?;
                self.fill_rounded(seg((fill_end - radius).max(0.), fill_end), 0., style.fill)?;
                self.fill_rounded(seg(over_start, over_end), radius, style.overdrive)?;
                self.fill_rounded(
                    seg(over_start, (over_start + radius).min(over_end)),
                    0.,
                    style.overdrive,
                )?;
            }
        }

        // Below overdrive the separator is a solid foreground tick; above it, the gap
        // left between the two fills already shows the track (see the doc comment).
        if overdrive_active && value <= overdrive_start {
            self.fill_rounded(seg(sep_x - half_sep, sep_x + half_sep), 0., style.separator)?;
        }
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
    use smithay::backend::allocator::Fourcc;
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

    use super::{
        bake_uncached_sized, tile_label_lines, Painter, Revision, TileMetrics,
        TILE_LABEL_EXPAND_LINES, TILE_LABEL_LINES,
    };
    use crate::render_helpers::vulkan::VulkanRenderer;

    /// A tile is `.overview-tile` padding around a `Shell.SquareBin`, so its **width**
    /// follows the *content height* — icon + spacing + one caption line
    /// (`shell-square-bin.c:14-30`, `iconGrid.js:62`). Deriving it from the icon alone
    /// made it 120 wide against GNOME's 145. Pinned in numbers because the whole grid
    /// and the search results card size off it.
    ///
    /// 145, not 144: the caption's line box at the base 11pt is 19 (`ceil(ascent) + ceil(descent)`
    /// — see [`crate::ui::line_height_px`]), and `label_h` was pinned at 18.
    ///
    /// A resting caption past the first line hangs *below* the tile — the tile stays
    /// square (reserving the second line in the box would cost a rung of the icon ladder
    /// on the small canvases the second line exists for), so the chrome drawn around a
    /// caption grows by the overhang instead.
    #[test]
    fn an_overview_tile_is_a_square_around_its_caption_box() {
        let m = TileMetrics::overview();
        // The caption band is the shared line box, not a number of its own — the tile is a
        // SquareBin, so a private copy here would resize the whole app grid behind everyone.
        assert_eq!(
            m.label_h,
            crate::ui::line_height_px(crate::ui::BASE_FONT_PT),
            "the caption band must be the shared line box at the caption's own size"
        );
        assert_eq!(m.label_w(), m.icon_px + m.label_gap + m.label_h);
        assert_eq!(m.label_w(), 121.);
        assert_eq!(m.size(), Size::from((145., 145.)));
        assert_eq!(m.size().w, m.label_w() + 2. * m.pad);

        // One line fits the box exactly; each further line hangs below it.
        assert_eq!(m.caption_overhang(1), 0.);
        assert_eq!(m.caption_overhang(2), m.label_h);
        let tile = Rectangle::new(Point::from((0., 0.)), m.size());
        assert_eq!(
            m.label_top(tile) + m.label_h,
            tile.size.h - m.pad,
            "one line ends at the tile's bottom padding"
        );
    }

    /// A folder tile's icon is a homogeneous 2×2 over the same icon box an app icon
    /// fills (`createFolderIcon`, `appDisplay.js:2138-2162`): four half-box cells,
    /// each with one member icon at 0.4× the box centered in it. The centering is the
    /// part worth pinning — it is what leaves the cross-shaped gap that reads as a
    /// folder, and cell-filling instead would make the four icons touch.
    #[test]
    fn a_folder_tile_composes_four_sub_icons_centered_in_a_two_by_two() {
        let m = TileMetrics::overview();
        let tile = Rectangle::new(Point::from((0., 0.)), m.size());
        let sub = m.folder_subicon_px();
        assert_eq!(sub, 38., "floor(0.4 * 96)");

        let centers: Vec<Point<f64, Logical>> =
            (0..4).map(|i| m.folder_subicon_center(tile, i)).collect();
        let icon = m.icon_center(tile);
        // Left-to-right then top-to-bottom, a quarter box out from the icon center.
        let q = m.icon_px / 4.;
        assert_eq!(centers[0], Point::from((icon.x - q, icon.y - q)));
        assert_eq!(centers[1], Point::from((icon.x + q, icon.y - q)));
        assert_eq!(centers[2], Point::from((icon.x - q, icon.y + q)));
        assert_eq!(centers[3], Point::from((icon.x + q, icon.y + q)));

        // Centered in their half-box cells, so the composition sits inside the icon
        // box with a gap down the middle rather than filling it edge to edge.
        let left = centers[0].x - sub / 2.;
        let right = centers[1].x + sub / 2.;
        assert!(left > icon.x - m.icon_px / 2., "inset from the box: {left}");
        assert!(
            right < icon.x + m.icon_px / 2.,
            "inset from the box: {right}"
        );
        assert!(
            centers[1].x - sub / 2. > centers[0].x + sub / 2.,
            "the two columns do not touch"
        );
    }

    /// A **resting** caption never splits a word: on a narrow tile (a small canvas shrinks
    /// the icon, and the caption box with it) "Graphics" must read "Graphic…", not
    /// "Graphi/cs". Expanding is the opposite — it exists to show the whole name, so there
    /// a word wider than the tile breaks across lines (Pango `WORD_CHAR`) rather than
    /// losing characters. Live report, 2026-07-28, on a 1024x665 canvas.
    #[test]
    fn a_resting_caption_ellipsizes_a_long_word_instead_of_splitting_it() {
        let pt = crate::ui::BASE_FONT_PT;
        // The caption box of a tile whose icon has stepped well down the ladder.
        let narrow = TileMetrics {
            icon_px: 32.,
            ..TileMetrics::overview()
        }
        .label_w();

        let resting = tile_label_lines("Graphics", pt, narrow, TILE_LABEL_LINES, false);
        assert_eq!(resting.len(), 1, "one word stays on one line: {resting:?}");
        assert!(
            resting[0].ends_with('…') && !resting[0].contains(' '),
            "…ellipsized, not split: {resting:?}"
        );

        // Two words still wrap at the space — that is the whole point of the second line.
        let two = tile_label_lines("Image Editors", pt, narrow, TILE_LABEL_LINES, false);
        assert_eq!(two.len(), 2, "a space is a break point: {two:?}");
        assert!(!two[0].ends_with('…'), "and needs no ellipsis: {two:?}");

        // A first line that had to be cut is the LAST line: "Chrome Web Store" in a box
        // too narrow for "Chrome" reads "Chr…", not "Chr…/We…".
        let cut_first = tile_label_lines("Chrome Web Store", pt, 40., TILE_LABEL_LINES, false);
        assert_eq!(
            cut_first.len(),
            1,
            "nothing follows an ellipsized line: {cut_first:?}"
        );
        assert!(cut_first[0].ends_with('…'));

        // Expanded may split it, because there is nowhere else for the characters to go.
        let expanded = tile_label_lines("Graphics", pt, narrow, TILE_LABEL_EXPAND_LINES, true);
        assert!(
            expanded.len() > 1 && !expanded.concat().contains('…'),
            "expanded breaks the word rather than cutting it: {expanded:?}"
        );
    }

    /// A name that does not fit is wrapped to [`TILE_LABEL_LINES`] and ellipsized past
    /// them at rest, and wrapped whole (no ellipsis) expanded — `_updateMultiline`,
    /// `appDisplay.js:1891-1924`, with the resting line count our own divergence.
    /// Before this, a long name was hard-clipped mid-glyph by the label band.
    #[test]
    fn a_long_tile_caption_ellipsizes_collapsed_and_wraps_expanded() {
        let name = "Passwords and Keys";
        let w = TileMetrics::overview().label_w();
        let pt = crate::ui::BASE_FONT_PT;

        let collapsed = tile_label_lines(name, pt, w, TILE_LABEL_LINES, false);
        assert_eq!(collapsed.len(), 2, "at rest a long name wraps to two lines");
        assert!(
            !collapsed.concat().contains('…'),
            "which this name fits in whole: {collapsed:?}"
        );
        // Longer than two lines still ends in an ellipsis rather than losing text.
        let long = tile_label_lines(
            "Passwords and Keys and Certificates",
            pt,
            w,
            TILE_LABEL_LINES,
            false,
        );
        assert_eq!(long.len(), 2);
        assert!(long[1].ends_with('…'), "cut past the last line: {long:?}");

        // One line is still one line — what a search result asks for.
        let one = tile_label_lines(name, pt, w, 1, false);
        assert_eq!(one.len(), 1);
        assert!(one[0].ends_with('…'));

        let expanded = tile_label_lines(name, pt, w, TILE_LABEL_EXPAND_LINES, true);
        assert!(expanded.len() > 1, "expanded wraps: {expanded:?}");
        assert!(!expanded.concat().contains('…'), "and drops the ellipsis");
        assert_eq!(
            expanded.join(" ").split_whitespace().collect::<Vec<_>>(),
            name.split_whitespace().collect::<Vec<_>>(),
            "the whole name is readable"
        );

        // A name that fits is untouched in both states — it stays in the page bake.
        assert_eq!(
            tile_label_lines("Files", pt, w, TILE_LABEL_LINES, false),
            vec!["Files"]
        );
        assert_eq!(
            tile_label_lines("Files", pt, w, TILE_LABEL_EXPAND_LINES, true),
            vec!["Files"]
        );
    }

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

    /// The horizontal alpha ramp on the rounded-rect material: full colour at one end,
    /// nothing at the other, linear between. Also pins the "run the rect past the buffer
    /// to keep a corner square" idiom the page hints use — with the rect extending 40px
    /// beyond the right edge, the right corners are clipped away and that edge is full
    /// height, while the left ones are visibly cut by the radius.
    #[test]
    fn a_faded_rounded_rect_ramps_across_its_width() {
        let mut vk = match VulkanRenderer::new() {
            Ok(vk) => vk,
            Err(e) => {
                eprintln!("skipping a_faded_rounded_rect_ramps_across_its_width: no device ({e})");
                return;
            }
        };

        let scale = 1.0;
        let size = Size::<i32, Physical>::from((100, 100));
        let mut tex = bake_uncached_sized(&mut vk, size, |frame| {
            let mut p = Painter::new(frame, scale, size);
            p.clear([0., 0., 0., 0.])?;
            // Opaque white, rounded 20, running 40px past the right edge; brightest at the
            // left edge (u=0) and gone by the right edge of the *drawn* rect (u=1 of 140).
            let rect =
                Rectangle::<f64, Logical>::new(Point::from((0., 0.)), Size::from((140., 100.)));
            p.fill_rounded_faded(rect, 20., [1., 1., 1., 1.], 0., 1.)?;
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
        let alpha = |x: i32, y: i32| pixels[((y * 100 + x) * 4 + 3) as usize] as i32;

        let (left, mid, right) = (alpha(2, 50), alpha(50, 50), alpha(98, 50));
        assert!(left > 220, "full colour at the ramp's start: {left}");
        assert!(
            (left - right) > 40 && mid > right && mid < left,
            "the ramp must fall monotonically across the width: {left} {mid} {right}"
        );
        // Left corners cut by the radius, right edge square (its corners fell outside).
        assert_eq!(alpha(1, 1), 0, "the left corner is rounded away");
        assert!(
            alpha(98, 1) > 0,
            "the right edge runs off the buffer square"
        );
    }

    /// The bake-attribution chain: a bake must be recorded against the *widget's*
    /// line, not against whichever helper in this file it travelled through.
    ///
    /// This is the load-bearing half of `frame_log`'s bake reporting, and it
    /// degrades silently: drop `#[track_caller]` from one helper and every widget
    /// routing through it collapses onto a single line in `widget.rs`, which reads
    /// like one very busy widget rather than a broken instrument. Nothing else
    /// notices — the counts and the timings stay right.
    ///
    /// Each helper here is entered from a distinct line of *this* test, so a
    /// collapsed chain shows up as a site in `widget.rs` instead.
    #[test]
    fn bake_sites_name_the_widget_not_the_helper() {
        let mut vk = match VulkanRenderer::new() {
            Ok(vk) => vk,
            Err(e) => {
                eprintln!("skipping bake_sites_name_the_widget_not_the_helper: no device ({e})");
                return;
            }
        };

        let size = Size::<i32, Physical>::from((8, 8));
        let logical = Size::<f64, Logical>::from((8., 8.));
        let paint_px = |_: &mut crate::render_helpers::vulkan::VulkanFrame| Ok(());

        let _ = crate::frame_log::take_bake_sites();

        let sized_line = line!() + 1;
        bake_uncached_sized(&mut vk, size, paint_px).expect("bake_uncached_sized");
        let uncached_line = line!() + 1;
        super::bake_uncached(&mut vk, 1.0, logical, |_, _| Ok(())).expect("bake_uncached");
        let cached_line = line!() + 2;
        let mut cache = super::BakeCache::default();
        super::bake(
            &mut vk,
            &mut cache,
            1.0,
            logical,
            0,
            |_| Ok(()),
            |_, _, _| Ok(()),
        )
        .expect("bake");

        let sites = crate::frame_log::take_bake_sites();
        let here = file!();
        for (what, line) in [
            ("bake_uncached_sized", sized_line),
            ("bake_uncached", uncached_line),
            ("bake", cached_line),
        ] {
            assert!(
                sites.iter().any(|s| s.file == here && s.line == line),
                "{what} was attributed to {:?}, not to its caller {here}:{line} — \
                 a #[track_caller] is missing from the chain in widget.rs",
                sites
                    .iter()
                    .map(|s| format!("{}:{}", s.file, s.line))
                    .collect::<Vec<_>>(),
            );
        }
    }

    /// Every input the paint closure reads has to move the key, or the widget serves a stale
    /// texture for as long as its content lives — the `128d112e` calendar freeze.
    #[test]
    fn a_changed_input_changes_the_revision() {
        let base = Revision::new()
            .of(3usize)
            .of("title")
            .px(120.)
            .color(ACCENT);
        assert_ne!(
            base.done(),
            Revision::new()
                .of(4usize)
                .of("title")
                .px(120.)
                .color(ACCENT)
                .done(),
            "a count change did not invalidate"
        );
        assert_ne!(
            base.done(),
            Revision::new()
                .of(3usize)
                .of("other")
                .px(120.)
                .color(ACCENT)
                .done(),
            "a text change did not invalidate"
        );
        assert_ne!(
            base.done(),
            Revision::new()
                .of(3usize)
                .of("title")
                .px(121.)
                .color(ACCENT)
                .done(),
            "a size change did not invalidate"
        );
        assert_ne!(
            base.done(),
            Revision::new()
                .of(3usize)
                .of("title")
                .px(120.)
                .color(RED)
                .done(),
            "a color change did not invalidate"
        );
        assert_eq!(
            base.done(),
            Revision::new()
                .of(3usize)
                .of("title")
                .px(120.)
                .color(ACCENT)
                .done(),
            "the same inputs must hit the cache"
        );
    }

    /// Order is part of the key, so two fields cannot swap their contributions and cancel out.
    #[test]
    fn the_order_of_the_inputs_is_part_of_the_revision() {
        assert_ne!(
            Revision::new().of("ab").of("c").done(),
            Revision::new().of("a").of("bc").done(),
            "adjacent inputs ran together, so a boundary shift is invisible"
        );
    }

    /// A NaN that hashed as itself would miss the cache on **every** frame — a full GPU round trip
    /// per frame, which is the exact failure this type exists to prevent, arriving silently.
    #[test]
    fn a_nan_input_still_hits_its_own_cache_entry() {
        assert_eq!(
            Revision::new().px(f64::NAN).done(),
            Revision::new().px(f64::NAN).done(),
            "a NaN input re-bakes forever"
        );
        assert_eq!(
            Revision::new().px(-0.0).done(),
            Revision::new().px(0.0).done(),
            "-0.0 and 0.0 paint the same and must share an entry"
        );
        assert_ne!(
            Revision::new().px(f64::NAN).done(),
            Revision::new().px(0.0).done()
        );
    }

    /// `each` folds a sequence, and a reordering of that sequence is a different bake.
    #[test]
    fn a_reordered_sequence_is_a_different_revision() {
        assert_ne!(
            Revision::new().each(["one", "two"]).done(),
            Revision::new().each(["two", "one"]).done(),
            "reordering the entries did not invalidate — a list that only reorders serves stale"
        );
        assert_eq!(
            Revision::new().each(["one", "two"]).done(),
            Revision::new().of("one").of("two").done(),
            "`each` must be the same as folding the items by hand"
        );
    }

    const ACCENT: [f32; 4] = [0.21, 0.52, 0.89, 1.];
    const RED: [f32; 4] = [0.89, 0.21, 0.21, 1.];
}
