use std::collections::HashMap;
use std::rc::Rc;

use glam::{Mat3, Vec2};
use niri_config::{
    Color, CornerRadius, GradientColorSpace, GradientInterpolation, HueInterpolation,
};
use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::gles::{GlesError, GlesFrame, GlesRenderer, Uniform};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::gpu_span_location;
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform};

use super::renderer::NiriRenderer;
use super::shader_element::ShaderRenderElement;
use super::shaders::{mat3_uniform, ProgramType, Shaders};

/// Renders a wide variety of borders and border parts.
///
/// This includes:
/// * sub- or super-rect of an angled linear gradient like CSS linear-gradient(angle, a, b).
/// * corner rounding.
/// * as a background rectangle and as parts of a border line.
#[derive(Debug, Clone)]
pub struct BorderRenderElement {
    inner: ShaderRenderElement,
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
        let inner = ShaderRenderElement::empty(ProgramType::Border, Kind::Unspecified);
        let mut rv = Self {
            inner,
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
        };
        rv.update_inner();
        rv
    }

    pub fn empty() -> Self {
        let inner = ShaderRenderElement::empty(ProgramType::Border, Kind::Unspecified);
        Self {
            inner,
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
        self.inner.damage_all();
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
        if self.params == params {
            return;
        }

        self.params = params;
        self.update_inner();
    }

    /// The renderer-agnostic derived quantities the border shader needs, computed from
    /// [`Self::params`]. Shared by the GLES uniform build ([`Self::update_inner`]) and the owned
    /// Vulkan push (`vulkan_push`) so the two never drift.
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

    fn update_inner(&mut self) {
        let Parameters {
            size,
            color_from,
            color_to,
            border_width,
            corner_radius,
            scale,
            alpha,
            ..
        } = self.params;

        let c = self.computed();
        let input_to_geo =
            Mat3::from_scale(c.area_size) * Mat3::from_translation(-c.geo_loc / c.area_size);

        self.inner.update(
            size,
            None,
            scale,
            alpha,
            Rc::new([
                Uniform::new("colorspace", c.colorspace),
                Uniform::new("hue_interpolation", c.hue_interpolation),
                Uniform::new("color_from", color_from.to_array_unpremul()),
                Uniform::new("color_to", color_to.to_array_unpremul()),
                Uniform::new("grad_offset", c.grad_offset.to_array()),
                Uniform::new("grad_width", c.grad_width),
                Uniform::new("grad_vec", c.grad_vec.to_array()),
                mat3_uniform("input_to_geo", input_to_geo),
                Uniform::new("geo_size", c.geo_size.to_array()),
                Uniform::new("outer_radius", <[f32; 4]>::from(corner_radius)),
                Uniform::new("border_width", border_width),
            ]),
            HashMap::new(),
        );
    }

    pub fn with_location(mut self, location: Point<f64, Logical>) -> Self {
        self.inner = self.inner.with_location(location);
        self
    }

    /// Whether `renderer` can draw this element as a real (rounded / gradient) border rather than a
    /// plain solid-color fallback. Callers use it to choose `BorderRenderElement` (rounded corners,
    /// gradients) over pointy `SolidColorRenderElement` quads. True when the GLES border program is
    /// loaded, **or** on the owned Vulkan renderer — which draws borders procedurally
    /// (`VulkanFrame::render_border`, always with rounded corners) and needs no GLES program.
    pub fn has_shader(renderer: &mut impl NiriRenderer) -> bool {
        if renderer.try_as_vulkan_renderer().is_some() {
            return true;
        }
        Shaders::get(renderer).is_some_and(|s| s.program(ProgramType::Border).is_some())
    }
}

impl Default for BorderRenderElement {
    fn default() -> Self {
        Self::empty()
    }
}

impl Element for BorderRenderElement {
    fn id(&self) -> &Id {
        self.inner.id()
    }

    fn current_commit(&self) -> CommitCounter {
        self.inner.current_commit()
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.inner.geometry(scale)
    }

    fn transform(&self) -> Transform {
        self.inner.transform()
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        self.inner.src()
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        self.inner.damage_since(scale, commit)
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        self.inner.opaque_regions(scale)
    }

    fn alpha(&self) -> f32 {
        self.inner.alpha()
    }

    fn kind(&self) -> Kind {
        self.inner.kind()
    }
}

impl RenderElement<GlesRenderer> for BorderRenderElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        let _span = tracy_client::span!("BorderRenderElement::draw");
        frame.with_gpu_span(gpu_span_location!("BorderRenderElement::draw"), |frame| {
            RenderElement::<GlesRenderer>::draw(
                &self.inner,
                frame,
                src,
                dst,
                damage,
                opaque_regions,
                cache,
            )
        })
    }

    fn underlying_storage(&self, renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        self.inner.underlying_storage(renderer)
    }
}

// The owned Vulkan renderer draws the border procedurally in its own pipeline (M3); it reads the
// raw `params` (not the GLES `inner` uniform list), sharing the derivation via `computed()`.
use crate::render_helpers::vulkan::{VulkanError, VulkanFrame, VulkanRenderer};

impl BorderRenderElement {
    /// Build the border material's push constants from `params`, with the quad placed at `dst`.
    /// (`target` is filled by `VulkanFrame::render_border`.)
    fn vulkan_push(&self, dst: Rectangle<i32, Physical>) -> niri_vk::render::BorderPush {
        let c = self.computed();
        let p = &self.params;
        niri_vk::render::BorderPush {
            origin: [dst.loc.x as f32, dst.loc.y as f32],
            size: [dst.size.w as f32, dst.size.h as f32],
            // proj/target are placeholders; VulkanFrame::render_border fills them from the frame.
            proj: niri_vk::render::IDENTITY_PROJ,
            target: [0.0, 0.0],
            border_width: p.border_width,
            colorspace: c.colorspace,
            color_from: p.color_from.to_array_unpremul(),
            color_to: p.color_to.to_array_unpremul(),
            outer_radius: <[f32; 4]>::from(p.corner_radius),
            grad_offset: c.grad_offset.to_array(),
            grad_vec: c.grad_vec.to_array(),
            area_size: c.area_size.to_array(),
            geo_loc: c.geo_loc.to_array(),
            geo_size: c.geo_size.to_array(),
            grad_width: c.grad_width,
            hue_interpolation: c.hue_interpolation,
            niri_scale: p.scale,
            niri_alpha: p.alpha,
        }
    }
}

impl RenderElement<VulkanRenderer> for BorderRenderElement {
    fn draw(
        &self,
        frame: &mut VulkanFrame<'_, '_>,
        _src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), VulkanError> {
        frame.render_border(self.vulkan_push(dst), dst, damage)
    }

    fn underlying_storage(&self, _renderer: &mut VulkanRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}
