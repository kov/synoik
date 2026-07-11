//! A sampled texture: device-local image uploaded from host RGBA via a staging buffer, plus a
//! view and sampler. This is the infrastructure both the blur passes and the glyph atlas reuse.

use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd};

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

        let view_ci = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .subresource_range(COLOR_RANGE);
        let view = unsafe { device.create_image_view(&view_ci, None) }
            .context("dmabuf render-target view")?;
        guard.view = view;

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
        gpu.run_commands(pool, |cbuf| unsafe {
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

    /// Re-acquire an already-imported sampled dmabuf for a fresh producer frame. The client renders
    /// new content into the *same* underlying dmabuf every frame (this image's memory IS that
    /// shared dmabuf), so a cache that reuses the [`Self::import_dmabuf_sampled`] image across
    /// commits must, before sampling it again, (a) make the producer's new writes visible to
    /// our sampler and (b) re-take queue-family ownership from `FOREIGN`. That's exactly the
    /// acquire barrier the initial import runs, with the queue-ownership acquire from `FOREIGN`
    /// carrying the visibility of the producer's writes. When `VK_EXT_queue_family_foreign` is
    /// present we keep the image's *current* tracked `old_layout` (`SHADER_READ_ONLY_OPTIMAL`)
    /// rather than `UNDEFINED`: `UNDEFINED` licenses the driver to discard/re-lay-out contents,
    /// harmless on this VM's LINEAR-only path but corrupting for a tiled/CCS modifier. Without
    /// the extension there is no `FOREIGN` ownership transfer, so a same-layout barrier with an
    /// empty source scope would be a visibility no-op and sample stale content; we instead
    /// force an `UNDEFINED`→`SHADER_READ_ONLY_OPTIMAL` transition, which does invalidate the
    /// read caches and is safe because only LINEAR buffers ever reach the cache (their bytes
    /// survive `UNDEFINED`). Either way we deliberately skip the paired ours→`FOREIGN`
    /// *release* after the previous frame's last sample: a full ownership round-trip is
    /// spec-correct but tolerated to skip for LINEAR (revisit if tiled modifiers are ever
    /// imported). The image ends in `SHADER_READ_ONLY_OPTIMAL`. Callers pass the tracked
    /// current layout as `old_layout`.
    pub fn reacquire_dmabuf_sampled(
        &self,
        gpu: &Gpu,
        pool: vk::CommandPool,
        old_layout: vk::ImageLayout,
    ) -> Result<()> {
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
        gpu.run_commands(pool, |cbuf| unsafe {
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
        })
        .context("re-acquire imported sampled image")
    }

    /// Upload `data` (tight `width*height*bpp` bytes) into a shader-readable `format` texture.
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
        let device = &gpu.device;
        let size = (width as vk::DeviceSize) * (height as vk::DeviceSize) * bpp;
        assert_eq!(
            data.len() as vk::DeviceSize,
            size,
            "texture data size mismatch"
        );

        // Frees any handle created below if a later `?` fails partway (so a failed allocate/bind/
        // create doesn't orphan a resource); on success the image/memory/view/sampler are nulled
        // out of it before they move into the returned `Texture`, leaving it only the staging.
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
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
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

        gpu.run_commands(pool, |cbuf| unsafe {
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
        })?;
        // `staging`/`smem` are no longer referenced; the guard frees them on return (both here and
        // on any error below), replacing the old manual free.

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

        // Success: hand the image/memory/view/sampler to the `Texture` and disarm the guard for
        // them (it still frees the staging buffer + memory on drop).
        guard.image = vk::Image::null();
        guard.memory = vk::DeviceMemory::null();
        guard.view = vk::ImageView::null();
        guard.sampler = vk::Sampler::null();
        Ok(Texture {
            image,
            view,
            sampler,
            memory,
            width,
            height,
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

    /// Re-upload a full `width*height` frame of tightly-packed pixels into this already-allocated
    /// image, reusing `staging` (the caller must have `ensure`d capacity and `write`n the data).
    /// It's a full overwrite, so the barrier discards the old contents (`UNDEFINED` old layout,
    /// valid regardless of the image's current layout) and leaves the image in
    /// `SHADER_READ_ONLY_OPTIMAL`. Used by the shm-client texture cache to refresh a cached texture
    /// in place instead of allocating a fresh image + staging buffer every commit.
    pub fn reupload_full(&self, gpu: &Gpu, pool: vk::CommandPool, staging: &Staging) -> Result<()> {
        let device = &gpu.device;
        gpu.run_commands(pool, |cbuf| unsafe {
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

/// A reusable `HOST_VISIBLE | HOST_COHERENT` staging buffer for repeated texture uploads. Grown on
/// demand and never shrunk; the buffer + memory persist across uploads, so re-uploading a client's
/// shm buffer every commit doesn't churn a fresh mappable allocation each time (the mappable-blob
/// type that pressures the Venus host). Not internally synchronized — the renderer serializes
/// access via `&mut self`, and each upload is fence-waited before the next, so overwriting the
/// mapped bytes is safe. Call [`Staging::destroy`] before dropping (it holds raw Vulkan handles).
pub struct Staging {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    capacity: vk::DeviceSize,
}

impl Staging {
    pub const fn new() -> Self {
        Self {
            buffer: vk::Buffer::null(),
            memory: vk::DeviceMemory::null(),
            capacity: 0,
        }
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
            .usage(vk::BufferUsageFlags::TRANSFER_SRC)
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
