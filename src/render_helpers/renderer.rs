use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::renderer::gles::{GlesFrame, GlesRenderer};
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
    R: ImportAll + ImportMem + ExportMem + Bind<Dmabuf> + AsGlesRenderer,
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
