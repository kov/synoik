// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The window picker's per-preview chrome (`js/ui/windowPreview.js`).
//!
//! gnome-shell's `WindowPreview` hangs three actors off each preview: the app
//! icon (always visible in the window picker), the title caption and the close
//! button. The latter two are the *overlay*: hidden until the pointer enters the
//! preview, then faded in over `WINDOW_OVERLAY_FADE_TIME` alongside the
//! preview's scale-up (`showOverlay`, `windowPreview.js:310-352`).
//!
//! **Scope.** All three. The icon and caption arrived with the app-lifecycle port
//! (`docs/fork/app-lifecycle-port.md` §2.5, L3), which is what gave the picker a
//! window→app resolution to hang them on.
//!
//! The two are gated differently, and that is the whole of their behavior:
//! the **icon** is always on in the window picker and its *scale* ramps with the
//! overview axis — `1 - |WINDOW_PICKER - currentState|`, so it grows out of nothing
//! on open and shrinks away into the app grid (`_updateIconScale`,
//! `windowPreview.js:238-252`); the **caption** is part of the hover overlay and
//! rides the same alpha as the close button.
//!
//! **Divergence.** GNOME shows the button only when `_windowCanClose()`
//! (`windowPreview.js:322,325`); we show it for every preview. Attached modal
//! dialogs — the case that makes `can_close` false in practice — are not modelled
//! in the picker yet.
//!
//! The button's geometry is in *screen* pixels, like the preview's hover growth:
//! gnome-shell allocates its previews in stage coordinates, so a 32px button
//! stays 32px however far the workspace row is zoomed out.

use std::cell::RefCell;
use std::collections::HashMap;

use smithay::backend::renderer::element::utils::RescaleRenderElement;
use smithay::backend::renderer::element::Kind;
use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::app_system::AppIconRef;
use crate::render_helpers::icon::{AppIconCache, IconCache};
use crate::render_helpers::texture::TextureRenderElement;
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::synoik_render_elements;
use crate::ui::widget::{
    self, style, Align, BakeCache, Painter, Rgba, SharedAppIconUploads, TextShaper, TextStyle,
};

/// The button's box, logical px — `$large_icon_size` (`_window-picker.scss:35-36`).
pub const CLOSE_SIZE: f64 = 32.;

/// The glyph inside it — `& StIcon { icon-size: $medium_icon_size }`
/// (`_window-picker.scss:44`).
const CLOSE_ICON_PX: f64 = 24.;

/// `$window_close_button_color` = `transparentize(lighten($system_bg_color, 7%),
/// .02)` (`_window-picker.scss:2`), with `$system_bg_color` =
/// `lighten($system_base_color, 5%)` (`_colors.scss:47`) — `#3f3f46` at 98%.
pub const CLOSE_BG: Rgba = [0.2471, 0.2471, 0.2745, 0.98];

/// `.window-close:hover` — the same color lightened 7% (`_window-picker.scss:47`).
/// Note this widget lightens on hover, but the direction is read, never assumed.
pub const CLOSE_BG_HOVER: Rgba = [0.3137, 0.3137, 0.349, 0.98];

/// The close button of a preview drawn at `preview`: a `CLOSE_SIZE` box centered
/// on the preview's top-right corner. gnome-shell aligns the button's own center
/// to the corner with a pair of `AlignConstraint`s whose pivots are `(0.5, -1)` on
/// X (factor 1 = the right edge) and `(-1, 0.5)` on Y (factor 0 = the top edge)
/// (`windowPreview.js:203-218`), so it half-overhangs both edges.
///
/// DIVERGENCE: `Meta.prefs_get_button_layout()` can put the window buttons on the
/// left, which moves this to the *top-left* corner (`_closeButtonSide`,
/// `windowPreview.js:193-197`). We always draw it on the right.
pub fn close_rect(preview: Rectangle<f64, Logical>) -> Rectangle<f64, Logical> {
    let center = Point::from((preview.loc.x + preview.size.w, preview.loc.y));
    Rectangle::new(
        center - Point::from((CLOSE_SIZE / 2., CLOSE_SIZE / 2.)),
        Size::from((CLOSE_SIZE, CLOSE_SIZE)),
    )
}

