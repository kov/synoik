use std::collections::HashMap;
use std::rc::Rc;

use glam::{Mat3, Vec2};
use niri_config::CornerRadius;
use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::gles::{GlesError, GlesFrame, GlesRenderer, GlesTexture, Uniform};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::backend::renderer::Texture as _;
use smithay::gpu_span_location;
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Logical, Physical, Rectangle, Scale, Size, Transform};

use super::renderer::{AsGlesFrame, NiriRenderer};
use super::shader_element::ShaderRenderElement;
use super::shaders::{mat3_uniform, ProgramType, Shaders};
use crate::backend::tty::{TtyFrame, TtyRenderer, TtyRendererError};

/// The resize cross-fade element. Blends a "prev" (pre-resize) and "next" (current) window snapshot
/// by the animation progress, clipping/rounding to the current geometry.
///
/// On GLES/Tty it wraps a runtime-GLSL [`ShaderRenderElement`] (`ProgramType::Resize`). On the
/// owned Vulkan renderer it carries the two `VkTexture`s + a prebuilt `ResizePush` and draws
/// through [`VulkanFrame::render_resize`](super::vulkan::VulkanFrame::render_resize) — the same
/// transforms, packed affine-diagonal (see `niri-vk/shaders/resize.frag`). One enum, dispatched per
/// concrete renderer, mirroring the `BorderRenderElement` dual-impl pattern (but texture-carrying).
#[derive(Debug)]
pub enum ResizeRenderElement {
    Gles(ShaderRenderElement),
    #[cfg(feature = "vulkan")]
    Vulkan(VulkanResizeRenderElement),
}

/// Shared geometry/transform math for the resize crossfade, computed once and lowered to either the
/// GLES shader uniforms (mat3) or the Vulkan `ResizePush` (affine-diagonal). See the GLES
/// `resize.frag` / `niri-vk/shaders/resize.frag`: only `input_to_curr_geo`, `geo_to_tex_prev`,
/// `geo_to_tex_next` are sampled; `curr_geo_to_{prev,next}_geo` are dead uniforms kept for GLES
/// only.
struct ResizeTransforms {
    /// Encompassing area (loc + size) the quad covers, adjusted to fit both scaled snapshots.
    area: Rectangle<f64, Logical>,
    input_to_curr_geo: Mat3,
    curr_geo_to_prev_geo: Mat3,
    curr_geo_to_next_geo: Mat3,
    geo_to_tex_prev: Mat3,
    geo_to_tex_next: Mat3,
    curr_geo_size: Vec2,
    /// Corner radius fitted to `curr_geo_size`.
    corner_radius: CornerRadius,
    scale_x: f32,
}

