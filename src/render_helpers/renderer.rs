use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::renderer::{Bind, ExportMem, ImportAll, ImportMem, Renderer, Texture};

/// Core renderer requirements shared by every niri renderer, independent of the concrete graphics
/// API. This is what generic code that *builds* and *composites* render elements needs.
pub trait NiriRenderer:
    ImportAll
    + ImportMem
    + ExportMem
    + Bind<Dmabuf>
    + Renderer<TextureId = Self::NiriTextureId, Error = Self::NiriError>
    + AsVulkanRenderer
{
    // Associated types to work around the instability of associated type bounds.
    type NiriTextureId: Texture + Clone + Send + 'static;
    type NiriError: std::error::Error + Send + Sync + 'static;
}

impl<R> NiriRenderer for R
where
    R: ImportAll + ImportMem + ExportMem + Bind<Dmabuf> + AsVulkanRenderer,
    R::TextureId: Texture + Clone + Send + 'static,
    R::Error: std::error::Error + Send + Sync + 'static,
{
    type NiriTextureId = R::TextureId;
    type NiriError = R::Error;
}

/// Fallible access to the owned `VulkanRenderer`, for render paths that need the concrete renderer
/// rather than the generic core (e.g. the resize crossfade, which builds `VkTexture`s and draws via
/// `VulkanFrame::render_resize`).
///
/// Only the `VulkanRenderer` returns `Some` — and it is the only renderer left, so this is a
/// formality that disappears when the render tree stops being generic over `R`.
pub trait AsVulkanRenderer {
    fn try_as_vulkan_renderer(
        &mut self,
    ) -> Option<&mut crate::render_helpers::vulkan::VulkanRenderer>;
}

impl AsVulkanRenderer for crate::render_helpers::vulkan::VulkanRenderer {
    fn try_as_vulkan_renderer(
        &mut self,
    ) -> Option<&mut crate::render_helpers::vulkan::VulkanRenderer> {
        Some(self)
    }
}

/// A renderer that can produce **re-sampleable** offscreen snapshots (see
/// [`crate::render_helpers::offscreen::OffscreenBuffer`]): it knows how to make a freshly-rendered
/// offscreen texture readable by a later sampling draw, and whether a cached offscreen texture can
/// be reused. The owned Vulkan renderer inserts a layout barrier and checks its own `Arc`
/// uniqueness.
pub trait OffscreenRenderer: Renderer {
    /// Prepare a just-rendered offscreen `texture` to be sampled: the Vulkan renderer transitions
    /// the image layout.
    fn make_offscreen_sampleable(&self, _texture: &Self::TextureId) -> anyhow::Result<()> {
        Ok(())
    }

    /// Whether a cached offscreen `texture` can be reused, i.e. no other live reference holds its
    /// GPU resources (a still-displayed snapshot from a previous frame must not be drawn over).
    fn offscreen_is_reusable(&self, texture: &mut Self::TextureId) -> bool;
}
