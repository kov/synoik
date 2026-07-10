//! A texture render element clipped to a rounded rectangle.
//!
//! Same idea as [`super::clipped_surface::ClippedSurfaceRenderElement`], but for
//! our own [`TextureRenderElement`]s rather than Wayland surfaces: the fork's
//! GNOME overview rounds the workspace wallpaper corners (gnome-shell's
//! `BACKGROUND_CORNER_RADIUS_PIXELS`). With a zero radius it draws the plain
//! texture, no shader override.

use glam::{Mat3, Vec2};
use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::gles::{
    GlesError, GlesFrame, GlesRenderer, GlesTexProgram, GlesTexture, Uniform,
};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::backend::renderer::Texture;
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Logical, Physical, Rectangle, Scale, Transform};

use super::renderer::AsGlesFrame as _;
use super::shaders::{mat3_uniform, Shaders};
use super::texture::TextureRenderElement;
use crate::backend::tty::{TtyFrame, TtyRenderer, TtyRendererError};

/// Generic over the stored texture `T` so the same element can be built for GLES (`GlesTexture`,
/// the default — every bare mention across the enum definitions resolves to it unchanged) and for
/// the owned Vulkan renderer (`VkTexture`). GLES rounds via the `clipped_surface` shader carried in
/// `program`; the Vulkan draw rounds in its own pipeline and leaves `program` `None`.
#[derive(Debug, Clone)]
pub struct RoundedTextureRenderElement<T: Texture = GlesTexture> {
    inner: TextureRenderElement<T>,
    program: Option<GlesTexProgram>,
    /// Corner radius in the logical units of `geometry`.
    corner_radius: f32,
    /// The clip rectangle, in the same logical space the element is drawn in.
    geometry: Rectangle<f64, Logical>,
    scale: f32,
}

impl<T: Texture> RoundedTextureRenderElement<T> {
    fn rounds(&self) -> Option<&GlesTexProgram> {
        (self.corner_radius > 0.)
            .then_some(self.program.as_ref())
            .flatten()
    }
}

impl RoundedTextureRenderElement<GlesTexture> {
    pub fn new(
        renderer: &mut GlesRenderer,
        inner: TextureRenderElement<GlesTexture>,
        corner_radius: f64,
        geometry: Rectangle<f64, Logical>,
        scale: Scale<f64>,
    ) -> Self {
        // Missing shader (failed to compile) degrades to square corners.
        let program = Shaders::get(renderer)
            .expect("a GlesRenderer is always GLES-backed")
            .clipped_surface
            .clone();
        Self {
            inner,
            program,
            corner_radius: corner_radius as f32,
            geometry,
            scale: scale.x as f32,
        }
    }

    /// Same mapping as `ClippedSurfaceRenderElement::compute_uniforms`, in
    /// buffer coordinates. Memory-imported textures are never y-inverted, so
    /// that leg is dropped.
    fn compute_uniforms(&self) -> Vec<Uniform<'static>> {
        let scale = Scale::from(f64::from(self.scale));
        let elem_geo = self.inner.geometry(scale);

        let elem_geo_loc = Vec2::new(elem_geo.loc.x as f32, elem_geo.loc.y as f32);
        let elem_geo_size = Vec2::new(elem_geo.size.w as f32, elem_geo.size.h as f32);

        let geo = self.geometry.to_physical_precise_round(scale);
        let geo_loc = Vec2::new(geo.loc.x, geo.loc.y);
        let geo_size = Vec2::new(geo.size.w, geo.size.h);

        let buf_size = self.inner.buffer().texture().size();
        let buf_size = Vec2::new(buf_size.w as f32, buf_size.h as f32);

        let src = Element::src(&self.inner);
        let src_loc = Vec2::new(src.loc.x as f32, src.loc.y as f32);
        let src_size = Vec2::new(src.size.w as f32, src.size.h as f32);

        let transform = self.inner.transform();
        // Same flip as clipped_surface.rs.
        let transform = match transform {
            Transform::_90 => Transform::_270,
            Transform::_270 => Transform::_90,
            x => x,
        };
        let transform_matrix = Mat3::from_translation(Vec2::new(0.5, 0.5))
            * Mat3::from_cols_array(transform.matrix().as_ref())
            * Mat3::from_translation(-Vec2::new(0.5, 0.5));

        let input_to_geo = transform_matrix
            * Mat3::from_scale(elem_geo_size / geo_size)
            * Mat3::from_translation((elem_geo_loc - geo_loc) / elem_geo_size)
            * Mat3::from_scale(buf_size / src_size)
            * Mat3::from_translation(-src_loc / buf_size);

        let geo_size = (self.geometry.size.w as f32, self.geometry.size.h as f32);
        let radius = self.corner_radius;

        vec![
            Uniform::new("niri_scale", self.scale),
            Uniform::new("geo_size", geo_size),
            Uniform::new("corner_radius", [radius, radius, radius, radius]),
            mat3_uniform("input_to_geo", input_to_geo),
        ]
    }
}