/// How far outside its slot a preview still counts as hovered, logical px.
///
/// **Divergence.** gnome-shell needs no such slop: its overlay actors are *children* of the
/// preview, so a pointer on the half of the close button that overhangs the preview is still
/// inside the preview's own reactive box. Ours are separate rects hit-tested against the slot,
/// so without this the pointer leaving the slot drops the hover — and the fade takes the
/// button away from under the pointer that was aiming at it.
const HOVER_SLOP: f64 = 8.;

/// The region that counts as "on" this preview for hover purposes: its slot plus a little
/// slop, and — whatever the slop — the whole of [`close_rect`], so the button can never fade
/// out from under a pointer that is on it.
pub fn hover_rect(preview: Rectangle<f64, Logical>) -> Rectangle<f64, Logical> {
    let grown = Rectangle::new(
        preview.loc - Point::from((HOVER_SLOP, HOVER_SLOP)),
        preview.size + Size::from((HOVER_SLOP * 2., HOVER_SLOP * 2.)),
    );
    // `Rectangle::merge` is the bounding box of the two, which is what we want: the button
    // sits on a corner, so the union is the slot grown to swallow it.
    grown.merge(close_rect(preview))
}

/// `ICON_SIZE` (`windowPreview.js:24`) — the app icon's box, logical px.
pub const ICON_SIZE: f64 = 64.;
/// `ICON_OVERLAP` (`:25`): the fraction of the icon that sits *inside* the
/// preview. The icon is bottom-anchored with this as its Y pivot, so the
/// remaining 30% hangs below the preview's bottom edge.
pub const ICON_OVERLAP: f64 = 0.7;
/// `ICON_TITLE_SPACING` (`:27`) — between the icon's bottom and the caption.
pub const ICON_TITLE_SPACING: f64 = 6.;

/// The app icon's centre for a preview drawn at `preview`: X-centred over it, and
/// Y placed by the `AlignConstraint` at factor 1 with pivot `y = ICON_OVERLAP`
/// (`windowPreview.js:151-156`) — the point `ICON_OVERLAP` of the way down the
/// icon lands on the preview's bottom edge.
pub fn icon_center(preview: Rectangle<f64, Logical>) -> Point<f64, Logical> {
    let bottom = preview.loc.y + preview.size.h;
    Point::from((
        preview.loc.x + preview.size.w / 2.,
        bottom - ICON_SIZE * (ICON_OVERLAP - 0.5),
    ))
}

/// The caption's text size. **Divergence (approved 2026-11-11):** gnome-shell's
/// `.window-caption` is a `%tooltip` pill at the stage's 11pt hung *below* the icon
/// (`windowPreview.js:159-186`, `_window-picker.scss:24-26`), which puts a bottom-row caption
/// off the screen once the picker is crowded. Ours sits *inside* the preview, just above the
/// icon, on an opaque OSD plate with a drop shadow and larger text, so it reads as chrome laid
/// over the window rather than as part of its content.
pub const CAPTION_PT: f64 = 13.;
/// The plate's padding around the label.
const CAPTION_PAD_V: f64 = 8.;
const CAPTION_PAD_H: f64 = 16.;
/// The least the plate keeps clear of the preview's sides; a longer title is ellipsized.
const CAPTION_MARGIN: f64 = 12.;
/// Heavier than the switcher's `0 8px 8px 0 $shadow_color`: the plate floats over arbitrary
/// window content, not over the wallpaper.
const CAPTION_SHADOW: widget::DropShadowSpec = widget::DropShadowSpec {
    blur: 12.,
    offset: (0., 4.),
    spread: 0.,
    color: [0., 0., 0., 0.5],
};

/// The caption as drawn on a preview of `preview`: `caption`, ellipsized to fit inside it.
pub fn caption_text(caption: &str, preview: Rectangle<f64, Logical>) -> String {
    let max_w = preview.size.w - 2. * (CAPTION_MARGIN + CAPTION_PAD_H);
    widget::ellipsized_line(caption, CAPTION_PT, max_w.max(0.))
}

/// The caption plate's box for an already-fitted `text` (see [`caption_text`]).
pub fn caption_size(text: &str) -> Size<f64, Logical> {
    let px = crate::ui::pt_to_px(CAPTION_PT);
    let w = synoik_vk::text::measure_line_width_weighted(text, px as f32, false);
    Size::from((w + 2. * CAPTION_PAD_H, px.ceil() + 2. * CAPTION_PAD_V))
}