#[allow(clippy::too_many_arguments)]
fn resize_transforms(
    area: Rectangle<f64, Logical>,
    scale: Scale<f64>,
    tex_prev_geo: Rectangle<i32, Physical>,
    tex_prev_size: Vec2,
    size_prev: Size<f64, Logical>,
    tex_next_geo: Rectangle<i32, Physical>,
    tex_next_size: Vec2,
    size_next: Size<f64, Logical>,
    corner_radius: CornerRadius,
) -> ResizeTransforms {
    let curr_geo = area;

    let scale_prev = area.size / size_prev;
    let scale_next = area.size / size_next;

    // Compute the area necessary to fit a crossfade.
    let tex_prev_geo_scaled = tex_prev_geo.to_f64().upscale(scale_prev);
    let tex_next_geo_scaled = tex_next_geo.to_f64().upscale(scale_next);
    let combined_geo = tex_prev_geo_scaled.merge(tex_next_geo_scaled).to_i32_up();

    let area = Rectangle::new(
        area.loc + combined_geo.loc.to_logical(scale),
        combined_geo.size.to_logical(scale),
    );

    // Convert Smithay types into glam types.
    let area_loc = Vec2::new(area.loc.x as f32, area.loc.y as f32);
    let area_size = Vec2::new(area.size.w as f32, area.size.h as f32);

    let curr_geo_loc = Vec2::new(curr_geo.loc.x as f32, curr_geo.loc.y as f32);
    let curr_geo_size = Vec2::new(curr_geo.size.w as f32, curr_geo.size.h as f32);

    let tex_prev_geo_loc = Vec2::new(tex_prev_geo.loc.x as f32, tex_prev_geo.loc.y as f32);
    let tex_next_geo_loc = Vec2::new(tex_next_geo.loc.x as f32, tex_next_geo.loc.y as f32);

    let size_prev = Vec2::new(size_prev.w as f32, size_prev.h as f32);
    let size_next = Vec2::new(size_next.w as f32, size_next.h as f32);

    let scale = Vec2::new(scale.x as f32, scale.y as f32);

    // Compute the transformation matrices.
    let input_to_curr_geo = Mat3::from_scale(area_size / curr_geo_size)
        * Mat3::from_translation((area_loc - curr_geo_loc) / area_size);

    let curr_geo_to_prev_geo = Mat3::from_scale(curr_geo_size / size_prev);
    let curr_geo_to_next_geo = Mat3::from_scale(curr_geo_size / size_next);

    let geo_to_tex_prev = Mat3::from_translation(-tex_prev_geo_loc / tex_prev_size)
        * Mat3::from_scale(size_prev / tex_prev_size * scale);
    let geo_to_tex_next = Mat3::from_translation(-tex_next_geo_loc / tex_next_size)
        * Mat3::from_scale(size_next / tex_next_size * scale);

    let corner_radius = corner_radius.fit_to(curr_geo_size.x, curr_geo_size.y);

    ResizeTransforms {
        area,
        input_to_curr_geo,
        curr_geo_to_prev_geo,
        curr_geo_to_next_geo,
        geo_to_tex_prev,
        geo_to_tex_next,
        curr_geo_size,
        corner_radius,
        scale_x: scale.x,
    }
}

impl ResizeRenderElement {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        area: Rectangle<f64, Logical>,
        scale: Scale<f64>,
        texture_prev: (GlesTexture, Rectangle<i32, Physical>),
        size_prev: Size<f64, Logical>,
        texture_next: (GlesTexture, Rectangle<i32, Physical>),
        size_next: Size<f64, Logical>,
        progress: f32,
        clamped_progress: f32,
        corner_radius: CornerRadius,
        clip_to_geometry: bool,
        result_alpha: f32,
    ) -> Self {
        let (texture_prev, tex_prev_geo) = texture_prev;
        let (texture_next, tex_next_geo) = texture_next;

        let t = resize_transforms(
            area,
            scale,
            tex_prev_geo,
            Vec2::new(texture_prev.width() as f32, texture_prev.height() as f32),
            size_prev,
            tex_next_geo,
            Vec2::new(texture_next.width() as f32, texture_next.height() as f32),
            size_next,
            corner_radius,
        );

        let clip_to_geometry = if clip_to_geometry { 1. } else { 0. };

        // Create the shader.
        Self::Gles(
            ShaderRenderElement::new(
                ProgramType::Resize,
                t.area.size,
                None,
                t.scale_x,
                result_alpha,
                Rc::new([
                    mat3_uniform("niri_input_to_curr_geo", t.input_to_curr_geo),
                    mat3_uniform("niri_curr_geo_to_prev_geo", t.curr_geo_to_prev_geo),
                    mat3_uniform("niri_curr_geo_to_next_geo", t.curr_geo_to_next_geo),
                    Uniform::new("niri_curr_geo_size", t.curr_geo_size.to_array()),
                    mat3_uniform("niri_geo_to_tex_prev", t.geo_to_tex_prev),
                    mat3_uniform("niri_geo_to_tex_next", t.geo_to_tex_next),
                    Uniform::new("niri_progress", progress),
                    Uniform::new("niri_clamped_progress", clamped_progress),
                    Uniform::new("niri_corner_radius", <[f32; 4]>::from(t.corner_radius)),
                    Uniform::new("niri_clip_to_geometry", clip_to_geometry),
                ]),
                HashMap::from([
                    (String::from("niri_tex_prev"), texture_prev),
                    (String::from("niri_tex_next"), texture_next),
                ]),
                Kind::Unspecified,
            )
            .with_location(t.area.loc),
        )
    }

    pub fn has_shader(renderer: &mut impl NiriRenderer) -> bool {
        Shaders::get(renderer).is_some_and(|s| s.program(ProgramType::Resize).is_some())
    }
}

