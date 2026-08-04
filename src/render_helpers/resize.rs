use glam::{Mat3, Vec2};
use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::backend::renderer::Texture as _;
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Logical, Physical, Rectangle, Scale, Size, Transform};
use synoik_config::CornerRadius;
use synoik_vk::render::ResizePush;

use crate::render_helpers::vulkan::{
    CustomResizePush, VkTexture, VulkanError, VulkanFrame, VulkanRenderer,
};

/// The resize cross-fade element. Blends a "prev" (pre-resize) and "next" (current) window snapshot
/// by the animation progress, clipping/rounding to the current geometry.
///
/// Carries the two `VkTexture`s plus a prebuilt push and draws through
/// [`VulkanFrame::render_resize`](crate::render_helpers::vulkan::VulkanFrame::render_resize), with
/// the transforms packed affine-diagonal (see `synoik-vk/shaders/resize.frag`).
#[derive(Debug)]
pub struct ResizeRenderElement {
    id: Id,
    commit: CommitCounter,
    area: Rectangle<f64, Logical>,
    alpha: f32,
    kind: Kind,
    tex_prev: VkTexture,
    tex_next: VkTexture,
    push: ResizePushKind,
}

/// Which resize material the element draws: the built-in crossfade (`render_resize`) or the user's
/// custom resize shader (`render_custom_resize`), chosen at construction by whether a custom resize
/// shader is installed.
#[derive(Debug)]
enum ResizePushKind {
    Builtin(ResizePush),
    Custom(CustomResizePush),
}

/// Shared geometry/transform math for the resize crossfade, computed once and lowered to the
/// `ResizePush` (affine-diagonal). See `synoik-vk/shaders/resize.frag`: only `input_to_curr_geo`,
/// `geo_to_tex_prev` and `geo_to_tex_next` are sampled by the built-in crossfade;
/// `curr_geo_to_{prev,next}_geo` are for the custom shader only.
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

/// Extract the affine-diagonal `[scale.xy, translate.xy]` from a scale+translate `Mat3` (the resize
/// transforms carry no rotation/shear). Mirrors `vulkan::custom::pack_affine`.
fn pack_affine(m: glam::Mat3) -> [f32; 4] {
    [m.x_axis.x, m.y_axis.y, m.z_axis.x, m.z_axis.y]
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
        texture_prev: (VkTexture, Rectangle<i32, Physical>),
        size_prev: Size<f64, Logical>,
        texture_next: (VkTexture, Rectangle<i32, Physical>),
        size_next: Size<f64, Logical>,
        progress: f32,
        clamped_progress: f32,
        corner_radius: CornerRadius,
        clip_to_geometry: bool,
        result_alpha: f32,
        use_custom: bool,
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

        let clip_to_geometry = if clip_to_geometry { 1. } else { 0. };
        let push = if use_custom {
            // The user's custom resize shader: the extra curr_geo_to_{prev,next}_geo matrices
            // and unclamped progress the built-in crossfade doesn't use.
            // origin/size/target/proj are filled by render_custom_resize.
            ResizePushKind::Custom(CustomResizePush {
                curr_geo_size: t.curr_geo_size.to_array(),
                input_to_curr_geo: pack_affine(t.input_to_curr_geo),
                curr_geo_to_prev_geo: pack_affine(t.curr_geo_to_prev_geo),
                curr_geo_to_next_geo: pack_affine(t.curr_geo_to_next_geo),
                geo_to_tex_prev: pack_affine(t.geo_to_tex_prev),
                geo_to_tex_next: pack_affine(t.geo_to_tex_next),
                corner_radius: <[f32; 4]>::from(t.corner_radius),
                progress,
                clamped_progress,
                clip_to_geometry,
                alpha: result_alpha,
                scale: t.scale_x,
                ..Default::default()
            })
        } else {
            ResizePushKind::Builtin(ResizePush {
                curr_geo_size: t.curr_geo_size.to_array(),
                input_to_curr_geo: pack_affine(t.input_to_curr_geo),
                geo_to_tex_prev: pack_affine(t.geo_to_tex_prev),
                geo_to_tex_next: pack_affine(t.geo_to_tex_next),
                corner_radius: <[f32; 4]>::from(t.corner_radius),
                clamped_progress,
                clip_to_geometry,
                synoik_scale: t.scale_x,
                synoik_alpha: result_alpha,
                // origin/size/target are filled by render_resize.
                ..Default::default()
            })
        };

        Self {
            id: Id::new(),
            commit: CommitCounter::default(),
            area: t.area,
            alpha: result_alpha,
            kind: Kind::Unspecified,
            tex_prev,
            tex_next,
            push,
        }
    }
}

impl Element for ResizeRenderElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.commit
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.area.to_physical_precise_round(scale)
    }

    fn transform(&self) -> Transform {
        Transform::Normal
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        Rectangle::from_size(Size::from((1., 1.)))
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        if commit != Some(self.commit) {
            DamageSet::from_slice(&[self.area.to_physical_precise_round(scale)])
        } else {
            DamageSet::default()
        }
    }

    fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        OpaqueRegions::default()
    }

    fn alpha(&self) -> f32 {
        self.alpha
    }

    fn kind(&self) -> Kind {
        self.kind
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
        match &self.push {
            ResizePushKind::Builtin(push) => {
                frame.render_resize(&self.tex_prev, &self.tex_next, dst, damage, *push)
            }
            ResizePushKind::Custom(push) => {
                frame.render_custom_resize(&self.tex_prev, &self.tex_next, dst, damage, *push)
            }
        }
    }

    fn underlying_storage(&self, _renderer: &mut VulkanRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}
