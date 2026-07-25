//! A sampled texture: device-local image uploaded from host RGBA via a staging buffer, plus a
//! view and sampler. This is the infrastructure both the blur passes and the glyph atlas reuse.

use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd};
use std::sync::Arc;

use anyhow::{Context, Result};
use ash::vk;

use crate::gpu::Gpu;
use crate::render::{COLOR_RANGE, FORMAT};

pub struct Texture {
    pub image: vk::Image,
    pub view: vk::ImageView,
    pub sampler: vk::Sampler,
    memory: vk::DeviceMemory,
    // Used by the blur (ping-pong sizing) and glyph-atlas milestones.
    #[allow(dead_code)]
    pub width: u32,
    #[allow(dead_code)]
    pub height: u32,
}

/// Unwind guard for [`Texture::upload`]: destroys every handle created so far if a later step
/// fails, so a mid-build error (e.g. a HOST_VISIBLE staging allocation failing under the Venus
/// mappable-blob pressure the shm cache targets) doesn't orphan the resource. Each field starts
/// null (`vkDestroy*`/`vkFree*` no-op on null) and is filled as `upload` progresses;
/// `staging`/`smem` are always freed (they never outlive `upload`), while the
/// image/memory/view/sampler that move into the finished `Texture` are nulled out of the guard
/// first so it leaves them alone. Mirrors the [`crate::blur::BlurChain`] `NewGuard` pattern.
struct UploadGuard<'a> {
    device: &'a ash::Device,
    staging: vk::Buffer,
    smem: vk::DeviceMemory,
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    sampler: vk::Sampler,
}

impl<'a> UploadGuard<'a> {
    fn new(device: &'a ash::Device) -> Self {
        Self {
            device,
            staging: vk::Buffer::null(),
            smem: vk::DeviceMemory::null(),
            image: vk::Image::null(),
            memory: vk::DeviceMemory::null(),
            view: vk::ImageView::null(),
            sampler: vk::Sampler::null(),
        }
    }
}

impl Drop for UploadGuard<'_> {
    fn drop(&mut self) {
        let d = self.device;
        unsafe {
            if self.sampler != vk::Sampler::null() {
                d.destroy_sampler(self.sampler, None);
            }
            if self.view != vk::ImageView::null() {
                d.destroy_image_view(self.view, None);
            }
            if self.image != vk::Image::null() {
                d.destroy_image(self.image, None);
            }
            if self.memory != vk::DeviceMemory::null() {
                d.free_memory(self.memory, None);
            }
            if self.staging != vk::Buffer::null() {
                d.destroy_buffer(self.staging, None);
            }
            if self.smem != vk::DeviceMemory::null() {
                d.free_memory(self.smem, None);
            }
        }
    }
}

/// Unwind guard for the image-building constructors ([`Texture::new_color_target`] and the two
/// dmabuf imports): frees the image/memory/view/sampler created so far if a later step fails —
/// `vkAllocateMemory` under the Venus mappable-blob pressure these paths hit, the FOREIGN-acquire
/// submit, or a view/sampler create — so a mid-build error doesn't orphan them. Each field starts
/// null (`vkDestroy*`/`vkFree*` no-op on null) and is filled as the constructor progresses; on
/// success [`Self::disarm`] nulls the four handles that move into the finished `Texture` so the
/// guard leaves them alone. Free order matches [`Texture::destroy`] (sampler→view→image→memory).
/// The sibling [`UploadGuard`] is the same guard plus the host staging buffer only
/// `Texture::upload` builds; kept separate so neither carries the other's irrelevant fields.
struct TextureGuard<'a> {
    device: &'a ash::Device,
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    sampler: vk::Sampler,
}

impl<'a> TextureGuard<'a> {
    fn new(device: &'a ash::Device) -> Self {
        Self {
            device,
            image: vk::Image::null(),
            memory: vk::DeviceMemory::null(),
            view: vk::ImageView::null(),
            sampler: vk::Sampler::null(),
        }
    }

    /// Disarm on success: the four handles now belong to the returned `Texture`, so the guard must
    /// not free them.
    fn disarm(&mut self) {
        self.image = vk::Image::null();
        self.memory = vk::DeviceMemory::null();
        self.view = vk::ImageView::null();
        self.sampler = vk::Sampler::null();
    }
}

impl Drop for TextureGuard<'_> {
    fn drop(&mut self) {
        let d = self.device;
        unsafe {
            if self.sampler != vk::Sampler::null() {
                d.destroy_sampler(self.sampler, None);
            }
            if self.view != vk::ImageView::null() {
                d.destroy_image_view(self.view, None);
            }
            if self.image != vk::Image::null() {
                d.destroy_image(self.image, None);
            }
            if self.memory != vk::DeviceMemory::null() {
                d.free_memory(self.memory, None);
            }
        }
    }
}

/// A texture built up to the point of its GPU copy: the device-local [`Texture`] plus the host
/// staging buffer holding its pixels, with the staging→image copy not yet recorded. Produced by
/// [`Texture::build_pending`] and consumed once the copy has been submitted (the caller frees the
/// staging then). Lets the single and batched upload paths share resource creation.
struct PendingUpload {
    staging: vk::Buffer,
    smem: vk::DeviceMemory,
    texture: Texture,
}

/// Uploads many textures with a single submit + fence-wait instead of one per texture — the
/// overview app grid decodes ~24 icons and would otherwise pay ~24 serialized queue round-trips on
/// first open (a real stutter on virtualized/Venus queues). Usage: [`TextureBatch::new`], then
/// [`upload`](Self::upload) per texture (creates its resources, no submit), then
/// [`finish`](Self::finish) once (records every copy into one command buffer, submits, waits, and
/// returns the textures). Dropping a batch without `finish` (an error/panic mid-build) frees every
/// resource staged so far.
pub struct TextureBatch<'a> {
    gpu: &'a Gpu,
    pool: vk::CommandPool,
    pending: Vec<PendingUpload>,
}

/// One texture staged into a shared batch staging buffer: its device-local resources, and where
/// its pixels sit in that buffer. The batched sibling of [`PendingUpload`], which owns a staging
/// buffer of its own.
struct BatchedUpload {
    texture: Texture,
    buffer_offset: vk::DeviceSize,
}

