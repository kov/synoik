// SPDX-License-Identifier: GPL-3.0-only
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::backend::renderer::Texture;
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Physical, Rectangle, Scale, Transform};

use super::texture::TextureRenderElement;

/// Generic over the stored texture `T`, mirroring
/// [`super::rounded_texture::RoundedTextureRenderElement`]. The renderer fades in its own pipeline.
#[derive(Debug, Clone)]
pub struct GradientFadeTextureRenderElement<T: Texture> {
    inner: TextureRenderElement<T>,
    cutoff: (f32, f32),
}

impl<T: Texture> GradientFadeTextureRenderElement<T> {
    /// The horizontal fade band `(left, right)` in the sampled texture's u coordinate — `(1, 1)`
    /// (no fade) when the texture is shown full-width, else a fade near the clipped right edge.
    /// Depends only on the buffer/src widths, so it is renderer-agnostic.
    fn compute_cutoff(texture: &TextureRenderElement<T>) -> (f32, f32) {
        let logical_w = texture.buffer().logical_size().w;
        let logical_src_w = texture.logical_src().size.w;
        if logical_src_w < logical_w {
            // Texture is clipped, add a fade.
            let cutoff = 1. - f64::min(18. / logical_src_w, 1.);
            let full = logical_src_w / logical_w;
            ((cutoff * full) as f32, full as f32)
        } else {
            // Texture is displayed full-size, no cutoff necessary.
            (1., 1.)
        }
    }
}

impl<T: Texture> Element for GradientFadeTextureRenderElement<T> {
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

use crate::render_helpers::vulkan::{VkTexture, VulkanError, VulkanFrame, VulkanRenderer};

impl GradientFadeTextureRenderElement<VkTexture> {
    pub fn new(texture: TextureRenderElement<VkTexture>) -> Self {
        let cutoff = Self::compute_cutoff(&texture);
        Self {
            inner: texture,
            cutoff,
        }
    }
}

impl RenderElement<VulkanRenderer> for GradientFadeTextureRenderElement<VkTexture> {
    fn draw(
        &self,
        frame: &mut VulkanFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), VulkanError> {
        let texture = self.inner.buffer().texture();
        let alpha = Element::alpha(&self.inner);
        let src_transform = Element::transform(&self.inner);
        frame.render_gradient_fade(texture, src, dst, damage, src_transform, self.cutoff, alpha)
    }

    fn underlying_storage(&self, _renderer: &mut VulkanRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}
