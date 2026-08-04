// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

use glam::Vec2;
use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::utils::CommitCounter;
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size};
use synoik_config::{
    Color, CornerRadius, GradientColorSpace, GradientInterpolation, HueInterpolation,
};

/// Renders a wide variety of borders and border parts.
///
/// This includes:
/// * sub- or super-rect of an angled linear gradient like CSS linear-gradient(angle, a, b).
/// * corner rounding.
/// * as a background rectangle and as parts of a border line.
///
/// Cloned per frame by its owners (focus ring, tab indicator, layout shadow) with the `Id` carried
/// along, so clones share one damage identity.
#[derive(Debug, Clone)]
pub struct BorderRenderElement {
    id: Id,
    commit_counter: CommitCounter,
    /// Where the element is drawn. Its size is [`Parameters::size`]; only [`Self::with_location`]
    /// sets the location, and it deliberately does not bump the commit counter.
    location: Point<f64, Logical>,
    params: Parameters,
}

/// Renderer-agnostic derived inputs for the border shader (see [`BorderRenderElement::computed`]).
#[derive(Clone, Copy)]
struct ComputedBorder {
    grad_offset: Vec2,
    grad_vec: Vec2,
    grad_width: f32,
    area_size: Vec2,
    geo_loc: Vec2,
    geo_size: Vec2,
    colorspace: f32,
    hue_interpolation: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Parameters {
    size: Size<f64, Logical>,
    gradient_area: Rectangle<f64, Logical>,
    gradient_format: GradientInterpolation,
    color_from: Color,
    color_to: Color,
    angle: f32,
    geometry: Rectangle<f64, Logical>,
    border_width: f32,
    corner_radius: CornerRadius,
    // Should only be used for visual improvements, i.e. corner radius anti-aliasing.
    scale: f32,
    alpha: f32,
}

impl BorderRenderElement {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        size: Size<f64, Logical>,
        gradient_area: Rectangle<f64, Logical>,
        gradient_format: GradientInterpolation,
        color_from: Color,
        color_to: Color,
        angle: f32,
        geometry: Rectangle<f64, Logical>,
        border_width: f32,
        corner_radius: CornerRadius,
        scale: f32,
        alpha: f32,
    ) -> Self {
        Self {
            id: Id::new(),
            commit_counter: CommitCounter::default(),
            location: Point::default(),
            params: Parameters {
                size,
                gradient_area,
                gradient_format,
                color_from,
                color_to,
                angle,
                geometry,
                border_width,
                corner_radius,
                scale,
                alpha,
            },
        }
    }

    pub fn empty() -> Self {
        Self {
            id: Id::new(),
            commit_counter: CommitCounter::default(),
            location: Point::default(),
            params: Parameters {
                size: Default::default(),
                gradient_area: Default::default(),
                gradient_format: GradientInterpolation::default(),
                color_from: Default::default(),
                color_to: Default::default(),
                angle: 0.,
                geometry: Default::default(),
                border_width: 0.,
                corner_radius: Default::default(),
                scale: 1.,
                alpha: 1.,
            },
        }
    }

    pub fn damage_all(&mut self) {
        self.commit_counter.increment();
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &mut self,
        size: Size<f64, Logical>,
        gradient_area: Rectangle<f64, Logical>,
        gradient_format: GradientInterpolation,
        color_from: Color,
        color_to: Color,
        angle: f32,
        geometry: Rectangle<f64, Logical>,
        border_width: f32,
        corner_radius: CornerRadius,
        scale: f32,
        alpha: f32,
    ) {
        let params = Parameters {
            size,
            gradient_area,
            gradient_format,
            color_from,
            color_to,
            angle,
            geometry,
            border_width,
            corner_radius,
            scale,
            alpha,
        };
        // Only bump the commit counter when something actually changed: this is called every frame,
        // so incrementing unconditionally would report damage over the whole element every frame.
        if self.params == params {
            return;
        }

        self.params = params;
        self.commit_counter.increment();
    }