/// Build every texture in `items` against **one** staging buffer and one submit.
///
/// The per-texture path ([`Texture::build_pending`]) creates a staging buffer, allocates its
/// memory, binds, maps and unmaps for each texture — five host round trips apiece, on top of the
/// image, its memory, the view, the sampler, the descriptor pool and the set. On a virtualized
/// driver those round trips are the dominant cost of an upload: the seat's app-grid open measured
/// **26 resources created in 28.24 ms** with only 11.32 ms of fence waits, i.e. creation was 65% of
/// the frame and 2.5× everything spent waiting on the GPU.
///
/// [`TextureBatch`] already collapsed N submits into one. This collapses the *staging* half too,
/// the same way the glyph atlas does it: one buffer, every region concatenated, `buffer_offset`
/// picking each out. The device-local image and its descriptor set stay per texture.
///
/// Each item is `(tight w*h*bpp bytes, width, height, format, bpp, components, filter)`; the
/// returned textures are in the same order. On any error every resource built so far is freed.
pub fn upload_batch_shared_staging(
    gpu: &Gpu,
    pool: vk::CommandPool,
    items: &[BatchItem<'_>],
) -> Result<Vec<Texture>> {
    if items.is_empty() {
        return Ok(Vec::new());
    }
    let device = &gpu.device;
    let mut guard = UploadGuard::new(device);

    // --- one staging buffer for every texture's pixels, concatenated ---
    let total: vk::DeviceSize = items.iter().map(|i| i.data.len() as vk::DeviceSize).sum();
    crate::stats::uploaded(total);
    let (staging, smem) = {
        let _timed = crate::stats::creating();
        let ci = vk::BufferCreateInfo::default()
            .size(total)
            .usage(vk::BufferUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let staging = unsafe { device.create_buffer(&ci, None) }.context("batch staging")?;
        guard.staging = staging;
        let sreq = unsafe { device.get_buffer_memory_requirements(staging) };
        let smem = gpu.allocate(
            sreq,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )?;
        guard.smem = smem;
        unsafe { device.bind_buffer_memory(staging, smem, 0) }?;
        (staging, smem)
    };

    let mut staged: Vec<BatchedUpload> = Vec::with_capacity(items.len());
    unsafe {
        let base = device
            .map_memory(smem, 0, total, vk::MemoryMapFlags::empty())
            .context("map batch staging")? as *mut u8;
        let mut offset: vk::DeviceSize = 0;
        for item in items {
            {
                let _timed = crate::stats::staging_write();
                std::ptr::copy_nonoverlapping(
                    item.data.as_ptr(),
                    base.add(offset as usize),
                    item.data.len(),
                );
            }
            offset += item.data.len() as vk::DeviceSize;
        }
        device.unmap_memory(smem);
    }

    // --- the device-local image + view + sampler + descriptor-visible resources, per texture ---
    let mut offset: vk::DeviceSize = 0;
    for item in items {
        match Texture::new_sampled_image(gpu, item) {
            Ok(texture) => {
                staged.push(BatchedUpload {
                    texture,
                    buffer_offset: offset,
                });
                offset += item.data.len() as vk::DeviceSize;
            }
            Err(err) => {
                for s in &staged {
                    s.texture.destroy(gpu);
                }
                return Err(err);
            }
        }
    }

    let result = gpu.run_commands(pool, crate::stats::SubmitSite::UploadBatch, |cbuf| unsafe {
        for s in &staged {
            record_upload_copy_at(
                device,
                cbuf,
                s.texture.image,
                staging,
                s.buffer_offset,
                s.texture.width,
                s.texture.height,
            );
        }
    });

    // The staging has served its purpose either way — `run_commands` waited, and drained the
    // device on a wait error, so nothing can still be reading it.
    unsafe {
        device.destroy_buffer(staging, None);
        device.free_memory(smem, None);
    }
    guard.staging = vk::Buffer::null();
    guard.smem = vk::DeviceMemory::null();

    match result {
        Ok(()) => Ok(staged.into_iter().map(|s| s.texture).collect()),
        Err(err) => {
            for s in &staged {
                s.texture.destroy(gpu);
            }
            Err(err)
        }
    }
}

/// One texture's worth of input to [`upload_batch_shared_staging`].
pub struct BatchItem<'a> {
    pub data: &'a [u8],
    pub width: u32,
    pub height: u32,
    pub format: vk::Format,
    pub components: vk::ComponentMapping,
    pub filter: vk::Filter,
}

impl<'a> TextureBatch<'a> {
    pub fn new(gpu: &'a Gpu, pool: vk::CommandPool) -> Self {
        Self {
            gpu,
            pool,
            pending: Vec::new(),
        }
    }

    /// Stage one texture: create its staging buffer + image/view/sampler and copy the pixels into
    /// the staging, but record no commands and submit nothing (that happens in `finish`). The data
    /// must be tight `width*height*bpp` bytes, matching [`Texture::upload`].
    #[allow(clippy::too_many_arguments)]
    pub fn upload(
        &mut self,
        width: u32,
        height: u32,
        data: &[u8],
        format: vk::Format,
        bpp: vk::DeviceSize,
        components: vk::ComponentMapping,
        filter: vk::Filter,
    ) -> Result<()> {
        let pending = Texture::build_pending(
            self.gpu, width, height, data, format, bpp, components, filter,
        )?;
        self.pending.push(pending);
        Ok(())
    }

    /// Record every staged copy into one command buffer, submit once, wait once, then free the
    /// staging buffers and return the finished textures (in `upload` order). On a submit/wait
    /// failure every staged texture is destroyed and the error is returned.
    pub fn finish(mut self) -> Result<Vec<Texture>> {
        let device = &self.gpu.device;
        // Drain so this batch's `Drop` (which runs when `self` falls out of scope below) sees an
        // empty list and frees nothing a second time. (A panic between here and the frees below
        // would leak the drained resources — the ash calls all return `Result`, so the only real
        // candidate is an allocation abort, which tears the process down anyway.)
        let pending = std::mem::take(&mut self.pending);
        if pending.is_empty() {
            return Ok(Vec::new());
        }

        let result = self.gpu.run_commands(
            self.pool,
            crate::stats::SubmitSite::UploadBatch,
            |cbuf| unsafe {
                for p in &pending {
                    record_upload_copy(
                        device,
                        cbuf,
                        p.texture.image,
                        p.staging,
                        p.texture.width,
                        p.texture.height,
                    );
                }
            },
        );

        // The staging buffers have served their purpose (or the submit failed — `run_commands`
        // already drained the device on a wait error, so this can't race an in-flight copy).
        for p in &pending {
            unsafe {
                device.destroy_buffer(p.staging, None);
                device.free_memory(p.smem, None);
            }
        }

        match result {
            Ok(()) => Ok(pending.into_iter().map(|p| p.texture).collect()),
            Err(err) => {
                for p in &pending {
                    p.texture.destroy(self.gpu);
                }
                Err(err)
            }
        }
    }
}

impl Drop for TextureBatch<'_> {
    fn drop(&mut self) {
        // Non-empty only when the batch was abandoned before `finish` (an early `?` or a panic);
        // free every resource staged so far. Nothing was submitted, so no copy is in flight.
        let device = &self.gpu.device;
        for p in self.pending.drain(..) {
            unsafe {
                device.destroy_buffer(p.staging, None);
                device.free_memory(p.smem, None);
            }
            p.texture.destroy(self.gpu);
        }
    }
}

impl Texture {
    /// Upload tight `width*height` RGBA8 pixels into a shader-readable texture.
    pub fn from_rgba(
        gpu: &Gpu,
        pool: vk::CommandPool,
        width: u32,
        height: u32,
        rgba: &[u8],
        filter: vk::Filter,
    ) -> Result<Self> {
        Self::upload(
            gpu,
            pool,
            width,
            height,
            rgba,
            FORMAT,
            4,
            vk::ComponentMapping::default(),
            filter,
        )
    }

    /// Upload tight `width*height` 32bpp pixels in an arbitrary byte order, selected by `format`
    /// (e.g. `B8G8R8A8_UNORM` for wl_shm ARGB, `R8G8B8A8_UNORM` for ABGR). The VkFormat defines the
    /// channel interpretation, so a sampler returns correct RGBA with no CPU swizzle. `alpha_one`
    /// forces the sampled alpha to 1.0 via the view (for X-formats whose fourth byte is undefined).
    #[allow(clippy::too_many_arguments)]
    pub fn from_bytes_32bpp(
        gpu: &Gpu,
        pool: vk::CommandPool,
        width: u32,
        height: u32,
        data: &[u8],
        format: vk::Format,
        alpha_one: bool,
        filter: vk::Filter,
    ) -> Result<Self> {
        let components = if alpha_one {
            vk::ComponentMapping::default().a(vk::ComponentSwizzle::ONE)
        } else {
            vk::ComponentMapping::default()
        };
        Self::upload(
            gpu, pool, width, height, data, format, 4, components, filter,
        )
    }

    /// Upload tight `width*height` single-channel coverage (R8) — the glyph atlas format.
    pub fn from_coverage(
        gpu: &Gpu,
        pool: vk::CommandPool,
        width: u32,
        height: u32,
        coverage: &[u8],
        filter: vk::Filter,
    ) -> Result<Self> {
        Self::upload(
            gpu,
            pool,
            width,
            height,
            coverage,
            vk::Format::R8_UNORM,
            1,
            vk::ComponentMapping::default(),
            filter,
        )
    }

