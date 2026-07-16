//! A texture render element clipped to a rounded rectangle.
//!
//! Same idea as [`super::clipped_surface::ClippedSurfaceRenderElement`], but for
//! our own [`TextureRenderElement`]s rather than Wayland surfaces: the fork's
//! GNOME overview rounds the workspace wallpaper corners (gnome-shell's
//! `BACKGROUND_CORNER_RADIUS_PIXELS`). With a zero radius it draws the plain
//! texture, no shader override.

use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::backend::renderer::Texture;
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Physical, Rectangle, Scale, Transform};

use super::texture::TextureRenderElement;

/// Generic over the stored texture `T`. The renderer rounds in its own pipeline.
///
/// The rounding always applies to the element's own quad. The GLES draw this replaces took a
/// separate clip rectangle and mapped into it, but every caller passed the element's own geometry,
/// and the pipeline here rounds `dst` — so the parameter is gone rather than silently ignored.
#[derive(Debug, Clone)]
pub struct RoundedTextureRenderElement<T: Texture> {
    inner: TextureRenderElement<T>,
    /// Corner radius, in logical units.
    corner_radius: f32,
    scale: f32,
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
        // A non-zero radius cuts the corners transparent; reporting them opaque would let occlusion
        // culling drop whatever peeks through.
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

use crate::render_helpers::vulkan::{VkTexture, VulkanError, VulkanFrame, VulkanRenderer};

impl RoundedTextureRenderElement<VkTexture> {
    pub fn new(
        inner: TextureRenderElement<VkTexture>,
        corner_radius: f64,
        scale: Scale<f64>,
    ) -> Self {
        Self {
            inner,
            corner_radius: corner_radius as f32,
            scale: scale.x as f32,
        }
    }
}

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
        // Zero radius: the corners aren't cut, so draw the plain texture.
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
        let src_transform = Element::transform(&self.inner);
        frame.render_rounded_texture(texture, src, dst, damage, src_transform, radius_px, alpha)
    }

    fn underlying_storage(&self, _renderer: &mut VulkanRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}