impl<T: Texture> Element for RoundedTextureRenderElement<T> {
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
        // A non-zero radius cuts the corners transparent. Gate on the radius rather than
        // `rounds()`: the Vulkan draw rounds in its own pipeline with no GLES `program`, so
        // `rounds()` is always `None` there and would otherwise report the cut corners as opaque
        // (occlusion culling would then drop whatever peeks through the corners).
        if self.corner_radius > 0. {
            return OpaqueRegions::default();
        }
        self.inner.opaque_regions(scale)
    }

    fn alpha(&self) -> f32 {
        self.inner.alpha()
    }

    fn kind(&self) -> Kind {
        self.inner.kind()
    }
}

impl RenderElement<GlesRenderer> for RoundedTextureRenderElement<GlesTexture> {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        let Some(program) = self.rounds() else {
            return RenderElement::<GlesRenderer>::draw(
                &self.inner,
                frame,
                src,
                dst,
                damage,
                opaque_regions,
                cache,
            );
        };

        frame.override_default_tex_program(program.clone(), self.compute_uniforms());
        RenderElement::<GlesRenderer>::draw(
            &self.inner,
            frame,
            src,
            dst,
            damage,
            opaque_regions,
            cache,
        )?;
        frame.clear_tex_program_override();
        Ok(())
    }

    fn underlying_storage(&self, _renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}

impl<'render> RenderElement<TtyRenderer<'render>> for RoundedTextureRenderElement<GlesTexture> {
    fn draw(
        &self,
        frame: &mut TtyFrame<'render, '_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), TtyRendererError<'render>> {
        let gles_frame = frame.as_gles_frame();
        RenderElement::<GlesRenderer>::draw(
            &self,
            gles_frame,
            src,
            dst,
            damage,
            opaque_regions,
            cache,
        )?;
        Ok(())
    }

    fn underlying_storage(
        &self,
        _renderer: &mut TtyRenderer<'render>,
    ) -> Option<UnderlyingStorage<'_>> {
        None
    }
}

// The owned Vulkan renderer rounds a `VkTexture` in its own pipeline (M3). The `GlesTexture`
// specialization keeps its degraded no-op `RenderElement<VulkanRenderer>` (a `GlesTexture` cannot
// be sampled by Vulkan), so `OutputRenderElements<VulkanRenderer>`, which carries the
// `GlesTexture` variant, still composes; only elements built directly with a `VkTexture` (the
// M3 tests, and later the Vulkan wallpaper path) take this real draw.
#[cfg(feature = "vulkan")]
use crate::render_helpers::vulkan::{VkTexture, VulkanError, VulkanFrame, VulkanRenderer};

#[cfg(feature = "vulkan")]
impl RoundedTextureRenderElement<VkTexture> {
    /// Build a rounded-texture element for the owned Vulkan renderer, which rounds in its own
    /// pipeline and needs no GLES shader program.
    pub fn new_vulkan(
        inner: TextureRenderElement<VkTexture>,
        corner_radius: f64,
        geometry: Rectangle<f64, Logical>,
        scale: Scale<f64>,
    ) -> Self {
        Self {
            inner,
            program: None,
            corner_radius: corner_radius as f32,
            geometry,
            scale: scale.x as f32,
        }
    }
}

#[cfg(feature = "vulkan")]
impl RenderElement<VulkanRenderer> for RoundedTextureRenderElement<VkTexture> {
    fn draw(
        &self,
        frame: &mut VulkanFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), VulkanError> {
        // Zero radius: the corners aren't cut, so draw the plain texture (already correct on
        // Vulkan). `rounds()` is always `None` here (no GLES program), so gate on the radius.
        if self.corner_radius <= 0. {
            return RenderElement::<VulkanRenderer>::draw(
                &self.inner,
                frame,
                src,
                dst,
                damage,
                opaque_regions,
                cache,
            );
        }

        let texture = self.inner.buffer().texture();
        // `corner_radius` is in the logical units of `geometry`; the SDF pipeline works in physical
        // pixels of `dst`. `self.scale` converts logical→physical, but the element may sit under an
        // outer `RescaleRenderElement` (overview expose zoom, thumbnail strip) that scales `dst`
        // relative to the element's own geometry. The GLES shader is invariant to that outer
        // rescale (it maps in geometry space); match it by folding in the dst/geometry ratio so the
        // radius tracks the on-screen size. With no outer rescale (`dst == geometry`) this is 1.
        let geo_w = self
            .inner
            .geometry(Scale::from(f64::from(self.scale)))
            .size
            .w;
        let rescale = if geo_w != 0 {
            dst.size.w as f32 / geo_w as f32
        } else {
            1.
        };
        let radius_px = self.corner_radius * self.scale * rescale;
        let alpha = Element::alpha(&self.inner);
        frame.render_rounded_texture(texture, src, dst, radius_px, alpha)
    }

    fn underlying_storage(&self, _renderer: &mut VulkanRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}
