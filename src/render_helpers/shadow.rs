use std::collections::HashMap;
use std::rc::Rc;

use glam::{Mat3, Vec2};
use niri_config::{Color, CornerRadius};
use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::gles::{GlesError, GlesFrame, GlesRenderer, Uniform};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::gpu_span_location;
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform};

use super::renderer::NiriRenderer;
use super::shader_element::ShaderRenderElement;
use super::shaders::{mat3_uniform, ProgramType, Shaders};
use crate::backend::tty::{TtyFrame, TtyRenderer, TtyRendererError};
use crate::render_helpers::renderer::AsGlesFrame as _;

/// Renders a rounded rectangle shadow.
#[derive(Debug, Clone)]
pub struct ShadowRenderElement {
    inner: ShaderRenderElement,
    params: Parameters,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Parameters {
    size: Size<f64, Logical>,
    geometry: Rectangle<f64, Logical>,
    color: Color,
    sigma: f32,
    corner_radius: CornerRadius,
    // Should only be used for visual improvements, i.e. corner radius anti-aliasing.
    scale: f32,
    alpha: f32,

    window_geometry: Rectangle<f64, Logical>,
    window_corner_radius: CornerRadius,
}

impl ShadowRenderElement {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        size: Size<f64, Logical>,
        geometry: Rectangle<f64, Logical>,
        color: Color,
        sigma: f32,
        corner_radius: CornerRadius,
        scale: f32,
        window_geometry: Rectangle<f64, Logical>,
        window_corner_radius: CornerRadius,
        alpha: f32,
    ) -> Self {
        let inner = ShaderRenderElement::empty(ProgramType::Shadow, Kind::Unspecified);
        let mut rv = Self {
            inner,
            params: Parameters {
                size,
                geometry,
                color,
                sigma,
                corner_radius,
                scale,
                alpha,
                window_geometry,
                window_corner_radius,
            },
        };
        rv.update_inner();
        rv
    }

