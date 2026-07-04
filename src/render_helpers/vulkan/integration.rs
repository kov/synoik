//! The niri-side renderer trait impls that make [`VulkanRenderer`] a [`NiriRenderer`]: the client
//! buffer imports ([`ImportMemWl`]/[`ImportEgl`]/[`ImportDma`]) and dmabuf-target [`Bind`], plus
//! the fallible GLES access ([`AsGlesRenderer`]).
//!
//! Client-buffer import and dmabuf scanout targets are **not implemented yet** — they return an
//! error. The offscreen render/readback path (M1) does not exercise them; real shm/dmabuf import is
//! the material tail (M3), and dmabuf render targets are Stage 3 (KMS scanout). Their presence here
//! is only what `NiriRenderer`'s supertraits require so a Vulkan renderer can be a `NiriRenderer`.

use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::egl::display::EGLBufferReader;
use smithay::backend::egl::Error as EglError;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::{Bind, ImportDma, ImportDmaWl, ImportEgl, ImportMemWl};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::utils::{Buffer as BufferCoord, Rectangle};
use smithay::wayland::compositor::SurfaceData;

use super::error::VulkanError;
use super::types::{VkFramebuffer, VkTexture};
use super::VulkanRenderer;
use crate::render_helpers::renderer::{AsGlesRenderer, OffscreenRenderer};

impl AsGlesRenderer for VulkanRenderer {
    fn try_as_gles_renderer(&mut self) -> Option<&mut GlesRenderer> {
        None
    }
}

impl OffscreenRenderer for VulkanRenderer {
    fn make_offscreen_sampleable(&self, texture: &VkTexture) -> anyhow::Result<()> {
        // Transition the just-rendered offscreen from TRANSFER_SRC_OPTIMAL to SHADER_READ_ONLY so a
        // later draw can sample it (the sampleable-offscreen bridge).
        self.make_sampleable(texture).map_err(Into::into)
    }

    fn offscreen_is_reusable(&self, texture: &mut VkTexture) -> bool {
        texture.is_unique_reference()
    }
}

impl Bind<Dmabuf> for VulkanRenderer {
    fn bind<'a>(&mut self, _target: &'a mut Dmabuf) -> Result<VkFramebuffer<'a>, VulkanError> {
        Err(VulkanError::Unsupported(
            "binding a dmabuf as a render target (KMS scanout is Stage 3)",
        ))
    }
}

impl ImportMemWl for VulkanRenderer {
    fn import_shm_buffer(
        &mut self,
        _buffer: &WlBuffer,
        _surface: Option<&SurfaceData>,
        _damage: &[Rectangle<i32, BufferCoord>],
    ) -> Result<VkTexture, VulkanError> {
        Err(VulkanError::Unsupported("shm client-buffer import"))
    }
}

impl ImportEgl for VulkanRenderer {
    fn bind_wl_display(&mut self, _display: &DisplayHandle) -> Result<(), EglError> {
        Err(EglError::NoEGLDisplayBound)
    }

    fn unbind_wl_display(&mut self) {}

    fn egl_reader(&self) -> Option<&EGLBufferReader> {
        None
    }

    fn import_egl_buffer(
        &mut self,
        _buffer: &WlBuffer,
        _surface: Option<&SurfaceData>,
        _damage: &[Rectangle<i32, BufferCoord>],
    ) -> Result<VkTexture, VulkanError> {
        Err(VulkanError::Unsupported("egl client-buffer import"))
    }
}

impl ImportDma for VulkanRenderer {
    fn import_dmabuf(
        &mut self,
        _dmabuf: &Dmabuf,
        _damage: Option<&[Rectangle<i32, BufferCoord>]>,
    ) -> Result<VkTexture, VulkanError> {
        Err(VulkanError::Unsupported("dmabuf client-buffer import"))
    }
}

impl ImportDmaWl for VulkanRenderer {}

/// Compile-time proof that [`VulkanRenderer`] now satisfies the (blanket) [`NiriRenderer`] core, so
/// it can flow through the generic render-element path.
const _: fn() = || {
    fn assert_niri_renderer<R: crate::render_helpers::renderer::NiriRenderer>() {}
    assert_niri_renderer::<VulkanRenderer>();
};