impl Element for ResizeRenderElement {
    fn id(&self) -> &Id {
        match self {
            ResizeRenderElement::Gles(e) => e.id(),
            #[cfg(feature = "vulkan")]
            ResizeRenderElement::Vulkan(e) => &e.id,
        }
    }

    fn current_commit(&self) -> CommitCounter {
        match self {
            ResizeRenderElement::Gles(e) => e.current_commit(),
            #[cfg(feature = "vulkan")]
            ResizeRenderElement::Vulkan(e) => e.commit,
        }
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        match self {
            ResizeRenderElement::Gles(e) => e.geometry(scale),
            #[cfg(feature = "vulkan")]
            ResizeRenderElement::Vulkan(e) => e.area.to_physical_precise_round(scale),
        }
    }

    fn transform(&self) -> Transform {
        match self {
            ResizeRenderElement::Gles(e) => e.transform(),
            #[cfg(feature = "vulkan")]
            ResizeRenderElement::Vulkan(_) => Transform::Normal,
        }
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        match self {
            ResizeRenderElement::Gles(e) => e.src(),
            #[cfg(feature = "vulkan")]
            ResizeRenderElement::Vulkan(_) => Rectangle::from_size(Size::from((1., 1.))),
        }
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        match self {
            ResizeRenderElement::Gles(e) => e.damage_since(scale, commit),
            #[cfg(feature = "vulkan")]
            ResizeRenderElement::Vulkan(e) => {
                if commit != Some(e.commit) {
                    DamageSet::from_slice(&[e.area.to_physical_precise_round(scale)])
                } else {
                    DamageSet::default()
                }
            }
        }
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        match self {
            ResizeRenderElement::Gles(e) => e.opaque_regions(scale),
            #[cfg(feature = "vulkan")]
            ResizeRenderElement::Vulkan(_) => OpaqueRegions::default(),
        }
    }

    fn alpha(&self) -> f32 {
        match self {
            ResizeRenderElement::Gles(e) => e.alpha(),
            #[cfg(feature = "vulkan")]
            ResizeRenderElement::Vulkan(e) => e.alpha,
        }
    }

    fn kind(&self) -> Kind {
        match self {
            ResizeRenderElement::Gles(e) => e.kind(),
            #[cfg(feature = "vulkan")]
            ResizeRenderElement::Vulkan(e) => e.kind,
        }
    }
}

impl RenderElement<GlesRenderer> for ResizeRenderElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        // The `Vulkan` arm only exists with the `vulkan` feature; without it this is irrefutable.
        #[allow(irrefutable_let_patterns)]
        let ResizeRenderElement::Gles(inner) = self
        else {
            return Ok(());
        };
        let _span = tracy_client::span!("ResizeRenderElement::draw");
        frame.with_gpu_span(gpu_span_location!("ResizeRenderElement::draw"), |frame| {
            RenderElement::<GlesRenderer>::draw(
                inner,
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
        match self {
            ResizeRenderElement::Gles(e) => e.underlying_storage(renderer),
            #[cfg(feature = "vulkan")]
            ResizeRenderElement::Vulkan(_) => None,
        }
    }
}

impl<'render> RenderElement<TtyRenderer<'render>> for ResizeRenderElement {
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
        match self {
            ResizeRenderElement::Gles(e) => e.underlying_storage(renderer),
            #[cfg(feature = "vulkan")]
            ResizeRenderElement::Vulkan(_) => None,
        }
    }
}

#[cfg(feature = "vulkan")]
mod vulkan {
    use niri_vk::render::ResizePush;
    use smithay::backend::renderer::element::{Id, Kind, RenderElement, UnderlyingStorage};
    use smithay::backend::renderer::utils::CommitCounter;
    use smithay::backend::renderer::Texture as _;
    use smithay::utils::user_data::UserDataMap;
    use smithay::utils::{Buffer, Logical, Physical, Rectangle, Scale, Size};