    /// A blank transfer-only image in an arbitrary `format`: `TRANSFER_DST | TRANSFER_SRC`,
    /// device-local, `OPTIMAL` tiling, and **no view or sampler** (it is never rendered into nor
    /// sampled — only blitted into and copied out of).
    ///
    /// This is the readback conversion staging image: blit an `R8G8B8A8` frame into a `B8G8R8A8`
    /// one and `vkCmdBlitImage` reorders the channels for us, so a BGRA consumer (an `Xrgb8888` shm
    /// pool, say) needs no CPU swizzle. `OPTIMAL` tiling matters: the spec's
    /// required-format-support tables mandate `BLIT_SRC | BLIT_DST` for both formats there, so
    /// the blit is guaranteed on any conformant driver rather than resting on what Venus
    /// happens to allow.
    ///
    /// `Texture::destroy` runs `vkDestroySampler`/`vkDestroyImageView` on the null handles, which
    /// is a defined no-op.
    pub fn new_transfer_image(
        gpu: &Gpu,
        width: u32,
        height: u32,
        format: vk::Format,
    ) -> Result<Self> {
        let device = &gpu.device;

        let image_ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);

        let mut guard = TextureGuard::new(device);
        let image =
            unsafe { device.create_image(&image_ci, None) }.context("transfer-image image")?;
        guard.image = image;
        let ireq = unsafe { device.get_image_memory_requirements(image) };
        let memory = gpu.allocate(ireq, vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
        guard.memory = memory;
        unsafe { device.bind_image_memory(image, memory, 0)? };

        guard.disarm();
        Ok(Self {
            image,
            view: vk::ImageView::null(),
            sampler: vk::Sampler::null(),
            memory,
            width,
            height,
        })
    }

    /// Create a blank `width*height` color target that is **both** renderable and sampleable: a
    /// device-local `COLOR_ATTACHMENT | SAMPLED | TRANSFER_SRC` image plus a view and sampler, with
    /// no upload. The image is left in `UNDEFINED` layout — the caller's render pass performs the
    /// first transition (its `initial_layout` is `UNDEFINED`, discarding the blank contents), and a
    /// later barrier moves it to `SHADER_READ_ONLY_OPTIMAL` when it is re-sampled. This is the
    /// offscreen-snapshot / blur / clipped-surface bridge: render into it, then sample it.
    pub fn new_color_target(
        gpu: &Gpu,
        width: u32,
        height: u32,
        filter: vk::Filter,
    ) -> Result<Self> {
        let device = &gpu.device;

        let image_ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(FORMAT)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            // TRANSFER_SRC for readback; TRANSFER_DST so it can also receive a blit/copy (e.g. the
            // blurred output lifted out of the blur chain).
            .usage(
                vk::ImageUsageFlags::COLOR_ATTACHMENT
                    | vk::ImageUsageFlags::SAMPLED
                    | vk::ImageUsageFlags::TRANSFER_SRC
                    | vk::ImageUsageFlags::TRANSFER_DST,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let mut guard = TextureGuard::new(device);
        let image =
            unsafe { device.create_image(&image_ci, None) }.context("color-target image")?;
        guard.image = image;
        let ireq = unsafe { device.get_image_memory_requirements(image) };
        let memory = gpu.allocate(ireq, vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
        guard.memory = memory;
        unsafe { device.bind_image_memory(image, memory, 0)? };

        let view_ci = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(FORMAT)
            .subresource_range(COLOR_RANGE);
        let view =
            unsafe { device.create_image_view(&view_ci, None) }.context("color-target view")?;
        guard.view = view;

        let sampler_ci = vk::SamplerCreateInfo::default()
            .mag_filter(filter)
            .min_filter(filter)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE);
        let sampler = unsafe { device.create_sampler(&sampler_ci, None) }.context("sampler")?;
        guard.sampler = sampler;

        guard.disarm();
        Ok(Texture {
            image,
            view,
            sampler,
            memory,
            width,
            height,
        })
    }

    /// Import a foreign (GBM-allocated) dmabuf as a **renderable** color target: a
    /// `COLOR_ATTACHMENT | TRANSFER_SRC` `VkImage` whose memory is the dmabuf, with an explicit DRM
    /// format modifier + plane layout. This is the KMS-scanout dual of [`crate::dmabuf`]'s sampled
    /// import (Stage 1) — render a frame into it and the pixels land in the dmabuf for scanout (or,
    /// in tests, a CPU map of the LINEAR buffer). Single plane only (all our formats are); the
    /// render pass performs the `UNDEFINED`→attachment transition, so no pre-acquire barrier is
    /// needed for the LINEAR modifier (nothing to detile). `TRANSFER_SRC` lets a test read it back.
    ///
    /// `fd` is duplicated internally (Vulkan consumes the dup on a successful import; the caller
    /// keeps ownership of the original). `format` must match `fourcc`'s byte order (e.g.
    /// `R8G8B8A8_UNORM` for `Abgr8888`/`Xbgr8888`, `B8G8R8A8_UNORM` for `Argb8888`/`Xrgb8888`).
    /// `usage` selects the role: `COLOR_ATTACHMENT | TRANSFER_SRC` to render straight into the
    /// dmabuf (RGBA-order scanout), or `COLOR_ATTACHMENT | TRANSFER_DST` for a present-blit target
    /// that receives a byte-reordering blit (the `Argb8888`/`Xrgb8888` KMS path).
    #[allow(clippy::too_many_arguments)]
    pub fn import_dmabuf_render_target(
        gpu: &Gpu,
        width: u32,
        height: u32,
        fd: BorrowedFd<'_>,
        offset: u32,
        stride: u32,
        modifier: u64,
        format: vk::Format,
        usage: vk::ImageUsageFlags,
        filter: vk::Filter,
    ) -> Result<Self> {
        let device = &gpu.device;

        let plane_layout = vk::SubresourceLayout {
            offset: offset as u64,
            size: 0,
            row_pitch: stride as u64,
            array_pitch: 0,
            depth_pitch: 0,
        };
        let mut mod_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(modifier)
            .plane_layouts(std::slice::from_ref(&plane_layout));
        let mut ext_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let image_ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut ext_info)
            .push_next(&mut mod_info);
        let mut guard = TextureGuard::new(device);
        let image = unsafe { device.create_image(&image_ci, None) }
            .context("create dmabuf render-target image")?;
        guard.image = image;

        let mem_req = unsafe { device.get_image_memory_requirements(image) };

        // Prefer the memory types the driver reports valid for this fd, but treat the query as
        // best-effort — on Venus it can reject a perfectly importable dmabuf (see `dmabuf.rs`).
        let ext_fd = ash::khr::external_memory_fd::Device::new(&gpu.instance, &gpu.device);
        let mut type_bits = mem_req.memory_type_bits;
        let mut fd_props = vk::MemoryFdPropertiesKHR::default();
        if let Ok(()) = unsafe {
            // The query borrows the fd; it does NOT consume it (unlike the import below), so pass a
            // plain borrow — duping here would leak one fd per call (and this runs per bind).
            ext_fd.get_memory_fd_properties(
                vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                fd.as_raw_fd(),
                &mut fd_props,
            )
        } {
            type_bits &= fd_props.memory_type_bits;
        }
        anyhow::ensure!(type_bits != 0, "no importable memory type for the dmabuf");
        let mem_type = type_bits.trailing_zeros();

        // Import consumes the fd on success; hand Vulkan a fresh dup and let the caller keep
        // theirs.
        let raw_fd = fd
            .try_clone_to_owned()
            .context("dup dmabuf fd for import")?
            .into_raw_fd();
        let mut import_info = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(raw_fd);
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_req.size)
            .memory_type_index(mem_type)
            .push_next(&mut import_info)
            .push_next(&mut dedicated);
        let memory = unsafe { device.allocate_memory(&alloc_info, None) }.map_err(|e| {
            // On failure Vulkan did not take the fd; close our dup so it doesn't leak.
            drop(unsafe { OwnedFd::from_raw_fd(raw_fd) });
            anyhow::anyhow!("import dmabuf render-target memory (vkAllocateMemory): {e}")
        })?;
        guard.memory = memory;
        unsafe { device.bind_image_memory(image, memory, 0) }
            .context("bind imported render-target memory")?;