    pub fn empty() -> Self {
        let inner = ShaderRenderElement::empty(ProgramType::Shadow, Kind::Unspecified);
        Self {
            inner,
            params: Parameters {
                size: Default::default(),
                geometry: Default::default(),
                color: Default::default(),
                sigma: 0.,
                corner_radius: Default::default(),
                scale: 1.,
                alpha: 1.,
                window_geometry: Default::default(),
                window_corner_radius: Default::default(),
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
        geometry: Rectangle<f64, Logical>,
        color: Color,
        sigma: f32,
        corner_radius: CornerRadius,
        scale: f32,
        window_geometry: Rectangle<f64, Logical>,
        window_corner_radius: CornerRadius,
        alpha: f32,
    ) {
        let params = Parameters {
            size,
            geometry,
            color,
            sigma,
            alpha,
            corner_radius,
            scale,
            window_geometry,
            window_corner_radius,
        };
        if self.params == params {
            return;
        }

        self.params = params;
        self.update_inner();
    }

    fn update_inner(&mut self) {
        let Parameters {
            size,
            geometry,
            color,
            sigma,
            alpha,
            corner_radius,
            scale,
            window_geometry,
            window_corner_radius,
        } = self.params;

        let area_size = Vec2::new(size.w as f32, size.h as f32);

        let geo_loc = Vec2::new(geometry.loc.x as f32, geometry.loc.y as f32);
        let geo_size = Vec2::new(geometry.size.w as f32, geometry.size.h as f32);

        let input_to_geo =
            Mat3::from_scale(area_size) * Mat3::from_translation(-geo_loc / area_size);

        let window_geo_loc = Vec2::new(window_geometry.loc.x as f32, window_geometry.loc.y as f32);
        let window_geo_size =
            Vec2::new(window_geometry.size.w as f32, window_geometry.size.h as f32);

        let window_input_to_geo =
            Mat3::from_scale(area_size) * Mat3::from_translation(-window_geo_loc / area_size);

        self.inner.update(
            size,
            None,
            scale,
            alpha,
            Rc::new([
                Uniform::new("shadow_color", color.to_array_premul()),
                Uniform::new("sigma", sigma),
                mat3_uniform("input_to_geo", input_to_geo),
                Uniform::new("geo_size", geo_size.to_array()),
                Uniform::new("corner_radius", <[f32; 4]>::from(corner_radius)),
                mat3_uniform("window_input_to_geo", window_input_to_geo),
                Uniform::new("window_geo_size", window_geo_size.to_array()),
                Uniform::new(
                    "window_corner_radius",
                    <[f32; 4]>::from(window_corner_radius),
                ),
            ]),
            HashMap::new(),
        );
    }

    pub fn with_location(mut self, location: Point<f64, Logical>) -> Self {
        self.inner = self.inner.with_location(location);
        self
    }

    pub fn with_alpha(mut self, alpha: f32) -> Self {
        self.inner = self.inner.with_alpha(alpha);
        self
    }

    pub fn has_shader(renderer: &mut impl NiriRenderer) -> bool {
        Shaders::get(renderer).is_some_and(|s| s.program(ProgramType::Shadow).is_some())
    }
}

impl Default for ShadowRenderElement {
    fn default() -> Self {
        Self::empty()
    }
}

impl Element for ShadowRenderElement {
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

impl RenderElement<GlesRenderer> for ShadowRenderElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        let _span = tracy_client::span!("ShadowRenderElement::draw");
        frame.with_gpu_span(gpu_span_location!("ShadowRenderElement::draw"), |frame| {
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

impl<'render> RenderElement<TtyRenderer<'render>> for ShadowRenderElement {
    fn draw(
        &self,
        frame: &mut TtyFrame<'_, '_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), TtyRendererError<'render>> {
        let frame = frame.as_gles_frame();
        RenderElement::<GlesRenderer>::draw(self, frame, src, dst, damage, opaque_regions, cache)?;
        Ok(())
    }

    fn underlying_storage(
        &self,
        renderer: &mut TtyRenderer<'render>,
    ) -> Option<UnderlyingStorage<'_>> {
        self.inner.underlying_storage(renderer)
    }
}

// The owned Vulkan renderer draws the shadow procedurally in its own pipeline (M3), reading the
// raw `params` rather than the GLES `inner` uniform list. The derivation is trivial (field
// extraction + the shared `area_size`), so it is inlined here rather than shared.
#[cfg(feature = "vulkan")]
use crate::render_helpers::vulkan::{VulkanError, VulkanFrame, VulkanRenderer};

#[cfg(feature = "vulkan")]
impl ShadowRenderElement {
    /// Build the shadow material's push constants from `params`, with the quad placed at `dst`.
    /// (`proj`/`target` are filled by `VulkanFrame::render_shadow`.)
    fn vulkan_push(&self, dst: Rectangle<i32, Physical>) -> niri_vk::render::ShadowPush {
        let p = &self.params;
        niri_vk::render::ShadowPush {
            origin: [dst.loc.x as f32, dst.loc.y as f32],
            size: [dst.size.w as f32, dst.size.h as f32],
            // proj/target are placeholders; VulkanFrame::render_shadow fills them from the frame.
            proj: niri_vk::render::IDENTITY_PROJ,
            target: [0.0, 0.0],
            sigma: p.sigma,
            niri_scale: p.scale,
            shadow_color: p.color.to_array_premul(),
            corner_radius: <[f32; 4]>::from(p.corner_radius),
            window_corner_radius: <[f32; 4]>::from(p.window_corner_radius),
            area_size: [p.size.w as f32, p.size.h as f32],
            geo_loc: [p.geometry.loc.x as f32, p.geometry.loc.y as f32],
            geo_size: [p.geometry.size.w as f32, p.geometry.size.h as f32],
            window_geo_loc: [
                p.window_geometry.loc.x as f32,
                p.window_geometry.loc.y as f32,
            ],
            window_geo_size: [
                p.window_geometry.size.w as f32,
                p.window_geometry.size.h as f32,
            ],
            niri_alpha: p.alpha,
            _pad0: 0.0,
        }
    }
}

#[cfg(feature = "vulkan")]
impl RenderElement<VulkanRenderer> for ShadowRenderElement {
    fn draw(
        &self,
        frame: &mut VulkanFrame<'_, '_>,
        _src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), VulkanError> {
        frame.render_shadow(self.vulkan_push(dst), dst, damage)
    }

    fn underlying_storage(&self, _renderer: &mut VulkanRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}