/// The caption plate's box on a preview of `preview`: X-centred, its bottom
/// `ICON_TITLE_SPACING` above the top of the app icon.
pub fn caption_rect(
    preview: Rectangle<f64, Logical>,
    size: Size<f64, Logical>,
) -> Rectangle<f64, Logical> {
    let icon_top = preview.loc.y + preview.size.h - ICON_SIZE * ICON_OVERLAP;
    Rectangle::new(
        Point::from((
            preview.loc.x + (preview.size.w - size.w) / 2.,
            icon_top - ICON_TITLE_SPACING - size.h,
        )),
        size,
    )
}

/// One preview's chrome, as the renderer needs it: where the preview draws, how
/// far its hover overlay has faded in, whether the pointer is on the close button,
/// and the app identity behind it.
#[derive(Debug, Clone)]
pub struct PreviewOverlay {
    pub preview: Rectangle<f64, Logical>,
    pub alpha: f32,
    pub hovered: bool,
    /// The window's app icon, resolved through the app model. `None` when the window resolves
    /// to no installed app: it is a window-backed app
    /// (`app_system::AppSystem::recompute_running`), which has an identity but no icon.
    pub icon: Option<AppIconRef>,
    /// `_getCaption` (`windowPreview.js:259-266`): the window title, falling back
    /// to the app name. Empty draws nothing.
    pub caption: String,
    /// The icon's scale on the overview axis — see the module docs. 0 draws none.
    pub icon_scale: f64,
}

/// One output's picker-chrome bakes: two disc bakes (normal + hover) so a frame
/// that draws one hovered and one fading-out button doesn't thrash a single
/// cache, plus one caption bake per caption *string* — two previews of the same
/// width but different titles would otherwise collide on
/// [`BakeCache`]'s (scale, physical size) key.
#[derive(Default)]
struct OutputChrome {
    disc: RefCell<BakeCache>,
    disc_hover: RefCell<BakeCache>,
    captions: RefCell<HashMap<String, BakeCache>>,
}

/// The picker chrome's GPU caches. The close glyph and the app icons are not
/// here: they come from the shared [`IconCache`] / [`AppIconCache`].
#[derive(Default)]
pub struct PreviewChrome {
    /// **One entry per output**, by name: the captions are pruned against the overlays of
    /// whichever output is drawing, so a shared map dropped every caption the other screen had
    /// baked — a screen with no hovered preview cleared it outright — and the discs are keyed by
    /// the drawing output's scale. Shared, a hovered preview's caption was re-baked, with a new
    /// `Id`, every time the other display rendered.
    outputs: RefCell<HashMap<String, OutputChrome>>,
    /// Not per output: an icon already on the GPU is not uploaded again for another screen.
    icon_uploads: SharedAppIconUploads,
}

impl PreviewChrome {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop this icon's uploads at every scale — the app-icon cache's
    /// invalidation hook (a re-theme or a fresh decode landing).
    pub fn drop_icon_upload(&self, icon: &AppIconRef, logical_px: u16) {
        widget::drop_app_icon_upload(&mut self.icon_uploads.borrow_mut(), icon, logical_px);
    }

