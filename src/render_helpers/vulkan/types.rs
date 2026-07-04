use std::fmt;
use std::sync::atomic::{AtomicI32, Ordering};
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

// --- VkTexture: a sampled texture id (Smithay `TextureId`), optionally an offscreen target -------

struct VkTextureInner {
    gpu: Arc<Gpu>,
    tex: NiriTexture,
    /// A one-set pool owned by this texture, so freeing the set can't outlive a shared pool.
    desc_pool: vk::DescriptorPool,
    set: vk::DescriptorSet,
    /// Present iff this texture was created as an offscreen render target
    /// (`Offscreen::create_buffer`) — a render-pass framebuffer over `tex`'s image, so a frame
    /// can draw into it and it can then be sampled through `set`. Imported textures
    /// (`ImportMem`) leave this `None`.
    framebuffer: Option<vk::Framebuffer>,
    /// The image's current layout, tracked across the synchronous (fence-per-submit) lifecycle so
    /// [`super::VulkanRenderer::make_sampleable`] and readback can insert the right barrier.
    /// Stored as `vk::ImageLayout::as_raw()`; interior-mutable so it can be updated through a
    /// shared `&VkTexture` (sampling borrows the source immutably).
    layout: AtomicI32,
    width: u32,
    height: u32,
    format: Fourcc,
    flipped: bool,
}

impl Drop for VkTextureInner {
    fn drop(&mut self) {
        unsafe {
            let d = &self.gpu.device;
            // Destroying the pool frees `set`; then the render-pass framebuffer (offscreen only);
            // then the sampled image/view/sampler (via `tex.destroy`).
            d.destroy_descriptor_pool(self.desc_pool, None);
            if let Some(fb) = self.framebuffer {
                d.destroy_framebuffer(fb, None);
            }
        }
        self.tex.destroy(&self.gpu);
    }
}

/// A Vulkan texture: always sampleable (owns a combined image-sampler descriptor set), and — when
/// created via `Offscreen::create_buffer` — also a render target (owns a render-pass framebuffer).
/// Cheap to clone (ref-counted); the last clone frees the GPU resources.
#[derive(Clone)]
pub struct VkTexture(Arc<VkTextureInner>);

impl VkTexture {
    /// An imported (sampled-only) texture: no framebuffer, already in `SHADER_READ_ONLY_OPTIMAL`
    /// (its upload transitioned it there).
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
            framebuffer: None,
            layout: AtomicI32::new(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL.as_raw()),
            width,
            height,
            format,
            flipped,
        }))
    }

    /// An offscreen render target that is also sampleable: carries a render-pass `framebuffer` and
    /// starts in `UNDEFINED` layout (the render pass performs the first transition).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new_offscreen(
        gpu: Arc<Gpu>,
        tex: NiriTexture,
        desc_pool: vk::DescriptorPool,
        set: vk::DescriptorSet,
        framebuffer: vk::Framebuffer,
        width: u32,
        height: u32,
        format: Fourcc,
    ) -> Self {
        VkTexture(Arc::new(VkTextureInner {
            gpu,
            tex,
            desc_pool,
            set,
            framebuffer: Some(framebuffer),
            layout: AtomicI32::new(vk::ImageLayout::UNDEFINED.as_raw()),
            width,
            height,
            format,
            flipped: false,
        }))
    }

    /// The combined image-sampler descriptor set that binds this texture at set 0, binding 0.
    pub(super) fn descriptor_set(&self) -> vk::DescriptorSet {
        self.0.set
    }

    pub(super) fn flipped(&self) -> bool {
        self.0.flipped
    }

    /// The underlying color image (sampled source and, for offscreens, the render-pass attachment).
    pub(super) fn image(&self) -> vk::Image {
        self.0.tex.image
    }

    /// The render-pass framebuffer, iff this is an offscreen target. `None` for imported textures.
    pub(super) fn framebuffer(&self) -> Option<vk::Framebuffer> {
        self.0.framebuffer
    }

    pub(super) fn extent(&self) -> (u32, u32) {
        (self.0.width, self.0.height)
    }

    /// The image's tracked current layout.
    pub(super) fn layout(&self) -> vk::ImageLayout {
        vk::ImageLayout::from_raw(self.0.layout.load(Ordering::Acquire))
    }

    /// Record the image's new layout after a barrier / render pass transition.
    pub(super) fn set_layout(&self, layout: vk::ImageLayout) {
        self.0.layout.store(layout.as_raw(), Ordering::Release);
    }
}

impl fmt::Debug for VkTexture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VkTexture")
            .field("width", &self.0.width)
            .field("height", &self.0.height)
            .field("format", &self.0.format)
            .field("flipped", &self.0.flipped)
            .field("offscreen", &self.0.framebuffer.is_some())
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

// --- VkFramebuffer: a bound target (Smithay `Framebuffer<'buffer>`) -----------------------------

/// A borrowed, in-use render target — Smithay's `Framebuffer`. Binding is zero-cost: it just
/// mutably borrows the offscreen [`VkTexture`] whose GPU framebuffer the frame renders into.
pub struct VkFramebuffer<'a> {
    pub(super) buffer: &'a mut VkTexture,
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
        self.buffer.width()
    }
    fn height(&self) -> u32 {
        self.buffer.height()
    }
    fn format(&self) -> Option<Fourcc> {
        self.buffer.format()
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
