// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! A live window, composited at a smaller size — `_createWindowClone`
//! (`js/ui/altTab.js:32-46`).
//!
//! **Live, not a snapshot.** The window's own surfaces are drawn through a scale, so a video
//! playing in an Alt-Tab preview keeps playing. There is no offscreen and no capture: the sandwich
//! is `render_normal` → clip to the window's geometry → `RescaleRenderElement` →
//! `RelocateRenderElement`, the same shape the overview's expose (`layout/workspace.rs`) and the
//! workspace thumbnail strip (`layout/monitor.rs`) already use.
//!
//! The rescale walks into a rounding trap that is already pinned in
//! `render_helpers::solid_color::rescale_tests`: rounding a rect's location and size apart doubles
//! the error at the far edge. Our smithay fork rounds extremities instead, which is why an
//! animated preview does not shimmer.
//!
//! **Known gap:** minification is done by the sampler with no mipmaps, and this is the worst case
//! for it in the codebase — a 1080p window into a 128px box is a ~1/8 downscale where the
//! overview's tiles are nearer 1/3. `ui/mru.rs` carries the same note ("this could use mipmaps,
//! for that it should be rendered through an offscreen"). Whether it is visible enough to be worth
//! an offscreen is a measurement on a text-heavy window, not a guess.

use smithay::backend::renderer::element::utils::{
    Relocate, RelocateRenderElement, RescaleRenderElement,
};
use smithay::utils::{Logical, Point, Rectangle, Scale, Size};
use synoik_config::{Color, CornerRadius, GradientInterpolation};

use crate::layout::{LayoutElement, LayoutElementRenderElement};
use crate::render_helpers::border::BorderRenderElement;
use crate::render_helpers::clipped_surface::ClippedSurfaceRenderElement;
use crate::render_helpers::RenderCtx;
use crate::synoik_render_elements;
use crate::window::mapped::Mapped;

// The window's own content, clipped to its geometry.
synoik_render_elements! {
    WindowThumbnailInner => {
        LayoutElement = LayoutElementRenderElement,
        ClippedSurface = ClippedSurfaceRenderElement,
        Border = BorderRenderElement,
    }
}

/// That content, scaled down and moved into its slot.
pub type WindowThumbnailRenderElement =
    RelocateRenderElement<RescaleRenderElement<WindowThumbnailInner>>;

/// How much to shrink `window` to fit inside a `side`x`side` box — `_createWindowClone`'s
/// `Math.min(1.0, size / width, size / height)` (`altTab.js:34`).
///
/// **The leading `1.0` is not decoration.** A window smaller than the box is drawn at its own
/// size, never blown up, so a small dialog shows small. Dropping the clamp makes every tiny
/// window a blurry full-size preview.
///
/// The scale is uniform, so aspect is preserved and the preview is letterboxed inside the square.
pub fn fit_scale(window: Size<f64, Logical>, side: f64) -> f64 {
    if window.w <= 0. || window.h <= 0. {
        return 1.;
    }
    (side / window.w).min(side / window.h).min(1.)
}

/// Where a window of `window` size lands inside the `box_rect` it is centered in.
pub fn fit_rect(
    window: Size<f64, Logical>,
    box_rect: Rectangle<f64, Logical>,
) -> Rectangle<f64, Logical> {
    let scale = fit_scale(window, box_rect.size.w.min(box_rect.size.h));
    let size = Size::from((window.w * scale, window.h * scale));
    Rectangle::new(
        Point::from((
            box_rect.loc.x + (box_rect.size.w - size.w) / 2.,
            box_rect.loc.y + (box_rect.size.h - size.h) / 2.,
        )),
        size,
    )
}