        // Only an image whose usage has a view-capable bit may have a view at all
        // (VUID-VkImageViewCreateInfo-image-04441). The present-blit target is imported
        // transfer-only — it is blitted into and read back, never attached or sampled (the shadow
        // is the attachment) — so a view on it is both invalid and useless: a blit takes images,
        // not views. Leave it null, exactly as `new_transfer_image` does; `Texture::destroy` treats
        // a null view as a defined no-op.
        const VIEWABLE: vk::ImageUsageFlags = vk::ImageUsageFlags::from_raw(
            vk::ImageUsageFlags::COLOR_ATTACHMENT.as_raw()
                | vk::ImageUsageFlags::SAMPLED.as_raw()
                | vk::ImageUsageFlags::STORAGE.as_raw()
                | vk::ImageUsageFlags::INPUT_ATTACHMENT.as_raw(),
        );
        let view = if usage.intersects(VIEWABLE) {
            let view_ci = vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(format)
                .subresource_range(COLOR_RANGE);
            let view = unsafe { device.create_image_view(&view_ci, None) }
                .context("dmabuf render-target view")?;
            guard.view = view;
            view
        } else {
            vk::ImageView::null()
        };

        // Sampler is unused (a scanout target is never sampled) but kept so this fits `Texture`.
        let sampler_ci = vk::SamplerCreateInfo::default()
            .mag_filter(filter)
            .min_filter(filter)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE);
        let sampler = unsafe { device.create_sampler(&sampler_ci, None) }.context("sampler")?;
        guard.sampler = sampler;

        guard.disarm();
        Ok(Texture {
            image,
            view,
            sampler,
            memory,
            width,
            height,
        })
    }

    /// Import a foreign (client-allocated) dmabuf as a **sampled** texture: a `SAMPLED` `VkImage`
    /// whose memory is the dmabuf, with an explicit DRM format modifier + single-plane layout, a
    /// view (optionally forcing sampled alpha to 1.0 for the `X`-formats via a component swizzle),
    /// and a sampler. This is the client-buffer dual of [`Self::import_dmabuf_render_target`]: the
    /// compositor samples it when compositing the window.
    ///
    /// Unlike the render-target import, the producer is outside Vulkan, so an explicit acquire
    /// barrier transitions `UNDEFINED`→`SHADER_READ_ONLY_OPTIMAL`, transferring ownership from the
    /// `FOREIGN` queue family when `VK_EXT_queue_family_foreign` is present. For the LINEAR
    /// modifier the bytes survive the `UNDEFINED` old layout (nothing to detile). Single plane
    /// only.
    ///
    /// `fd` is duplicated internally (Vulkan consumes the dup on success; the caller keeps the
    /// original). `format` must match `fourcc`'s byte order (e.g. `B8G8R8A8_UNORM` for
    /// `Argb8888`/`Xrgb8888`, `R8G8B8A8_UNORM` for `Abgr8888`/`Xbgr8888`).
    #[allow(clippy::too_many_arguments)]
    pub fn import_dmabuf_sampled(
        gpu: &Gpu,
        pool: vk::CommandPool,
        width: u32,
        height: u32,
        fd: BorrowedFd<'_>,
        offset: u32,
        stride: u32,
        modifier: u64,
        format: vk::Format,
        alpha_one: bool,
        filter: vk::Filter,
    ) -> Result<Self> {
        let device = &gpu.device;

        let plane_layout = vk::SubresourceLayout {
            offset: offset as u64,
            size: 0,
            row_pitch: stride as u64,
            array_pitch: 0,
            depth_pitch: 0,
        };
        let mut mod_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(modifier)
            .plane_layouts(std::slice::from_ref(&plane_layout));
        let mut ext_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let image_ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut ext_info)
            .push_next(&mut mod_info);
        let mut guard = TextureGuard::new(device);
        let image = unsafe { device.create_image(&image_ci, None) }
            .context("create sampled dmabuf image")?;
        guard.image = image;

        let mem_req = unsafe { device.get_image_memory_requirements(image) };

        // Prefer the memory types the driver reports valid for this fd, but treat the query as
        // best-effort — on Venus it can reject a perfectly importable dmabuf (see `dmabuf.rs`).
        let ext_fd = ash::khr::external_memory_fd::Device::new(&gpu.instance, &gpu.device);
        let mut type_bits = mem_req.memory_type_bits;
        let mut fd_props = vk::MemoryFdPropertiesKHR::default();
        if let Ok(()) = unsafe {
            ext_fd.get_memory_fd_properties(
                vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                fd.as_raw_fd(),
                &mut fd_props,
            )
        } {
            type_bits &= fd_props.memory_type_bits;
        }
        anyhow::ensure!(type_bits != 0, "no importable memory type for the dmabuf");
        let mem_type = type_bits.trailing_zeros();

        // Import consumes the fd on success; hand Vulkan a fresh dup and let the caller keep
        // theirs.
        let raw_fd = fd
            .try_clone_to_owned()
            .context("dup dmabuf fd for import")?
            .into_raw_fd();
        let mut import_info = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(raw_fd);
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_req.size)
            .memory_type_index(mem_type)
            .push_next(&mut import_info)
            .push_next(&mut dedicated);
        let memory = unsafe { device.allocate_memory(&alloc_info, None) }.map_err(|e| {
            // On failure Vulkan did not take the fd; close our dup so it doesn't leak.
            drop(unsafe { OwnedFd::from_raw_fd(raw_fd) });
            anyhow::anyhow!("import sampled dmabuf memory (vkAllocateMemory): {e}")
        })?;
        guard.memory = memory;
        unsafe { device.bind_image_memory(image, memory, 0) }
            .context("bind imported sampled memory")?;

        // Acquire the content from the outside-Vulkan producer and transition it for sampling.
        // Transfer ownership from the FOREIGN queue family when that extension is present, else a
        // plain layout transition. For the LINEAR modifier the bytes survive `UNDEFINED`.
        let (src_qf, dst_qf) = if gpu.supports("VK_EXT_queue_family_foreign") {
            (vk::QUEUE_FAMILY_FOREIGN_EXT, gpu.queue_family)
        } else {
            (vk::QUEUE_FAMILY_IGNORED, vk::QUEUE_FAMILY_IGNORED)
        };
        gpu.run_commands(pool, crate::stats::SubmitSite::Transition, |cbuf| unsafe {
            let barrier = vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_queue_family_index(src_qf)
                .dst_queue_family_index(dst_qf)
                .image(image)
                .subresource_range(COLOR_RANGE);
            device.cmd_pipeline_barrier(
                cbuf,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier],
            );
        })
        .context("acquire imported sampled image")?;

        let components = if alpha_one {
            vk::ComponentMapping::default().a(vk::ComponentSwizzle::ONE)
        } else {
            vk::ComponentMapping::default()
        };
        let view_ci = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .components(components)
            .subresource_range(COLOR_RANGE);
        let view =
            unsafe { device.create_image_view(&view_ci, None) }.context("sampled dmabuf view")?;
        guard.view = view;

        let sampler_ci = vk::SamplerCreateInfo::default()
            .mag_filter(filter)
            .min_filter(filter)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE);
        let sampler = unsafe { device.create_sampler(&sampler_ci, None) }
            .context("sampled dmabuf sampler")?;
        guard.sampler = sampler;

        guard.disarm();
        Ok(Texture {
            image,
            view,
            sampler,
            memory,
            width,
            height,
        })
    }

    /// Record the re-acquire barrier for an already-imported sampled dmabuf into an existing
    /// `cbuf` — the caller owns the submit. This is the barrier-recording half of
    /// [`Self::reacquire_dmabuf_sampled`]; the compositor records it directly into a frame's
    /// command buffer (before that frame's render pass) so the acquire rides the frame's single
    /// submit instead of a per-commit standalone submit + fence-wait.
    ///
    /// The client renders new content into the *same* underlying dmabuf every frame (this image's
    /// memory IS that shared dmabuf), so a cache that reuses the [`Self::import_dmabuf_sampled`]
    /// image across commits must, before sampling it again, (a) invalidate our sampler's caches so
    /// it re-reads the shared memory and (b) re-take queue-family ownership from `FOREIGN`. This is
    /// an ownership *acquire* + cache-visibility barrier ONLY: it does **not** synchronize with the
    /// producer's GPU writes (a pipeline barrier cannot observe a foreign GPU context). The
    /// producer's writes are guaranteed to have already landed in the shared memory UPSTREAM, by
    /// the commit-time acquire blocker (`linux-drm-syncobj-v1` timeline point / implicit-fence
    /// poll); see the renderer's `import_dmabuf_as_texture`.
    ///
    /// When `VK_EXT_queue_family_foreign` is present we keep the image's *current* tracked
    /// `old_layout` (`SHADER_READ_ONLY_OPTIMAL`) rather than `UNDEFINED`: `UNDEFINED` licenses the
    /// driver to discard/re-lay-out contents, harmless on this VM's LINEAR-only path but corrupting
    /// for a tiled/CCS modifier. Without the extension there is no `FOREIGN` ownership transfer, so
    /// a same-layout barrier with an empty source scope would be a cache-invalidation no-op and
    /// sample stale content; we instead force an `UNDEFINED`→`SHADER_READ_ONLY_OPTIMAL` transition,
    /// which does invalidate the read caches and is safe because only LINEAR buffers ever reach the
    /// cache (their bytes survive `UNDEFINED`). Either way we deliberately skip the paired
    /// ours→`FOREIGN` *release* after the previous frame's last sample: a full ownership round-trip
    /// is spec-correct but tolerated to skip for LINEAR (revisit if tiled modifiers are ever
    /// imported). The image ends in `SHADER_READ_ONLY_OPTIMAL`. Callers pass the tracked current
    /// layout as `old_layout`.
    pub fn record_reacquire_dmabuf_sampled(
        &self,
        gpu: &Gpu,
        cbuf: vk::CommandBuffer,
        old_layout: vk::ImageLayout,
    ) {
        let device = &gpu.device;
        let (src_qf, dst_qf, old_layout) = if gpu.supports("VK_EXT_queue_family_foreign") {
            (vk::QUEUE_FAMILY_FOREIGN_EXT, gpu.queue_family, old_layout)
        } else {
            (
                vk::QUEUE_FAMILY_IGNORED,
                vk::QUEUE_FAMILY_IGNORED,
                vk::ImageLayout::UNDEFINED,
            )
        };
        unsafe {
            let barrier = vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .old_layout(old_layout)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_queue_family_index(src_qf)
                .dst_queue_family_index(dst_qf)
                .image(self.image)
                .subresource_range(COLOR_RANGE);
            device.cmd_pipeline_barrier(
                cbuf,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier],
            );
        }
    }

    /// Standalone variant of [`Self::record_reacquire_dmabuf_sampled`]: records the acquire barrier
    /// on its own command buffer and submits it. Kept as a self-contained niri-vk API for callers
    /// that re-acquire outside a frame; the compositor's hot path uses the `record_*` variant to
    /// fold the barrier into the frame submit. See that method for the full rationale.
    pub fn reacquire_dmabuf_sampled(
        &self,
        gpu: &Gpu,
        pool: vk::CommandPool,
        old_layout: vk::ImageLayout,
    ) -> Result<()> {
        gpu.run_commands(pool, crate::stats::SubmitSite::Transition, |cbuf| {
            self.record_reacquire_dmabuf_sampled(gpu, cbuf, old_layout)
        })
        .context("re-acquire imported sampled image")
    }

    /// Upload `data` (tight `width*height*bpp` bytes) into a shader-readable `format` texture.
    /// One image, one submit, one fence-wait. For uploading many textures at once (the overview
    /// app grid's ~24 icons on first open), [`TextureBatch`] shares the resource creation but
    /// records every copy into a single submit — see the module's batch section.
    #[allow(clippy::too_many_arguments)]
    fn upload(
        gpu: &Gpu,
        pool: vk::CommandPool,
        width: u32,
        height: u32,
        data: &[u8],
        format: vk::Format,
        bpp: vk::DeviceSize,
        components: vk::ComponentMapping,
        filter: vk::Filter,
    ) -> Result<Self> {
        let PendingUpload {
            staging,
            smem,
            texture,
        } = Self::build_pending(gpu, width, height, data, format, bpp, components, filter)?;
        let device = &gpu.device;
        let result = gpu.run_commands(pool, crate::stats::SubmitSite::Upload, |cbuf| unsafe {
            record_upload_copy(device, cbuf, texture.image, staging, width, height);
        });
        // The staging buffer has served its purpose either way; on a submit/wait error also
        // free the half-built texture (its copy never ran). `run_commands` already drains the
        // device on a wait error, so freeing here can't race an in-flight submission.
        unsafe {
            device.destroy_buffer(staging, None);
            device.free_memory(smem, None);
        }
        match result {
            Ok(()) => Ok(texture),
            Err(err) => {
                texture.destroy(gpu);
                Err(err)
            }
        }
    }

    /// The device-local half of an upload: a sampled image, its memory, view and sampler, with no
    /// staging buffer and no commands. Split out for [`upload_batch_shared_staging`], which shares
    /// one staging buffer across the batch and so cannot use
    /// [`build_pending`](Self::build_pending).
    fn new_sampled_image(gpu: &Gpu, item: &BatchItem<'_>) -> Result<Texture> {
        let _timed = crate::stats::creating();
        let device = &gpu.device;
        let mut guard = UploadGuard::new(device);

        let image_ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(item.format)
            .extent(vk::Extent3D {
                width: item.width,
                height: item.height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { device.create_image(&image_ci, None) }.context("texture image")?;
        guard.image = image;
        let ireq = unsafe { device.get_image_memory_requirements(image) };
        let memory = gpu.allocate(ireq, vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
        guard.memory = memory;
        unsafe { device.bind_image_memory(image, memory, 0)? };

        let view_ci = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(item.format)
            .components(item.components)
            .subresource_range(COLOR_RANGE);
        let view = unsafe { device.create_image_view(&view_ci, None) }.context("texture view")?;
        guard.view = view;

        let sampler_ci = vk::SamplerCreateInfo::default()
            .mag_filter(item.filter)
            .min_filter(item.filter)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE);
        let sampler = unsafe { device.create_sampler(&sampler_ci, None) }.context("sampler")?;
        guard.sampler = sampler;

        guard.image = vk::Image::null();
        guard.memory = vk::DeviceMemory::null();
        guard.view = vk::ImageView::null();
        guard.sampler = vk::Sampler::null();
        Ok(Texture {
            image,
            view,
            sampler,
            memory,
            width: item.width,
            height: item.height,
        })
    }

    /// Build everything `upload` needs before the GPU copy — the host staging buffer (already
    /// holding the pixels) and the device-local `Texture` (image/view/sampler), with the copy
    /// still un-recorded. Shared by the single [`upload`](Self::upload) and the batched
    /// [`TextureBatch`] so their resource creation stays identical; the caller records the copy
    /// (via [`record_upload_copy`]) and frees the staging once it has been submitted.
    #[allow(clippy::too_many_arguments)]
    fn build_pending(
        gpu: &Gpu,
        width: u32,
        height: u32,
        data: &[u8],
        format: vk::Format,
        bpp: vk::DeviceSize,
        components: vk::ComponentMapping,
        filter: vk::Filter,
    ) -> Result<PendingUpload> {
        let _timed = crate::stats::creating();
        let device = &gpu.device;
        let size = (width as vk::DeviceSize) * (height as vk::DeviceSize) * bpp;
        crate::stats::uploaded(size);
        assert_eq!(
            data.len() as vk::DeviceSize,
            size,
            "texture data size mismatch"
        );

        // Frees any handle created below if a later `?` fails partway (so a failed allocate/bind/
        // create doesn't orphan a resource); on success all six handles are nulled out of it
        // before they move into the returned `PendingUpload`.
        let mut guard = UploadGuard::new(device);

        // --- staging buffer with the pixel data ---
        let staging_ci = vk::BufferCreateInfo::default()
            .size(size)
            .usage(vk::BufferUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let staging =
            unsafe { device.create_buffer(&staging_ci, None) }.context("staging buffer")?;
        guard.staging = staging;
        let sreq = unsafe { device.get_buffer_memory_requirements(staging) };
        let smem = gpu.allocate(
            sreq,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )?;
        guard.smem = smem;
        unsafe {
            device.bind_buffer_memory(staging, smem, 0)?;
            let ptr = device
                .map_memory(smem, 0, size, vk::MemoryMapFlags::empty())
                .context("map staging")? as *mut u8;
            {
                // Timed apart from creation: this is a host write into write-combined memory, not
                // a round trip, and it is the entire cost of a wallpaper upload. Conflating the
                // two made `created` read as 9.96ms on a frame that created almost nothing.
                let _timed = crate::stats::staging_write();
                std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
            }
            device.unmap_memory(smem);
        }

        // --- device-local sampled image ---
        let image_ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { device.create_image(&image_ci, None) }.context("texture image")?;
        guard.image = image;
        let ireq = unsafe { device.get_image_memory_requirements(image) };
        let memory = gpu.allocate(ireq, vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
        guard.memory = memory;
        unsafe { device.bind_image_memory(image, memory, 0)? };

        // The view/sampler only reference the image handle, so they are valid before the copy
        // runs (nothing samples the texture until its batch is submitted).
        let view_ci = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .components(components)
            .subresource_range(COLOR_RANGE);
        let view = unsafe { device.create_image_view(&view_ci, None) }.context("texture view")?;
        guard.view = view;

        let sampler_ci = vk::SamplerCreateInfo::default()
            .mag_filter(filter)
            .min_filter(filter)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE);
        let sampler = unsafe { device.create_sampler(&sampler_ci, None) }.context("sampler")?;
        guard.sampler = sampler;

        // Success: the handles now belong to `PendingUpload`; disarm the guard for all of them.
        guard.staging = vk::Buffer::null();
        guard.smem = vk::DeviceMemory::null();
        guard.image = vk::Image::null();
        guard.memory = vk::DeviceMemory::null();
        guard.view = vk::ImageView::null();
        guard.sampler = vk::Sampler::null();
        Ok(PendingUpload {
            staging,
            smem,
            texture: Texture {
                image,
                view,
                sampler,
                memory,
                width,
                height,
            },
        })
    }

    pub fn destroy(&self, gpu: &Gpu) {
        unsafe {
            let d = &gpu.device;
            d.destroy_sampler(self.sampler, None);
            d.destroy_image_view(self.view, None);
            d.destroy_image(self.image, None);
            d.free_memory(self.memory, None);
        }
    }

    /// A blank R8 coverage image of `side` × `side`, `SAMPLED | TRANSFER_DST` with a NEAREST
    /// sampler, left in `SHADER_READ_ONLY_OPTIMAL` and zero-filled — the persistent glyph atlas.
    ///
    /// Unlike [`from_coverage`](Self::from_coverage) this uploads nothing: glyphs are copied in
    /// afterwards, region by region, by [`upload_coverage_regions`](Self::upload_coverage_regions).
    /// It is immediately safe to sample (all zero coverage = fully transparent), so a run whose
    /// glyphs are all already resident needs no GPU work at all.
    ///
    /// Zeroing is done with a clear rather than a host copy so no `side`-squared staging buffer is
    /// allocated (2048² would be a 4 MiB mappable blob, the allocation type that pressures the
    /// Venus host).
    pub fn new_coverage_atlas(gpu: &Gpu, pool: vk::CommandPool, side: u32) -> Result<Self> {
        let _timed = crate::stats::creating();
        let device = &gpu.device;
        let mut guard = UploadGuard::new(device);

        let image_ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::R8_UNORM)
            .extent(vk::Extent3D {
                width: side,
                height: side,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { device.create_image(&image_ci, None) }.context("atlas image")?;
        guard.image = image;
        let ireq = unsafe { device.get_image_memory_requirements(image) };
        let memory = gpu.allocate(ireq, vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
        guard.memory = memory;
        unsafe { device.bind_image_memory(image, memory, 0)? };

        let view_ci = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(vk::Format::R8_UNORM)
            .components(vk::ComponentMapping::default())
            .subresource_range(COLOR_RANGE);
        let view = unsafe { device.create_image_view(&view_ci, None) }.context("atlas view")?;
        guard.view = view;

        // NEAREST: glyph coverage is placed at whole pixels and sampled 1:1, so filtering would
        // only blur it.
        let sampler_ci = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::NEAREST)
            .min_filter(vk::Filter::NEAREST)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE);
        let sampler = unsafe { device.create_sampler(&sampler_ci, None) }.context("sampler")?;
        guard.sampler = sampler;

        // Creation ends here. What follows is a submit + fence wait, which the `Transition` site
        // already reports — leaving the timer running would count the same milliseconds twice, in
        // two clauses of the same log line.
        drop(_timed);

        let result = gpu.run_commands(pool, crate::stats::SubmitSite::Transition, |cbuf| unsafe {
            transition(
                device,
                cbuf,
                image,
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::AccessFlags::empty(),
                vk::AccessFlags::TRANSFER_WRITE,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
            );
            device.cmd_clear_color_image(
                cbuf,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &vk::ClearColorValue { float32: [0.; 4] },
                std::slice::from_ref(&COLOR_RANGE),
            );
            transition(
                device,
                cbuf,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                vk::AccessFlags::TRANSFER_WRITE,
                vk::AccessFlags::SHADER_READ,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
            );
        });

        let texture = Texture {
            image,
            view,
            sampler,
            memory,
            width: side,
            height: side,
        };
        // Past the fallible steps: the handles belong to `texture` now.
        guard.image = vk::Image::null();
        guard.memory = vk::DeviceMemory::null();
        guard.view = vk::ImageView::null();
        guard.sampler = vk::Sampler::null();

        match result {
            Ok(()) => Ok(texture),
            Err(err) => {
                texture.destroy(gpu);
                Err(err)
            }
        }
    }

    /// Copy freshly rasterized glyphs into this atlas: one `(x, y, w, h, coverage)` region each,
    /// all in a single command buffer, so a run that missed on several glyphs still costs one GPU
    /// round trip. No-op for an empty `regions`, which is the common case once the alphabet in use
    /// is resident — that is the whole point of a persistent atlas.
    ///
    /// The regions must not overlap and must lie inside the image; the atlas allocator guarantees
    /// both. Everything outside them keeps its contents (`SHADER_READ_ONLY_OPTIMAL` in, same out),
    /// so glyphs uploaded by earlier calls survive.
    pub fn upload_coverage_regions(
        &self,
        gpu: &Arc<Gpu>,
        pool: vk::CommandPool,
        regions: &[CoverageRegion<'_>],
    ) -> Result<()> {
        let Some(staged) = self.stage_coverage_regions(gpu, regions)? else {
            return Ok(());
        };
        let image = self.image;
        gpu.run_commands(pool, crate::stats::SubmitSite::UploadGlyphs, |cbuf| {
            record_coverage_copy(&gpu.device, cbuf, image, &staged)
        })
        // `staged` frees itself here; `run_commands` already waited (and drained the device on a
        // wait error), so no copy can still be reading it.
    }

    /// The host half of [`upload_coverage_regions`](Self::upload_coverage_regions): allocate the
    /// staging buffer and write every region's coverage into it, recording **no** commands. The
    /// returned [`GlyphStaging`] carries what a later [`record_coverage_copy`] needs, and frees
    /// itself when dropped — so whoever ends up owning it (a one-shot submit, a frame's in-flight
    /// record) does not have to remember to.
    ///
    /// `Ok(None)` for an empty `regions` — the common case once the alphabet in use is resident.
    pub fn stage_coverage_regions(
        &self,
        gpu: &Arc<Gpu>,
        regions: &[CoverageRegion<'_>],
    ) -> Result<Option<GlyphStaging>> {
        if regions.is_empty() {
            return Ok(None);
        }
        let device = &gpu.device;

        // One staging buffer for every region, concatenated; `buffer_offset` picks each out.
        let total: vk::DeviceSize = regions.iter().map(|r| r.coverage.len() as u64).sum();
        crate::stats::uploaded(total);
        let mut guard = UploadGuard::new(device);
        let staging_ci = vk::BufferCreateInfo::default()
            .size(total)
            .usage(vk::BufferUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let staging =
            unsafe { device.create_buffer(&staging_ci, None) }.context("atlas staging")?;
        guard.staging = staging;
        let sreq = unsafe { device.get_buffer_memory_requirements(staging) };
        let smem = gpu.allocate(
            sreq,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )?;
        guard.smem = smem;

        let mut copies = Vec::with_capacity(regions.len());
        unsafe {
            device.bind_buffer_memory(staging, smem, 0)?;
            let base = device
                .map_memory(smem, 0, total, vk::MemoryMapFlags::empty())
                .context("map atlas staging")? as *mut u8;
            let mut offset: vk::DeviceSize = 0;
            for r in regions {
                debug_assert_eq!(
                    r.coverage.len(),
                    (r.w * r.h) as usize,
                    "coverage size does not match the region"
                );
                std::ptr::copy_nonoverlapping(
                    r.coverage.as_ptr(),
                    base.add(offset as usize),
                    r.coverage.len(),
                );
                copies.push(
                    vk::BufferImageCopy::default()
                        .buffer_offset(offset)
                        .buffer_row_length(r.w)
                        .buffer_image_height(r.h)
                        .image_subresource(vk::ImageSubresourceLayers {
                            aspect_mask: vk::ImageAspectFlags::COLOR,
                            mip_level: 0,
                            base_array_layer: 0,
                            layer_count: 1,
                        })
                        .image_offset(vk::Offset3D {
                            x: r.x as i32,
                            y: r.y as i32,
                            z: 0,
                        })
                        .image_extent(vk::Extent3D {
                            width: r.w,
                            height: r.h,
                            depth: 1,
                        }),
                );
                offset += r.coverage.len() as u64;
            }
            device.unmap_memory(smem);
        }

        // The handles belong to `GlyphStaging` now; disarm the guard.
        guard.staging = vk::Buffer::null();
        guard.smem = vk::DeviceMemory::null();
        Ok(Some(GlyphStaging {
            gpu: gpu.clone(),
            buffer: staging,
            memory: smem,
            copies,
        }))
    }

    /// Re-upload a full `width*height` frame of tightly-packed pixels into this already-allocated
    /// image, reusing `staging` (the caller must have `ensure`d capacity and `write`n the data).
    /// It's a full overwrite, so the barrier discards the old contents (`UNDEFINED` old layout,
    /// valid regardless of the image's current layout) and leaves the image in
    /// `SHADER_READ_ONLY_OPTIMAL`. Used by the shm-client texture cache to refresh a cached texture
    /// in place instead of allocating a fresh image + staging buffer every commit.
    pub fn reupload_full(&self, gpu: &Gpu, pool: vk::CommandPool, staging: &Staging) -> Result<()> {
        let device = &gpu.device;
        // A full overwrite of the image's extent; the caller wrote exactly that into `staging`.
        crate::stats::uploaded((self.width as u64) * (self.height as u64) * 4);
        gpu.run_commands(pool, crate::stats::SubmitSite::UploadShm, |cbuf| unsafe {
            transition(
                device,
                cbuf,
                self.image,
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::AccessFlags::empty(),
                vk::AccessFlags::TRANSFER_WRITE,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
            );
            let region = vk::BufferImageCopy::default()
                .image_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                })
                .image_extent(vk::Extent3D {
                    width: self.width,
                    height: self.height,
                    depth: 1,
                });
            device.cmd_copy_buffer_to_image(
                cbuf,
                staging.buffer,
                self.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[region],
            );
            transition(
                device,
                cbuf,
                self.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                vk::AccessFlags::TRANSFER_WRITE,
                vk::AccessFlags::SHADER_READ,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
            );
        })
    }
}

/// One glyph's coverage bitmap and where it goes in the atlas, for
/// [`Texture::upload_coverage_regions`].
pub struct CoverageRegion<'a> {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    /// Tightly packed `w * h` R8 coverage.
    pub coverage: &'a [u8],
}

/// A reusable `HOST_VISIBLE | HOST_COHERENT` staging buffer for repeated transfers, in either
/// direction. Grown on demand and never shrunk; the buffer + memory persist, so re-uploading a
/// client's shm buffer every commit — or reading a frame back every commit — doesn't churn a fresh
/// mappable allocation each time (the mappable-blob type that pressures the Venus host). Not
/// internally synchronized — the renderer serializes access via `&mut self`, and each transfer is
/// fence-waited before the next, so overwriting the mapped bytes is safe. Call [`Staging::destroy`]
/// before dropping (it holds raw Vulkan handles).
pub struct Staging {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    capacity: vk::DeviceSize,
    usage: vk::BufferUsageFlags,
}

impl Staging {
    /// An upload staging buffer (`TRANSFER_SRC`): host writes, GPU reads.
    pub const fn new() -> Self {
        Self::with_usage(vk::BufferUsageFlags::TRANSFER_SRC)
    }

    /// A readback staging buffer (`TRANSFER_DST`): GPU writes, host reads.
    pub const fn new_readback() -> Self {
        Self::with_usage(vk::BufferUsageFlags::TRANSFER_DST)
    }

    const fn with_usage(usage: vk::BufferUsageFlags) -> Self {
        Self {
            buffer: vk::Buffer::null(),
            memory: vk::DeviceMemory::null(),
            capacity: 0,
            usage,
        }
    }

    /// The underlying buffer handle. Null until the first [`Staging::ensure`].
    pub fn buffer(&self) -> vk::Buffer {
        self.buffer
    }

    /// Bytes currently allocated. `ensure` only reallocates when it must grow past this.
    pub fn capacity(&self) -> vk::DeviceSize {
        self.capacity
    }

    /// Ensure the staging holds at least `size` bytes, (re)allocating grow-only if needed. Safe to
    /// realloc synchronously: the renderer fence-waits every upload, so nothing references the old
    /// buffer.
    pub fn ensure(&mut self, gpu: &Gpu, size: vk::DeviceSize) -> Result<()> {
        if size <= self.capacity {
            return Ok(());
        }
        unsafe { self.destroy(&gpu.device) };
        let device = &gpu.device;
        let ci = vk::BufferCreateInfo::default()
            .size(size)
            .usage(self.usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let buffer = unsafe { device.create_buffer(&ci, None) }.context("staging buffer")?;
        // `buffer` is live but not yet owned by `self`, so destroy it on any error before
        // returning: a failed allocate/bind (host mappable-memory exhaustion — the very
        // pressure this cache targets) must not orphan the handle, or a failing grow would
        // leak one per commit.
        let req = unsafe { device.get_buffer_memory_requirements(buffer) };
        let memory = match gpu.allocate(
            req,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        ) {
            Ok(memory) => memory,
            Err(e) => {
                unsafe { device.destroy_buffer(buffer, None) };
                return Err(e);
            }
        };
        if let Err(e) =
            unsafe { device.bind_buffer_memory(buffer, memory, 0) }.context("bind staging")
        {
            unsafe {
                device.destroy_buffer(buffer, None);
                device.free_memory(memory, None);
            }
            return Err(e);
        }
        self.buffer = buffer;
        self.memory = memory;
        self.capacity = size;
        Ok(())
    }

    /// Copy `data` into the staging (caller must have `ensure`d `data.len()` capacity). Maps and
    /// unmaps around the copy; `HOST_COHERENT` means no explicit flush is needed.
    pub fn write(&self, gpu: &Gpu, data: &[u8]) -> Result<()> {
        assert!(data.len() as vk::DeviceSize <= self.capacity);
        let device = &gpu.device;
        unsafe {
            let ptr = device
                .map_memory(
                    self.memory,
                    0,
                    data.len() as vk::DeviceSize,
                    vk::MemoryMapFlags::empty(),
                )
                .context("map staging")? as *mut u8;
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
            device.unmap_memory(self.memory);
        }
        Ok(())
    }

    /// Copy `len` bytes out of the staging into a fresh `Vec` (caller must have `ensure`d `len`
    /// capacity, and the GPU write must already be complete — the renderer fence-waits). Maps and
    /// unmaps around the copy; `HOST_COHERENT` means no explicit invalidate is needed.
    pub fn read(&self, gpu: &Gpu, len: usize) -> Result<Vec<u8>> {
        assert!(len as vk::DeviceSize <= self.capacity);
        let device = &gpu.device;
        let mut out = vec![0u8; len];
        unsafe {
            let ptr = device
                .map_memory(
                    self.memory,
                    0,
                    len as vk::DeviceSize,
                    vk::MemoryMapFlags::empty(),
                )
                .context("map staging for readback")? as *const u8;
            std::ptr::copy_nonoverlapping(ptr, out.as_mut_ptr(), len);
            device.unmap_memory(self.memory);
        }
        Ok(out)
    }

    /// Free the underlying buffer + memory (idempotent; leaves the staging empty — a later `ensure`
    /// reallocates).
    ///
    /// # Safety
    /// No in-flight GPU work may reference the buffer.
    pub unsafe fn destroy(&mut self, device: &ash::Device) {
        if self.memory != vk::DeviceMemory::null() {
            device.destroy_buffer(self.buffer, None);
            device.free_memory(self.memory, None);
        }
        self.buffer = vk::Buffer::null();
        self.memory = vk::DeviceMemory::null();
        self.capacity = 0;
    }
}

impl Default for Staging {
    fn default() -> Self {
        Self::new()
    }
}

/// A glyph-atlas staging buffer with its pixels already written and its copy regions computed,
/// waiting for someone to record the copy ([`record_coverage_copy`]) into a command buffer.
///
/// Owns its buffer + memory and frees them on drop, holding an `Arc<Gpu>` to guarantee the device
/// outlives them — the same shape as `VkSubmitFence`. That is what lets the *frame* decide, at
/// submit time, whether this lives in an in-flight record or dies with the fence wait, without any
/// retirement path having to know a glyph staging exists.
///
/// **It must outlive the submit of the command buffer its copy was recorded into.** Dropping it
/// earlier leaves the GPU reading freed memory — legal Vulkan as far as any validation layer is
/// concerned, and invisible until glyphs come out as garbage.
pub struct GlyphStaging {
    gpu: Arc<Gpu>,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    copies: Vec<vk::BufferImageCopy>,
}

impl Drop for GlyphStaging {
    fn drop(&mut self) {
        unsafe {
            self.gpu.device.destroy_buffer(self.buffer, None);
            self.gpu.device.free_memory(self.memory, None);
        }
    }
}

/// Record a staged glyph upload into `cbuf`: barrier out of the sampleable layout, copy every
/// region, barrier back. Must be called **outside a render pass** (copies and layout transitions
/// both are), and the second barrier is what orders the copy before any glyph draw later in the
/// same command buffer.
///
/// Split out of [`Texture::upload_coverage_regions`] so the same recording serves a standalone
/// submit and a fold into a frame's own command buffer.
pub fn record_coverage_copy(
    device: &ash::Device,
    cbuf: vk::CommandBuffer,
    image: vk::Image,
    staged: &GlyphStaging,
) {
    unsafe {
        // Preserve what is already in the atlas: transition FROM the sampleable layout the image is
        // left in, not from UNDEFINED (which would license discarding it). The image is never in
        // UNDEFINED here — `new_coverage_atlas` transitions it at creation.
        transition(
            device,
            cbuf,
            image,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::AccessFlags::SHADER_READ,
            vk::AccessFlags::TRANSFER_WRITE,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::PipelineStageFlags::TRANSFER,
        );
        device.cmd_copy_buffer_to_image(
            cbuf,
            staged.buffer,
            image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &staged.copies,
        );
        transition(
            device,
            cbuf,
            image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::AccessFlags::TRANSFER_WRITE,
            vk::AccessFlags::SHADER_READ,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
        );
    }
}

/// Record a freshly-created (`UNDEFINED`) image's staging→image upload into `cbuf`: barrier to
/// `TRANSFER_DST`, copy the full `width*height` buffer, barrier to `SHADER_READ_ONLY`. Shared by
/// [`Texture::upload`] (one image, one submit) and [`TextureBatch`] (N images, one submit) so the
/// barrier/copy stay byte-identical between the single and batched paths.
unsafe fn record_upload_copy(
    device: &ash::Device,
    cbuf: vk::CommandBuffer,
    image: vk::Image,
    staging: vk::Buffer,
    width: u32,
    height: u32,
) {
    record_upload_copy_at(device, cbuf, image, staging, 0, width, height)
}

/// [`record_upload_copy`] against a shared staging buffer, reading from `buffer_offset` rather
/// than the start. One function for both so a batch and a single upload cannot drift in the
/// barriers they record.
unsafe fn record_upload_copy_at(
    device: &ash::Device,
    cbuf: vk::CommandBuffer,
    image: vk::Image,
    staging: vk::Buffer,
    buffer_offset: vk::DeviceSize,
    width: u32,
    height: u32,
) {
    transition(
        device,
        cbuf,
        image,
        vk::ImageLayout::UNDEFINED,
        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        vk::AccessFlags::empty(),
        vk::AccessFlags::TRANSFER_WRITE,
        vk::PipelineStageFlags::TOP_OF_PIPE,
        vk::PipelineStageFlags::TRANSFER,
    );
    let region = vk::BufferImageCopy::default()
        .buffer_offset(buffer_offset)
        .image_subresource(vk::ImageSubresourceLayers {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            mip_level: 0,
            base_array_layer: 0,
            layer_count: 1,
        })
        .image_extent(vk::Extent3D {
            width,
            height,
            depth: 1,
        });
    device.cmd_copy_buffer_to_image(
        cbuf,
        staging,
        image,
        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        &[region],
    );
    transition(
        device,
        cbuf,
        image,
        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        vk::AccessFlags::TRANSFER_WRITE,
        vk::AccessFlags::SHADER_READ,
        vk::PipelineStageFlags::TRANSFER,
        vk::PipelineStageFlags::FRAGMENT_SHADER,
    );
}

#[allow(clippy::too_many_arguments)]
unsafe fn transition(
    device: &ash::Device,
    cbuf: vk::CommandBuffer,
    image: vk::Image,
    old: vk::ImageLayout,
    new: vk::ImageLayout,
    src_access: vk::AccessFlags,
    dst_access: vk::AccessFlags,
    src_stage: vk::PipelineStageFlags,
    dst_stage: vk::PipelineStageFlags,
) {
    let barrier = vk::ImageMemoryBarrier::default()
        .old_layout(old)
        .new_layout(new)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(COLOR_RANGE)
        .src_access_mask(src_access)
        .dst_access_mask(dst_access);
    device.cmd_pipeline_barrier(
        cbuf,
        src_stage,
        dst_stage,
        vk::DependencyFlags::empty(),
        &[],
        &[],
        &[barrier],
    );
}