    /// The derived quantities the border draw needs, computed from [`Self::params`].
    fn computed(&self) -> ComputedBorder {
        let Parameters {
            size,
            gradient_area,
            gradient_format,
            angle,
            geometry,
            ..
        } = self.params;

        let grad_offset = geometry.loc - gradient_area.loc;
        let grad_offset = Vec2::new(grad_offset.x as f32, grad_offset.y as f32);

        let grad_dir = Vec2::from_angle(angle);

        let (w, h) = (gradient_area.size.w as f32, gradient_area.size.h as f32);

        let mut grad_area_diag = Vec2::new(w, h);
        if (grad_dir.x < 0. && 0. <= grad_dir.y) || (0. <= grad_dir.x && grad_dir.y < 0.) {
            grad_area_diag.x = -w;
        }

        let mut grad_vec = grad_area_diag.project_onto(grad_dir);
        if grad_dir.y < 0. {
            grad_vec = -grad_vec;
        }

        let area_size = Vec2::new(size.w as f32, size.h as f32);
        let geo_loc = Vec2::new(geometry.loc.x as f32, geometry.loc.y as f32);
        let geo_size = Vec2::new(geometry.size.w as f32, geometry.size.h as f32);

        let colorspace = match gradient_format.color_space {
            GradientColorSpace::Srgb => 0.,
            GradientColorSpace::SrgbLinear => 1.,
            GradientColorSpace::Oklab => 2.,
            GradientColorSpace::Oklch => 3.,
        };

        let hue_interpolation = match gradient_format.hue_interpolation {
            HueInterpolation::Shorter => 0.,
            HueInterpolation::Longer => 1.,
            HueInterpolation::Increasing => 2.,
            HueInterpolation::Decreasing => 3.,
        };

        ComputedBorder {
            grad_offset,
            grad_vec,
            grad_width: w,
            area_size,
            geo_loc,
            geo_size,
            colorspace,
            hue_interpolation,
        }
    }

    /// Moves the element. Deliberately does not bump the commit counter: the owners re-place a
    /// cloned element every frame, and treating a move as damage here would defeat damage tracking.
    pub fn with_location(mut self, location: Point<f64, Logical>) -> Self {
        self.location = location;
        self
    }
}

impl Default for BorderRenderElement {
    fn default() -> Self {
        Self::empty()
    }
}

// `transform` and `damage_since` are deliberately not overridden — the element this was promoted
// from did not override them either, so both keep the `Element` defaults.
impl Element for BorderRenderElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.commit_counter
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        Rectangle::new(self.location, self.params.size).to_physical_precise_round(scale)
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        Rectangle::from_size(Size::from((1., 1.)))
    }

    fn alpha(&self) -> f32 {
        self.params.alpha
    }

    fn kind(&self) -> Kind {
        Kind::Unspecified
    }
}

// The renderer draws the border procedurally in its own pipeline, reading `params` via
// `computed()`.
use crate::render_helpers::vulkan::{VulkanError, VulkanFrame, VulkanRenderer};

impl BorderRenderElement {
    /// Build the border material's push constants from `params`, with the quad placed at `dst`
    /// showing the `src` sub-rect of the element. (`target` is filled by
    /// `VulkanFrame::render_border`.)
    ///
    /// Like the shadow material, the shader gets `v_uv` running 0..1 across `dst` and derives every
    /// coordinate from `v_uv * area_size - geo_loc`. A `CropRenderElement` shrinks `dst` and
    /// reports the matching sub-rect of the unit `src`, so the crop has to be folded in here or
    /// the whole border — rounding, gradient and all — is squeezed into the surviving strip.
    fn vulkan_push(
        &self,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
    ) -> synoik_vk::render::BorderPush {
        let c = self.computed();
        let p = &self.params;
        // `v_uv * (size * src.size) - (loc - src.loc * size)` == `(src.loc + v_uv * src.size) *
        // size - loc`, so the crop folds into `area_size` and `geo_loc` alone.
        let area = [
            c.area_size.x * src.size.w as f32,
            c.area_size.y * src.size.h as f32,
        ];
        let crop_off = [
            src.loc.x as f32 * c.area_size.x,
            src.loc.y as f32 * c.area_size.y,
        ];
        synoik_vk::render::BorderPush {
            origin: [dst.loc.x as f32, dst.loc.y as f32],
            size: [dst.size.w as f32, dst.size.h as f32],
            // proj/target are placeholders; VulkanFrame::render_border fills them from the frame.
            proj: synoik_vk::render::IDENTITY_PROJ,
            target: [0.0, 0.0],
            border_width: p.border_width,
            colorspace: c.colorspace,
            color_from: p.color_from.to_array_unpremul(),
            color_to: p.color_to.to_array_unpremul(),
            outer_radius: <[f32; 4]>::from(p.corner_radius),
            grad_offset: c.grad_offset.to_array(),
            grad_vec: c.grad_vec.to_array(),
            area_size: area,
            geo_loc: [c.geo_loc.x - crop_off[0], c.geo_loc.y - crop_off[1]],
            geo_size: c.geo_size.to_array(),
            grad_width: c.grad_width,
            hue_interpolation: c.hue_interpolation,
            synoik_scale: p.scale,
            synoik_alpha: p.alpha,
        }
    }
}

impl RenderElement<VulkanRenderer> for BorderRenderElement {
    fn draw(
        &self,
        frame: &mut VulkanFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), VulkanError> {
        frame.render_border(self.vulkan_push(src, dst), dst, damage)
    }

