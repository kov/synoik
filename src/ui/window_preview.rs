//! The window picker's per-preview chrome (`js/ui/windowPreview.js`).
//!
//! gnome-shell's `WindowPreview` hangs three actors off each preview: the app
//! icon (always visible in the window picker), the title caption and the close
//! button. The latter two are the *overlay*: hidden until the pointer enters the
//! preview, then faded in over `WINDOW_OVERLAY_FADE_TIME` alongside the
//! preview's scale-up (`showOverlay`, `windowPreview.js:310-352`).
//!
//! **Scope.** The close button only. The caption and the app icon are deferred —
//! both need a per-window text/app-icon resolution the picker doesn't do yet;
//! recorded in `docs/fork/overview-port.md`.
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

use smithay::backend::renderer::element::Kind;
use smithay::utils::{Logical, Point, Rectangle, Size, Transform};

use crate::render_helpers::icon::IconCache;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::ui::widget::{self, style, BakeCache, Painter, Rgba};

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

/// One preview's overlay, as the renderer needs it: where the preview draws, how
/// far its overlay has faded in, and whether the pointer is on the button itself.
#[derive(Debug, Clone, Copy)]
pub struct PreviewOverlay {
    pub preview: Rectangle<f64, Logical>,
    pub alpha: f32,
    pub hovered: bool,
}

/// The picker chrome's GPU caches: two disc bakes (normal + hover) so a frame
/// that draws one hovered and one fading-out button doesn't thrash a single
/// cache. The close glyph is not here — it comes from the shared [`IconCache`].
#[derive(Default)]
pub struct PreviewChrome {
    disc: RefCell<BakeCache>,
    disc_hover: RefCell<BakeCache>,
}

impl PreviewChrome {
    pub fn new() -> Self {
        Self::default()
    }

    /// Render the close button of each overlay, topmost first (the caller pushes
    /// these above the previews).
    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        icons: &IconCache,
        scale: f64,
        overlays: &[PreviewOverlay],
    ) -> Vec<TextureRenderElement<VkTexture>> {
        let mut elements = Vec::new();
        if overlays.is_empty() {
            return elements;
        }

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
                elements.push(TextureRenderElement::from_texture_buffer(
                    tb.clone(),
                    center - Point::from((logical.w / 2., logical.h / 2.)),
                    overlay.alpha,
                    None,
                    None,
                    Kind::Unspecified,
                ));
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
                    elements.push(TextureRenderElement::from_texture_buffer(
                        buffer,
                        rect.loc,
                        overlay.alpha,
                        None,
                        None,
                        Kind::Unspecified,
                    ));
                }
                Err(err) => tracing::error!("error baking the preview close button: {err:#}"),
            }
        }

        elements
    }
}