    /// Render each preview's chrome, topmost first (the caller pushes these above
    /// the previews): close button, caption plate, app icon. `output` names the
    /// screen being drawn, whose bakes these are.
    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        icons: &IconCache,
        app_icons: &AppIconCache,
        output: &str,
        scale: f64,
        overlays: &[PreviewOverlay],
    ) -> Vec<PreviewChromeRenderElement> {
        let mut elements: Vec<PreviewChromeRenderElement> = Vec::new();
        let mut outputs = self.outputs.borrow_mut();
        if overlays.is_empty() {
            outputs.remove(output);
            return elements;
        }
        let chrome = outputs.entry(output.to_owned()).or_default();

        // Only the captions on screen keep a bake; a retitled window would
        // otherwise leak one cache entry per title it ever had.
        chrome
            .captions
            .borrow_mut()
            .retain(|text, _| overlays.iter().any(|o| &o.caption == text));

        // The glyph (white — `color: $system_fg_color`), cached by the icon cache.
        let icon = icons.texture(
            renderer,
            "preview-close-symbolic",
            CLOSE_ICON_PX,
            scale,
            style::TEXT,
        );

        for overlay in overlays {
            let rect = close_rect(overlay.preview);
            let bg = if overlay.hovered {
                CLOSE_BG_HOVER
            } else {
                CLOSE_BG
            };
            let cache = if overlay.hovered {
                &chrome.disc_hover
            } else {
                &chrome.disc
            };

            // The glyph goes on top of the disc, so it is pushed first.
            if let Some(tb) = icon.as_ref() {
                let logical = tb.logical_size();
                let center = rect.loc + rect.size.downscale(2.).to_point();
                elements.push(
                    TextureRenderElement::from_texture_buffer(
                        tb.clone(),
                        center - Point::from((logical.w / 2., logical.h / 2.)),
                        overlay.alpha,
                        None,
                        None,
                        Kind::Unspecified,
                    )
                    .into(),
                );
            }

            let disc = widget::bake(
                renderer,
                &mut cache.borrow_mut(),
                scale,
                rect.size,
                0,
                |_| Ok(()),
                |frame, phys, ()| {
                    let mut p = Painter::new(frame, scale, phys);
                    p.clear(style::TRANSPARENT)?;
                    // `border-radius: $forced_circular_radius` — a full circle.
                    p.fill_rounded_full(CLOSE_SIZE / 2., bg)
                },
            );
            match disc {
                Ok(buffer) => {
                    elements.push(
                        TextureRenderElement::from_texture_buffer(
                            buffer,
                            rect.loc,
                            overlay.alpha,
                            None,
                            None,
                            Kind::Unspecified,
                        )
                        .into(),
                    );
                }
                Err(err) => tracing::error!("error baking the preview close button: {err:#}"),
            }
        }

        for overlay in overlays {
            if overlay.icon_scale > 0. {
                if let Some(icon) = &overlay.icon {
                    let center = icon_center(overlay.preview);
                    // The icon is uploaded at its *full* size and scaled on the
                    // GPU, never re-decoded per frame: `_updateIconScale` is a
                    // `scale_x/scale_y` on the actor, and a size in the cache key
                    // would re-rasterize the icon on every frame of the open
                    // animation.
                    if let Some(element) = widget::app_icon_element(
                        renderer,
                        &mut self.icon_uploads.borrow_mut(),
                        app_icons,
                        icon,
                        ICON_SIZE,
                        scale,
                        Point::default(),
                        center,
                        1.,
                    ) {
                        let origin = center.to_physical_precise_round(scale);
                        elements.push(
                            RescaleRenderElement::from_element(element, origin, overlay.icon_scale)
                                .into(),
                        );
                    }
                }
            }

            if overlay.alpha > 0. && !overlay.caption.is_empty() {
                if let Some(element) = Self::caption_element(chrome, renderer, scale, overlay) {
                    elements.push(element.into());
                }
            }
        }

        elements
    }

    /// One caption plate, baked and placed. `None` when the bake failed.
    fn caption_element(
        chrome: &OutputChrome,
        renderer: &mut VulkanRenderer,
        scale: f64,
        overlay: &PreviewOverlay,
    ) -> Option<TextureRenderElement<VkTexture>> {
        let text = caption_text(&overlay.caption, overlay.preview);
        let plate = caption_rect(overlay.preview, caption_size(&text));
        let radius = plate.size.h / 2.;

        // The bake carries the shadow too, so it is the plate padded by the blur's reach
        // (~3σ) plus the downward offset on every side.
        let pad = CAPTION_SHADOW.blur * 1.5 + 1. + CAPTION_SHADOW.offset.1;
        let inner = Rectangle::new(Point::from((pad, pad)), plate.size);
        let size = plate.size + Size::from((pad * 2., pad * 2.));

        // The cache is per caption string, but the fitted text also depends on the preview's
        // width; a different cut is a different bake.
        let revision = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            text.hash(&mut h);
            h.finish()
        };

        let mut caches = chrome.captions.borrow_mut();
        let cache = caches.entry(overlay.caption.clone()).or_default();
        let texture = widget::bake(
            renderer,
            cache,
            scale,
            size,
            revision,
            |r| {
                let mut shaper = TextShaper::new(r, scale);
                shaper.shape(&text, TextStyle::new(CAPTION_PT))
            },
            |frame, phys, label| {
                let mut p = Painter::new(frame, scale, phys);
                p.clear(style::TRANSPARENT)?;
                if style::shadows() {
                    p.drop_shadow(
                        inner,
                        radius,
                        CAPTION_SHADOW.blur,
                        CAPTION_SHADOW.offset,
                        CAPTION_SHADOW.color,
                    )?;
                }
                p.fill_rounded(inner, radius, style::osd_bg())?;
                p.stroke_rounded(
                    inner,
                    radius,
                    style::osd_border_width(),
                    style::osd_border(),
                )?;
                p.text(
                    label,
                    inner.loc + inner.size.downscale(2.).to_point(),
                    Align::CENTER,
                    style::OSD_FG,
                )
            },
        );
        match texture {
            Ok(buffer) => Some(TextureRenderElement::from_texture_buffer(
                buffer,
                plate.loc - Point::from((pad, pad)),
                overlay.alpha,
                None,
                None,
                Kind::Unspecified,
            )),
            Err(err) => {
                tracing::error!("error baking a preview caption: {err:#}");
                None
            }
        }
    }
}

