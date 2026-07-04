use std::fmt;
use std::sync::Arc;

use ash::vk;
use niri_vk::gpu::Gpu;
use niri_vk::texture::Texture as NiriTexture;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::{Texture, TextureMapping};

/// The one Vulkan color format this skeleton renders/imports/exports. `R8G8B8A8_UNORM` is
/// `[R,G,B,A]` in memory, i.e. DRM `ABGR8888` (and `XBGR8888` ignoring alpha).
pub(super) const IMAGE_VK_FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;

/// Whether `f` is one of the DRM fourccs whose byte order matches [`IMAGE_VK_FORMAT`].
pub(super) fn is_rgba8888(f: Fourcc) -> bool {
    matches!(f, Fourcc::Abgr8888 | Fourcc::Xbgr8888)
}

// --- VkTexture: a sampled texture id (Smithay `TextureId`) ------------------------------------

struct VkTextureInner {
    gpu: Arc<Gpu>,
    tex: NiriTexture,
    /// A one-set pool owned by this texture, so freeing the set can't outlive a shared pool.
    desc_pool: vk::DescriptorPool,
    set: vk::DescriptorSet,
    width: u32,
    height: u32,
    format: Fourcc,
    flipped: bool,
}

impl Drop for VkTextureInner {
    fn drop(&mut self) {
        unsafe {
            // Destroying the pool frees `set`; then the sampled image/view/sampler.
            self.gpu
                .device
                .destroy_descriptor_pool(self.desc_pool, None);
        }
        self.tex.destroy(&self.gpu);
    }
}

/// A sampled Vulkan texture. Cheap to clone (ref-counted); the last clone frees the GPU resources.
#[derive(Clone)]
pub struct VkTexture(Arc<VkTextureInner>);

impl VkTexture {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        gpu: Arc<Gpu>,
        tex: NiriTexture,
        desc_pool: vk::DescriptorPool,
        set: vk::DescriptorSet,
        width: u32,
        height: u32,
        format: Fourcc,
        flipped: bool,
    ) -> Self {
        VkTexture(Arc::new(VkTextureInner {
            gpu,
            tex,
            desc_pool,
            set,
            width,
            height,
            format,
            flipped,
        }))
    }

    /// The combined image-sampler descriptor set that binds this texture at set 0, binding 0.
    pub(super) fn descriptor_set(&self) -> vk::DescriptorSet {
        self.0.set
    }

    pub(super) fn flipped(&self) -> bool {
        self.0.flipped
    }
}

impl fmt::Debug for VkTexture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VkTexture")
            .field("width", &self.0.width)
            .field("height", &self.0.height)
            .field("format", &self.0.format)
            .field("flipped", &self.0.flipped)
            .finish()
    }
}

impl Texture for VkTexture {
    fn width(&self) -> u32 {
        self.0.width
    }
    fn height(&self) -> u32 {
        self.0.height
    }
    fn format(&self) -> Option<Fourcc> {
        Some(self.0.format)
    }
}

// --- VkRenderBuffer: an owned offscreen target (Smithay `Offscreen` target) --------------------

/// An owned offscreen color target: a device-local `COLOR_ATTACHMENT | TRANSFER_SRC` image plus a
/// render pass framebuffer over it. Produced by `Offscreen::create_buffer`, bound by `Bind`.
pub struct VkRenderBuffer {
    gpu: Arc<Gpu>,
    pub(super) image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    pub(super) framebuffer: vk::Framebuffer,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) format: Fourcc,
}

impl VkRenderBuffer {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        gpu: Arc<Gpu>,
        image: vk::Image,
        memory: vk::DeviceMemory,
        view: vk::ImageView,
        framebuffer: vk::Framebuffer,
        width: u32,
        height: u32,
        format: Fourcc,
    ) -> Self {
        VkRenderBuffer {
            gpu,
            image,
            memory,
            view,
            framebuffer,
            width,
            height,
            format,
        }
    }
}

impl Drop for VkRenderBuffer {
    fn drop(&mut self) {
        unsafe {
            let d = &self.gpu.device;
            d.destroy_framebuffer(self.framebuffer, None);
            d.destroy_image_view(self.view, None);
            d.destroy_image(self.image, None);
            d.free_memory(self.memory, None);
        }
    }
}

impl fmt::Debug for VkRenderBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VkRenderBuffer")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("format", &self.format)
            .finish()
    }
}

// --- VkFramebuffer: a bound target (Smithay `Framebuffer<'buffer>`) -----------------------------

/// A borrowed, in-use render target — Smithay's `Framebuffer`. Binding is zero-cost: it just
/// mutably borrows the [`VkRenderBuffer`] whose GPU framebuffer the frame renders into.
pub struct VkFramebuffer<'a> {
    pub(super) buffer: &'a mut VkRenderBuffer,
}

impl fmt::Debug for VkFramebuffer<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VkFramebuffer")
            .field("buffer", &self.buffer)
            .finish()
    }
}

impl Texture for VkFramebuffer<'_> {
    fn width(&self) -> u32 {
        self.buffer.width
    }
    fn height(&self) -> u32 {
        self.buffer.height
    }
    fn format(&self) -> Option<Fourcc> {
        Some(self.buffer.format)
    }
}

// --- VkMapping: a downloaded pixel buffer (Smithay `TextureMapping`) ----------------------------

/// A CPU-readable copy of a framebuffer/texture region — Smithay's `TextureMapping`. Owns the
/// bytes; `ExportMem::map_texture` hands out a borrowed slice of them.
pub struct VkMapping {
    pub(super) data: Vec<u8>,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) format: Fourcc,
}

impl fmt::Debug for VkMapping {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VkMapping")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("format", &self.format)
            .field("len", &self.data.len())
            .finish()
    }
}

impl Texture for VkMapping {
    fn width(&self) -> u32 {
        self.width
    }
    fn height(&self) -> u32 {
        self.height
    }
    fn format(&self) -> Option<Fourcc> {
        Some(self.format)
    }
}

impl TextureMapping for VkMapping {
    fn flipped(&self) -> bool {
        false
    }
}