/// Draw `mapped` live, fitted and centered inside `box_rect`.
///
/// `output_scale` is the output's fractional scale; `alpha` fades the whole preview.
///
/// `corner_radius` rounds the preview itself, in **final** logical px — the radius you want to see
/// on screen, not in the window's own space. It is divided by the fit scale on the way in, since
/// the clip is applied before the rescale; a preview shrunk 5x needs a 5x radius to come out
/// right. This is the switcher sub-list's `.thumbnail` `border-radius`
/// (`_switcher-popup.scss:53-56`); pass 0 where the reference rounds nothing, like the window
/// switcher's own 128px previews.
///
/// The window's *own* corner radius still applies — a CSD window keeps its rounded corners in a
/// preview — so the two are combined by taking the larger. They are the same clip: rounding the
/// preview more than the window rounds itself is what the sub-list asks for, and rounding it less
/// would square off corners the window had already curved.
pub fn render(
    mapped: &Mapped,
    mut ctx: RenderCtx,
    box_rect: Rectangle<f64, Logical>,
    output_scale: f64,
    alpha: f32,
    corner_radius: f64,
    push: &mut dyn FnMut(WindowThumbnailRenderElement),
) {
    let _span = tracy_client::span!("window_thumbnail::render");

    let window_size = mapped.size().to_f64();
    if window_size.w <= 0. || window_size.h <= 0. {
        return;
    }

    let dest = fit_rect(window_size, box_rect);
    let zoom = fit_scale(window_size, box_rect.size.w.min(box_rect.size.h));
    let s = Scale::from(output_scale);

    // Clip the window's surfaces to its geometry, so CSD shadows and over-sized buffers do not
    // spill out of the preview. A blocked-out window arrives as a solid colour instead and needs
    // the rounded shader explicitly, since its CSD corners were already rounded by the client.
    let geo = Rectangle::from_size(window_size);
    let own = if mapped.sizing_mode().is_normal() {
        mapped.geometry_corner_radius()
    } else {
        CornerRadius::default()
    };
    // In window space, so the rescale below lands it on `corner_radius`.
    let wanted = if zoom > 0. { corner_radius / zoom } else { 0. } as f32;
    let radius = CornerRadius {
        top_left: own.top_left.max(wanted),
        top_right: own.top_right.max(wanted),
        bottom_right: own.bottom_right.max(wanted),
        bottom_left: own.bottom_left.max(wanted),
    };

    let clip = move |elem| match elem {
        LayoutElementRenderElement::Wayland(elem) => {
            if ClippedSurfaceRenderElement::will_clip(&elem, s, geo, radius) {
                WindowThumbnailInner::ClippedSurface(ClippedSurfaceRenderElement::new(
                    elem, s, geo, radius,
                ))
            } else {
                WindowThumbnailInner::LayoutElement(LayoutElementRenderElement::Wayland(elem))
            }
        }
        LayoutElementRenderElement::SolidColor(elem) => {
            if radius != CornerRadius::default() {
                BorderRenderElement::new(
                    geo.size,
                    Rectangle::from_size(geo.size),
                    GradientInterpolation::default(),
                    Color::from_color32f(elem.color()),
                    Color::from_color32f(elem.color()),
                    0.,
                    Rectangle::from_size(geo.size),
                    0.,
                    radius,
                    output_scale as f32,
                    1.,
                )
                .into()
            } else {
                LayoutElementRenderElement::SolidColor(elem).into()
            }
        }
        elem @ LayoutElementRenderElement::BackgroundEffect(_) => elem.into(),
    };

    mapped.render_normal(ctx.r(), Point::from((0., 0.)), s, alpha, &mut |elem| {
        let elem = RescaleRenderElement::from_element(clip(elem), Point::from((0, 0)), zoom);
        push(RelocateRenderElement::from_element(
            elem,
            dest.loc.to_physical_precise_round(output_scale),
            Relocate::Relative,
        ));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A window bigger than the box shrinks to fit, preserving aspect.
    #[test]
    fn a_large_window_fits_the_box_on_its_long_side() {
        // 16:9, so width is the binding dimension.
        let scale = fit_scale(Size::from((1920., 1080.)), 128.);
        assert_eq!(scale, 128. / 1920.);

        let rect = fit_rect(
            Size::from((1920., 1080.)),
            Rectangle::new(Point::from((0., 0.)), Size::from((128., 128.))),
        );
        assert_eq!(rect.size.w, 128.);
        assert_eq!(rect.size.h, 72.);
        // Letterboxed: centered vertically in the square.
        assert_eq!(rect.loc.y, 28.);
        assert_eq!(rect.loc.x, 0.);
    }

    /// A window *smaller* than the box is drawn at its own size — the `min(1.0)` clamp.
    ///
    /// Without it a 64x48 dialog would be blown up to fill a 128px box, which is both blurry and
    /// misleading: the preview would claim the window is bigger than it is.
    #[test]
    fn a_small_window_is_never_blown_up() {
        assert_eq!(fit_scale(Size::from((64., 48.)), 128.), 1.);

        let rect = fit_rect(
            Size::from((64., 48.)),
            Rectangle::new(Point::from((10., 20.)), Size::from((128., 128.))),
        );
        assert_eq!(rect.size, Size::from((64., 48.)));
        // Still centered in its cell.
        assert_eq!(rect.loc, Point::from((10. + 32., 20. + 40.)));
    }

    /// A degenerate size cannot produce a NaN scale or a divide by zero.
    #[test]
    fn a_zero_sized_window_does_not_break_the_arithmetic() {
        assert_eq!(fit_scale(Size::from((0., 0.)), 128.), 1.);
        assert_eq!(fit_scale(Size::from((100., 0.)), 128.), 1.);
    }
}
