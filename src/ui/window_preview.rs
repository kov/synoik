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
use smithay::utils::{Logical, Point, Rectangle, Size, Transform};

use crate::app_system::AppIconRef;
use crate::niri_render_elements;
use crate::render_helpers::icon::{AppIconCache, IconCache};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::ui::widget::{
    self, style, BakeCache, Painter, Rgba, SharedAppIconUploads, TextShaper, TextStyle,
};

/// The button's box, logical px — `$large_icon_size` (`_window-picker.scss:35-36`).
pub const CLOSE_SIZE: f64 = 32.;

/// The glyph inside it — `& StIcon { icon-size: $medium_icon_size }`
/// (`_window-picker.scss:44`).
const CLOSE_ICON_PX: f64 = 24.;

/// `$window_close_button_color` = `transparentize(lighten($system_bg_color, 7%),
/// .02)` (`_window-picker.scss:2`), with `$system_bg_color` =
/// `lighten($system_base_color, 5%)` (`_colors.scss:47`) — `#3f3f46` at 98%.
const CLOSE_BG: Rgba = [0.2471, 0.2471, 0.2745, 0.98];

/// `.window-close:hover` — the same color lightened 7% (`_window-picker.scss:47`).
/// Note this widget lightens on hover, but the direction is read, never assumed.
const CLOSE_BG_HOVER: Rgba = [0.3137, 0.3137, 0.349, 0.98];

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

/// The caption pill's top-left for a preview of `preview` and a pill of `size`.
/// X-centred; Y is the preview's bottom edge plus the icon's overhang and the
/// spacing (`windowPreview.js:170-186`).
pub fn caption_origin(
    preview: Rectangle<f64, Logical>,
    size: Size<f64, Logical>,
) -> Point<f64, Logical> {
    let bottom = preview.loc.y + preview.size.h;
    Point::from((
        preview.loc.x + (preview.size.w - size.w) / 2.,
        bottom + ICON_SIZE * (1. - ICON_OVERLAP) + ICON_TITLE_SPACING,
    ))
}

/// One preview's chrome, as the renderer needs it: where the preview draws, how
/// far its hover overlay has faded in, whether the pointer is on the close button,
/// and the app identity behind it.
#[derive(Debug, Clone)]
pub struct PreviewOverlay {
    pub preview: Rectangle<f64, Logical>,
    pub alpha: f32,
    pub hovered: bool,
    /// The window's app icon, resolved through the app model. `None` when the
    /// window resolves to no installed app — GNOME always has one because it
    /// synthesizes a window-backed `ShellApp`, which we do not
    /// (`app_system::AppSystem::recompute_running`).
    pub icon: Option<AppIconRef>,
    /// `_getCaption` (`windowPreview.js:259-266`): the window title, falling back
    /// to the app name. Empty draws nothing.
    pub caption: String,
    /// The icon's scale on the overview axis — see the module docs. 0 draws none.
    pub icon_scale: f64,
}

/// The picker chrome's GPU caches: two disc bakes (normal + hover) so a frame
/// that draws one hovered and one fading-out button doesn't thrash a single
/// cache, plus one caption bake per caption *string* — two previews of the same
/// width but different titles would otherwise collide on
/// [`BakeCache`]'s (scale, physical size) key. The close glyph and the app icons
/// are not here: they come from the shared [`IconCache`] / [`AppIconCache`].
#[derive(Default)]
pub struct PreviewChrome {
    disc: RefCell<BakeCache>,
    disc_hover: RefCell<BakeCache>,
    captions: RefCell<HashMap<String, BakeCache>>,
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
    /// the previews): close button, caption pill, app icon.
    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        icons: &IconCache,
        app_icons: &AppIconCache,
        scale: f64,
        overlays: &[PreviewOverlay],
    ) -> Vec<PreviewChromeRenderElement> {
        let mut elements: Vec<PreviewChromeRenderElement> = Vec::new();
        if overlays.is_empty() {
            self.captions.borrow_mut().clear();
            return elements;
        }

        // Only the captions on screen keep a bake; a retitled window would
        // otherwise leak one cache entry per title it ever had.
        self.captions
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
                &self.disc_hover
            } else {
                &self.disc
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
                Ok(texture) => {
                    let buffer = TextureBuffer::from_texture(
                        renderer,
                        texture,
                        scale,
                        Transform::Normal,
                        vec![],
                    );
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
                if let Some(element) = self.caption_element(renderer, scale, overlay) {
                    elements.push(element.into());
                }
            }
        }

        elements
    }

    /// One caption pill, baked and placed. `None` when the bake failed.
    fn caption_element(
        &self,
        renderer: &mut VulkanRenderer,
        scale: f64,
        overlay: &PreviewOverlay,
    ) -> Option<TextureRenderElement<VkTexture>> {
        let size = widget::Tooltip::size(&overlay.caption);
        let mut caches = self.captions.borrow_mut();
        let cache = caches.entry(overlay.caption.clone()).or_default();
        let texture = widget::bake(
            renderer,
            cache,
            scale,
            size,
            0,
            |r| {
                let mut shaper = TextShaper::new(r, scale);
                shaper.shape(&overlay.caption, TextStyle::new(widget::Tooltip::TEXT_PT))
            },
            |frame, phys, label| {
                let mut p = Painter::new(frame, scale, phys);
                p.tooltip(size, label)
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
                Some(TextureRenderElement::from_texture_buffer(
                    buffer,
                    caption_origin(overlay.preview, size),
                    overlay.alpha,
                    None,
                    None,
                    Kind::Unspecified,
                ))
            }
            Err(err) => {
                tracing::error!("error baking a preview caption: {err:#}");
                None
            }
        }
    }
}

niri_render_elements! {
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
    /// = 19.2px below the preview, and the caption's top is that plus
    /// `ICON_TITLE_SPACING`.
    #[test]
    fn the_icon_straddles_the_preview_bottom_and_the_caption_clears_it() {
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

        let size = Size::<f64, Logical>::from((200., 30.));
        let origin = caption_origin(preview, size);
        assert_eq!(origin.x, 200., "the caption is centred too");
        assert!(
            (origin.y - (bottom + 19.2 + ICON_TITLE_SPACING)).abs() < 1e-9,
            "the caption clears the icon's overhang by ICON_TITLE_SPACING"
        );
        assert!(
            origin.y > icon_bottom,
            "and so it never overlaps the icon itself"
        );
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
