use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::renderer::gles::{GlesFrame, GlesRenderer, GlesTexture};
use smithay::backend::renderer::{
    Bind, ExportMem, ImportAll, ImportMem, Renderer, RendererSuper, Texture,
};

use crate::backend::tty::{TtyFrame, TtyRenderer};

/// Core renderer requirements shared by every niri renderer, independent of the concrete graphics
/// API. This is what generic code that *builds* and *composites* render elements needs.
///
/// GLES-only capabilities are reached through the *fallible*
/// [`AsGlesRenderer::try_as_gles_renderer`] (which returns `None` for a non-GLES renderer), so a
/// renderer that isn't backed by GLES — the owned Vulkan renderer — can implement this core while
/// GLES-specific features degrade gracefully.
pub trait NiriRenderer:
    ImportAll
    + ImportMem
    + ExportMem
    + Bind<Dmabuf>
    + Renderer<TextureId = Self::NiriTextureId, Error = Self::NiriError>
    + AsGlesRenderer
    + AsVulkanRenderer
{
    // Associated types to work around the instability of associated type bounds.
    type NiriTextureId: Texture + Clone + Send + 'static;
    // The `From<GlesError>` bound is retained so generic code can still propagate GLES errors; a
    // non-GLES renderer satisfies it with a trivial conversion.
    type NiriError: std::error::Error
        + Send
        + Sync
        + From<<GlesRenderer as RendererSuper>::Error>
        + 'static;
}

impl<R> NiriRenderer for R
where
    R: ImportAll + ImportMem + ExportMem + Bind<Dmabuf> + AsGlesRenderer + AsVulkanRenderer,
    R::TextureId: Texture + Clone + Send + 'static,
    R::Error:
        std::error::Error + Send + Sync + From<<GlesRenderer as RendererSuper>::Error> + 'static,
{
    type NiriTextureId = R::TextureId;
    type NiriError = R::Error;
}

/// Fallible access to an underlying `GlesRenderer`.
///
/// GLES-backed renderers (`GlesRenderer` itself, and the Tty `MultiRenderer`) return `Some`; a
/// non-GLES renderer (the owned Vulkan renderer) returns `None`. Generic render code uses this to
/// gate GLES-only features — custom shaders, the GNOME wallpaper, xray, CPU→`GlesTexture` UI
/// uploads — degrading them (rendering nothing) rather than failing to compile on a Vulkan
/// renderer.
pub trait AsGlesRenderer {
    fn try_as_gles_renderer(&mut self) -> Option<&mut GlesRenderer>;

    /// Infallible access, for code paths only reached on GLES-backed renderers. **Panics** on a
    /// non-GLES renderer — generic render code that must also support the Vulkan renderer should
    /// use [`AsGlesRenderer::try_as_gles_renderer`] and degrade the GLES-only feature instead.
    fn as_gles_renderer(&mut self) -> &mut GlesRenderer {
        self.try_as_gles_renderer()
            .expect("this render path requires a GLES-backed renderer")
    }
}

impl AsGlesRenderer for GlesRenderer {
    fn try_as_gles_renderer(&mut self) -> Option<&mut GlesRenderer> {
        Some(self)
    }
}

impl AsGlesRenderer for TtyRenderer<'_> {
    fn try_as_gles_renderer(&mut self) -> Option<&mut GlesRenderer> {
        Some(self.as_mut())
    }
}

/// Fallible access to the owned `VulkanRenderer` — the dual of [`AsGlesRenderer`] for Vulkan-only
/// render paths (e.g. the resize crossfade, which builds `VkTexture`s and draws via
/// `VulkanFrame::render_resize`). Only the `VulkanRenderer` returns `Some`; GLES-backed renderers
/// return `None`, so a generic render path can specialize to Vulkan and degrade elsewhere.
pub trait AsVulkanRenderer {
    fn try_as_vulkan_renderer(
        &mut self,
    ) -> Option<&mut crate::render_helpers::vulkan::VulkanRenderer>;
}

impl AsVulkanRenderer for GlesRenderer {
    fn try_as_vulkan_renderer(
        &mut self,
    ) -> Option<&mut crate::render_helpers::vulkan::VulkanRenderer> {
        None
    }
}

impl AsVulkanRenderer for TtyRenderer<'_> {
    fn try_as_vulkan_renderer(
        &mut self,
    ) -> Option<&mut crate::render_helpers::vulkan::VulkanRenderer> {
        None
    }
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
/// be reused. GLES textures are immediately sampleable and expose `is_unique_reference` directly;
/// the owned Vulkan renderer inserts a layout barrier and checks its own `Arc` uniqueness.
///
/// Only implemented for the renderers that actually drive `OffscreenBuffer::render` —
/// `GlesRenderer` (the live path) and the owned `VulkanRenderer` (offscreen-verified). The Tty
/// `MultiRenderer` is never passed to it (its callers drop to GLES first), so it needs no impl.
pub trait OffscreenRenderer: Renderer {
    /// Prepare a just-rendered offscreen `texture` to be sampled. No-op on GLES (an FBO-backed
    /// texture is immediately sampleable); the Vulkan renderer transitions the image layout.
    fn make_offscreen_sampleable(&self, _texture: &Self::TextureId) -> anyhow::Result<()> {
        Ok(())
    }

    /// Whether a cached offscreen `texture` can be reused, i.e. no other live reference holds its
    /// GPU resources (a still-displayed snapshot from a previous frame must not be drawn over).
    fn offscreen_is_reusable(&self, texture: &mut Self::TextureId) -> bool;
}

impl OffscreenRenderer for GlesRenderer {
    fn offscreen_is_reusable(&self, texture: &mut GlesTexture) -> bool {
        texture.is_unique_reference()
    }
}

/// Trait for getting the underlying `GlesFrame`.
///
/// Only used by the concrete `RenderElement<TtyRenderer>` bridges (unwrapping the Tty
/// `MultiRenderer` frame to a `GlesFrame`), so it stays infallible.
pub trait AsGlesFrame<'frame, 'buffer>
where
    Self: 'frame,
{
    fn as_gles_frame(&mut self) -> &mut GlesFrame<'frame, 'buffer>;
}

impl<'frame, 'buffer> AsGlesFrame<'frame, 'buffer> for GlesFrame<'frame, 'buffer> {
    fn as_gles_frame(&mut self) -> &mut GlesFrame<'frame, 'buffer> {
        self
    }
}

impl<'frame, 'buffer> AsGlesFrame<'frame, 'buffer> for TtyFrame<'_, 'frame, 'buffer> {
    fn as_gles_frame(&mut self) -> &mut GlesFrame<'frame, 'buffer> {
        self.as_mut()
    }
}
