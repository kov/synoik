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

/// Map a 32-bpp DRM fourcc to the VkFormat that reproduces its byte order when sampled, plus
/// whether the fourth byte is `X` (undefined) and alpha must be forced to 1.0. Vulkan non-PACK
/// formats name channels in memory-byte order, so the client's bytes upload verbatim (no CPU
/// swizzle) and a sampler returns correct RGBA regardless of texture format. Both VkFormats are
/// mandatory SAMPLED_IMAGE formats, so no runtime feature query is needed. This is broader than
/// [`is_rgba8888`] (which gates the RGBA-order render targets / readback) because clients most
/// commonly send ARGB/XRGB (BGRA byte order).
pub(super) fn import_format(f: Fourcc) -> Option<(vk::Format, bool)> {
    match f {
        Fourcc::Argb8888 => Some((vk::Format::B8G8R8A8_UNORM, false)),
        Fourcc::Xrgb8888 => Some((vk::Format::B8G8R8A8_UNORM, true)),
        Fourcc::Abgr8888 => Some((vk::Format::R8G8B8A8_UNORM, false)),
        Fourcc::Xbgr8888 => Some((vk::Format::R8G8B8A8_UNORM, true)),
        _ => None,
    }
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

    /// A dmabuf-backed scanout render target: like [`Self::new_offscreen`] but carries **no**
    /// descriptor set (a scanout buffer is rendered into and handed to KMS, never sampled), so its
    /// `NiriTexture` need not — and, being `COLOR_ATTACHMENT`-only, must not — advertise `SAMPLED`
    /// usage. The null `desc_pool`/`set` are valid no-ops to destroy. Starts `UNDEFINED`; the
    /// render pass performs the first transition.
    pub(super) fn new_dmabuf_target(
        gpu: Arc<Gpu>,
        tex: NiriTexture,
        framebuffer: vk::Framebuffer,
        width: u32,
        height: u32,
        format: Fourcc,
    ) -> Self {
        VkTexture(Arc::new(VkTextureInner {
            gpu,
            tex,
            desc_pool: vk::DescriptorPool::null(),
            set: vk::DescriptorSet::null(),
            framebuffer: Some(framebuffer),
            layout: AtomicI32::new(vk::ImageLayout::UNDEFINED.as_raw()),
            width,
            height,
            format,
            flipped: false,
        }))
    }

    /// A dmabuf-backed **present-blit** target: the scanout buffer whose byte order (`Argb8888`/
    /// `Xrgb8888` → `B8G8R8A8`) differs from the renderer's RGBA render pass, so it is never
    /// rendered into directly — [`super::VulkanFrame::finish`] blits the R8G8B8A8 shadow into it
    /// (reordering bytes). Carries no framebuffer (not a render target) and no descriptor set
    /// (never sampled). Starts `UNDEFINED`; the blit barriers transition it.
    pub(super) fn new_present_target(
        gpu: Arc<Gpu>,
        tex: NiriTexture,
        width: u32,
        height: u32,
        format: Fourcc,
    ) -> Self {
        VkTexture(Arc::new(VkTextureInner {
            gpu,
            tex,
            desc_pool: vk::DescriptorPool::null(),
            set: vk::DescriptorSet::null(),
            framebuffer: None,
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

    /// Whether this is the only handle to the underlying GPU resources (no other clone is live) —
    /// the [`smithay::backend::renderer::gles::GlesTexture::is_unique_reference`] equivalent, used
    /// by `OffscreenBuffer` to decide whether a cached offscreen texture can be reused.
    pub fn is_unique_reference(&mut self) -> bool {
        Arc::get_mut(&mut self.0).is_some()
    }

    /// The underlying color image (sampled source and, for offscreens, the render-pass attachment).
    pub(super) fn image(&self) -> vk::Image {
        self.0.tex.image
    }

    /// The underlying niri-vk texture (image + view + sampler), for low-level pipelines that sample
    /// it directly — e.g. the blur chain, which needs its `view`/`sampler`.
    pub(super) fn niri_texture(&self) -> &NiriTexture {
        &self.0.tex
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

    /// Re-upload a full frame of tightly-packed pixels into this texture's existing image, reusing
    /// `staging` (the caller must have written the data into it). The shm-cache hit path: refresh a
    /// cached texture in place with no allocation. The image ends in `SHADER_READ_ONLY_OPTIMAL`.
    pub(super) fn reupload_shm(
        &self,
        pool: vk::CommandPool,
        staging: &niri_vk::texture::Staging,
    ) -> anyhow::Result<()> {
        self.0.tex.reupload_full(&self.0.gpu, pool, staging)?;
        self.set_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        Ok(())
    }

    /// Whether two handles refer to the *same* underlying texture (ref-counted inner). Used by the
    /// dmabuf-import-cache test to prove a cache hit returns the reused image, not a fresh import.
    #[cfg(test)]
    pub(super) fn same_image(&self, other: &VkTexture) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }

    /// Record the deferred re-acquire barrier for this cached dmabuf-import into `cbuf` (a frame's
    /// command buffer), then advance the tracked layout. The client wrote new content into the same
    /// shared dmabuf, so before sampling again we re-take ownership from `FOREIGN` and invalidate
    /// the sampler caches. The barrier is *recorded, not submitted*: it rides the frame's single
    /// submit (see [`super::renderer::VulkanRenderer::record_pending_dmabuf_acquires`]). No
    /// allocation — reuses the imported image, view and descriptor set. Reads the current tracked
    /// layout at record time. The image ends in `SHADER_READ_ONLY_OPTIMAL`. See
    /// [`niri_vk::texture::Texture::record_reacquire_dmabuf_sampled`].
    pub(super) fn record_reacquire_dmabuf(&self, cbuf: vk::CommandBuffer) {
        self.0
            .tex
            .record_reacquire_dmabuf_sampled(&self.0.gpu, cbuf, self.layout());
        self.set_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
    }
}

// The shm texture cache stores `VkTexture` in the surface's `data_map`, which requires the value be
// `Send + Sync`. (All the Vulkan handles it holds are `u64`/atomics; `Gpu`'s dispatch tables are
// `Send + Sync`.) Pin it so a future field can't silently break the cache.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<VkTexture>();
};

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

/// An in-use render target — Smithay's `Framebuffer`. Holds the target [`VkTexture`] as a cheap
/// ref-counted clone rather than a borrow: a `Framebuffer` must be able to outlive the `bind` call,
/// and for a dmabuf scanout target the caller owns only the `Dmabuf` (not a `VkTexture`), so there
/// is nothing to borrow — the renderer imports one and the framebuffer keeps it alive (cf. Pixman's
/// refcounted `Framebuffer` image). The lifetime is a marker so this still fits Smithay's
/// `Framebuffer<'buffer>` GAT. Cloning is an `Arc` bump; the underlying GPU image is shared, so
/// rendering through the clone writes the same image the caller (offscreen) or dmabuf reads back.
pub struct VkFramebuffer<'a> {
    pub(super) buffer: VkTexture,
    /// Set on the **present-blit** scanout path (KMS planes that want `Argb8888`/`Xrgb8888`, whose
    /// byte order differs from the renderer's RGBA render pass): `buffer` is then an R8G8B8A8
    /// shadow rendered into as usual, and this is the imported dmabuf that
    /// [`super::VulkanFrame::finish`] blits the shadow into (reordering bytes RGBA→BGRA).
    /// `None` for direct render targets (offscreen textures, or RGBA-order dmabufs rendered
    /// into in place).
    pub(super) present: Option<VkTexture>,
    pub(super) _marker: std::marker::PhantomData<&'a mut ()>,
}

impl VkFramebuffer<'_> {
    pub(super) fn new(buffer: VkTexture) -> Self {
        VkFramebuffer {
            buffer,
            present: None,
            _marker: std::marker::PhantomData,
        }
    }

    /// A present-blit framebuffer: render into `buffer` (an R8G8B8A8 shadow), then blit into
    /// `present` (the imported scanout dmabuf) on `finish`. See [`Self::present`].
    pub(super) fn new_with_present(buffer: VkTexture, present: VkTexture) -> Self {
        VkFramebuffer {
            buffer,
            present: Some(present),
            _marker: std::marker::PhantomData,
        }
    }
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
        // On the present-blit path the true target is the scanout dmabuf (`Argb8888`/`Xrgb8888`),
        // not the R8G8B8A8 shadow rendered into — report the format callers can actually scan out.
        self.present
            .as_ref()
            .map_or_else(|| self.buffer.format(), |p| p.format())
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