    fn underlying_storage(&self, _renderer: &mut VulkanRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn elem(alpha: f32) -> BorderRenderElement {
        BorderRenderElement::new(
            Size::from((100., 100.)),
            Rectangle::from_size(Size::from((100., 100.))),
            GradientInterpolation::default(),
            Color::new_unpremul(1., 0., 0., 1.),
            Color::new_unpremul(0., 0., 1., 1.),
            0.,
            Rectangle::from_size(Size::from((100., 100.))),
            4.,
            CornerRadius::default(),
            1.,
            alpha,
        )
    }

    /// The owners call `update` every frame with mostly-unchanged parameters and re-place a clone
    /// via `with_location`. Both must leave the commit counter alone, or every frame reports damage
    /// over the whole element and damage tracking stops meaning anything. Only a real parameter
    /// change may bump it.
    #[test]
    fn only_a_real_parameter_change_damages() {
        let mut e = elem(1.);
        let base = e.current_commit();

        e.update(
            Size::from((100., 100.)),
            Rectangle::from_size(Size::from((100., 100.))),
            GradientInterpolation::default(),
            Color::new_unpremul(1., 0., 0., 1.),
            Color::new_unpremul(0., 0., 1., 1.),
            0.,
            Rectangle::from_size(Size::from((100., 100.))),
            4.,
            CornerRadius::default(),
            1.,
            1.,
        );
        assert_eq!(
            e.current_commit(),
            base,
            "an update with identical parameters must not damage"
        );

        let moved = e.clone().with_location(Point::from((7., 9.)));
        assert_eq!(
            moved.current_commit(),
            base,
            "with_location must not damage"
        );
        assert_eq!(
            moved.id(),
            e.id(),
            "a relocated clone must keep the damage identity"
        );

        // A changed parameter (alpha) must damage.
        e.update(
            Size::from((100., 100.)),
            Rectangle::from_size(Size::from((100., 100.))),
            GradientInterpolation::default(),
            Color::new_unpremul(1., 0., 0., 1.),
            Color::new_unpremul(0., 0., 1., 1.),
            0.,
            Rectangle::from_size(Size::from((100., 100.))),
            4.,
            CornerRadius::default(),
            1.,
            0.5,
        );
        assert_ne!(
            e.current_commit(),
            base,
            "a changed parameter must damage the element"
        );
    }

    /// `with_location` sets where the element draws; its size comes from the parameters.
    #[test]
    fn geometry_follows_location_and_size() {
        let e = elem(1.).with_location(Point::from((10., 20.)));
        let geo = e.geometry(Scale::from(1.));
        assert_eq!(geo.loc.x, 10);
        assert_eq!(geo.loc.y, 20);
        assert_eq!(geo.size.w, 100);
        assert_eq!(geo.size.h, 100);
    }

    /// The alpha the draw reads is the one `Element` reports (the trap that bit
    /// `ShadowRenderElement`).
    #[test]
    fn element_alpha_matches_the_draw_alpha() {
        let e = elem(0.25);
        assert_eq!(e.alpha(), 0.25);
        let push = e.vulkan_push(
            Rectangle::from_size(Size::from((1., 1.))),
            Rectangle::from_size(smithay::utils::Size::from((100, 100))),
        );
        assert_eq!(push.synoik_alpha, 0.25);
    }

    /// Same crop trap as `ShadowRenderElement`: a `CropRenderElement` shrinks `dst` and reports a
    /// sub-rect of the unit `src`, and the shader's `v_uv * area_size - geo_loc` has to follow it
    /// or the rounding and the gradient get squeezed into the surviving strip.
    #[test]
    fn a_cropped_draw_keeps_the_border_anchored() {
        let e = elem(1.);

        let full = e.vulkan_push(
            Rectangle::from_size(Size::from((1., 1.))),
            Rectangle::from_size(smithay::utils::Size::from((100, 100))),
        );
        assert_eq!(full.area_size, [100., 100.]);
        assert_eq!(full.geo_loc, [0., 0.]);

        // Crop away the left 40%.
        let cropped = e.vulkan_push(
            Rectangle::new(Point::from((0.4, 0.)), Size::from((0.6, 1.))),
            Rectangle::new(
                smithay::utils::Point::from((40, 0)),
                smithay::utils::Size::from((60, 100)),
            ),
        );
        let close = |a: [f32; 2], b: [f32; 2]| {
            assert!(
                (a[0] - b[0]).abs() < 1e-3 && (a[1] - b[1]).abs() < 1e-3,
                "{a:?} != {b:?}"
            );
        };
        close(cropped.area_size, [60., 100.]);
        close(cropped.geo_loc, [-40., 0.]);
        assert_eq!(cropped.geo_size, full.geo_size);

        let coords =
            |p: &synoik_vk::render::BorderPush, uv: f32| uv * p.area_size[0] - p.geo_loc[0];
        assert!((coords(&full, 0.7) - coords(&cropped, 0.5)).abs() < 1e-3);
    }
}