    use super::{resize_transforms, ResizeRenderElement};
    use crate::render_helpers::vulkan::{VkTexture, VulkanError, VulkanFrame, VulkanRenderer};

    /// The Vulkan arm of [`ResizeRenderElement`]: two `VkTexture` snapshots + a prebuilt
    /// `ResizePush`, drawn via `VulkanFrame::render_resize`.
    #[derive(Debug)]
    pub struct VulkanResizeRenderElement {
        pub(super) id: Id,
        pub(super) commit: CommitCounter,
        pub(super) area: Rectangle<f64, Logical>,
        pub(super) alpha: f32,
        pub(super) kind: Kind,
        tex_prev: VkTexture,
        tex_next: VkTexture,
        push: ResizePush,
    }

    /// Extract the affine-diagonal `[scale.xy, translate.xy]` from a scale+translate `Mat3` (the
    /// resize transforms carry no rotation/shear). Mirrors `vulkan::custom::pack_affine`.
    fn pack_affine(m: glam::Mat3) -> [f32; 4] {
        [m.x_axis.x, m.y_axis.y, m.z_axis.x, m.z_axis.y]
    }

    impl ResizeRenderElement {
        #[allow(clippy::too_many_arguments)]
        pub fn new_vulkan(
            area: Rectangle<f64, Logical>,
            scale: Scale<f64>,
            texture_prev: (VkTexture, Rectangle<i32, Physical>),
            size_prev: Size<f64, Logical>,
            texture_next: (VkTexture, Rectangle<i32, Physical>),
            size_next: Size<f64, Logical>,
            clamped_progress: f32,
            corner_radius: niri_config::CornerRadius,
            clip_to_geometry: bool,
            result_alpha: f32,
        ) -> Self {
            let (tex_prev, tex_prev_geo) = texture_prev;
            let (tex_next, tex_next_geo) = texture_next;

            let t = resize_transforms(
                area,
                scale,
                tex_prev_geo,
                glam::Vec2::new(tex_prev.width() as f32, tex_prev.height() as f32),
                size_prev,
                tex_next_geo,
                glam::Vec2::new(tex_next.width() as f32, tex_next.height() as f32),
                size_next,
                corner_radius,
            );

            let push = ResizePush {
                curr_geo_size: t.curr_geo_size.to_array(),
                input_to_curr_geo: pack_affine(t.input_to_curr_geo),
                geo_to_tex_prev: pack_affine(t.geo_to_tex_prev),
                geo_to_tex_next: pack_affine(t.geo_to_tex_next),
                corner_radius: <[f32; 4]>::from(t.corner_radius),
                clamped_progress,
                clip_to_geometry: if clip_to_geometry { 1. } else { 0. },
                niri_scale: t.scale_x,
                niri_alpha: result_alpha,
                // origin/size/target are filled by render_resize.
                ..Default::default()
            };

            Self::Vulkan(VulkanResizeRenderElement {
                id: Id::new(),
                commit: CommitCounter::default(),
                area: t.area,
                alpha: result_alpha,
                kind: Kind::Unspecified,
                tex_prev,
                tex_next,
                push,
            })
        }
    }

    impl RenderElement<VulkanRenderer> for ResizeRenderElement {
        fn draw(
            &self,
            frame: &mut VulkanFrame<'_, '_>,
            _src: Rectangle<f64, Buffer>,
            dst: Rectangle<i32, Physical>,
            damage: &[Rectangle<i32, Physical>],
            _opaque_regions: &[Rectangle<i32, Physical>],
            _cache: Option<&UserDataMap>,
        ) -> Result<(), VulkanError> {
            let ResizeRenderElement::Vulkan(inner) = self else {
                return Ok(());
            };
            frame.render_resize(&inner.tex_prev, &inner.tex_next, dst, damage, inner.push)
        }

        fn underlying_storage(
            &self,
            _renderer: &mut VulkanRenderer,
        ) -> Option<UnderlyingStorage<'_>> {
            None
        }
    }
}

#[cfg(feature = "vulkan")]
pub use vulkan::VulkanResizeRenderElement;