synoik_render_elements! {
    PreviewChromeRenderElement => {
        Texture = TextureRenderElement<VkTexture>,
        // The app icon, scaled on the overview axis (`_updateIconScale`).
        ScaledTexture = RescaleRenderElement<TextureRenderElement<VkTexture>>,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The preview's slot, and the reference numbers the constraints work out to
    /// (`windowPreview.js:142-186`): the icon hangs `ICON_SIZE * (1 - ICON_OVERLAP)`
    /// = 19.2px below the preview. The caption plate (our divergence) sits inside
    /// the preview, `ICON_TITLE_SPACING` above the icon.
    #[test]
    fn the_icon_straddles_the_preview_bottom_and_the_caption_sits_above_it() {
        let preview =
            Rectangle::<f64, Logical>::new(Point::from((100., 200.)), Size::from((400., 300.)));
        let bottom = 500.;

        let center = icon_center(preview);
        assert_eq!(center.x, 300., "the icon is centred over the preview");
        let icon_top = center.y - ICON_SIZE / 2.;
        let icon_bottom = center.y + ICON_SIZE / 2.;
        assert!(
            (icon_bottom - bottom - 19.2).abs() < 1e-9,
            "30% of the icon hangs below the preview, got {}",
            icon_bottom - bottom
        );
        assert!(
            (bottom - icon_top - ICON_SIZE * ICON_OVERLAP).abs() < 1e-9,
            "70% of it sits inside"
        );

        let plate = caption_rect(preview, Size::from((200., 30.)));
        assert_eq!(plate.loc.x, 200., "the caption is centred too");
        assert!(
            (plate.loc.y + plate.size.h - (icon_top - ICON_TITLE_SPACING)).abs() < 1e-9,
            "the caption ends ICON_TITLE_SPACING above the icon"
        );
        assert!(
            preview.contains_rect(plate),
            "and it is inside the preview, so it can never fall off the screen"
        );
    }

    /// A title wider than the preview is ellipsized to keep the plate inside it.
    #[test]
    fn a_long_caption_is_ellipsized_to_the_preview() {
        let preview =
            Rectangle::<f64, Logical>::new(Point::from((0., 0.)), Size::from((240., 200.)));
        let long = "a considerably longer window title than any preview this narrow can hold";
        let text = caption_text(long, preview);
        assert_ne!(text, long, "the title is cut");
        let plate = caption_rect(preview, caption_size(&text));
        assert!(
            plate.size.w <= preview.size.w - 2. * CAPTION_MARGIN + 1e-9,
            "the plate keeps CAPTION_MARGIN from both sides: {plate:?} in {preview:?}"
        );

        let short = caption_text("Files", preview);
        assert_eq!(short, "Files", "a title that fits is left alone");
    }

    /// `%tooltip` is a pill: its radius is half its height whatever the label
    /// (`border-radius: $forced_circular_radius`, `_common.scss:230`), and the box
    /// is the label plus `$base_padding`/`$base_padding * 2`.
    #[test]
    fn the_caption_pill_grows_with_its_label_but_stays_a_pill() {
        use crate::ui::widget::Tooltip;

        let short = Tooltip::size("hi");
        let long = Tooltip::size("a considerably longer window title");
        assert!(long.w > short.w, "the pill widens with the label");
        assert_eq!(long.h, short.h, "but a single line never grows taller");
        assert_eq!(Tooltip::radius(long), long.h / 2.);
        assert!(
            short.h >= 2. * Tooltip::PAD_V,
            "the box model includes the vertical padding"
        );
    }
}
