//! A sampled texture: device-local image uploaded from host RGBA via a staging buffer, plus a
//! view and sampler. This is the infrastructure both the blur passes and the glyph atlas reuse.

use std::os::fd::{BorrowedFd, FromRawFd, IntoRawFd, OwnedFd};

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
        let image =
            unsafe { device.create_image(&image_ci, None) }.context("color-target image")?;
        let ireq = unsafe { device.get_image_memory_requirements(image) };
        let memory = gpu.allocate(ireq, vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
        unsafe { device.bind_image_memory(image, memory, 0)? };

        let view_ci = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(FORMAT)
            .subresource_range(COLOR_RANGE);
        let view =
            unsafe { device.create_image_view(&view_ci, None) }.context("color-target view")?;

        let sampler_ci = vk::SamplerCreateInfo::default()
            .mag_filter(filter)
            .min_filter(filter)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE);
        let sampler = unsafe { device.create_sampler(&sampler_ci, None) }.context("sampler")?;

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
    /// `R8G8B8A8_UNORM` for `Abgr8888`/`Xbgr8888`).
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
            .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut ext_info)
            .push_next(&mut mod_info);
        let image = unsafe { device.create_image(&image_ci, None) }
            .context("create dmabuf render-target image")?;

        let mem_req = unsafe { device.get_image_memory_requirements(image) };

        // Prefer the memory types the driver reports valid for this fd, but treat the query as
        // best-effort — on Venus it can reject a perfectly importable dmabuf (see `dmabuf.rs`).
        let ext_fd = ash::khr::external_memory_fd::Device::new(&gpu.instance, &gpu.device);
        let mut type_bits = mem_req.memory_type_bits;
        let mut fd_props = vk::MemoryFdPropertiesKHR::default();
        if let Ok(()) = unsafe {
            ext_fd.get_memory_fd_properties(
                vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                fd.try_clone_to_owned()
                    .context("dup dmabuf fd for property query")?
                    .into_raw_fd(),
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
        unsafe { device.bind_image_memory(image, memory, 0) }
            .context("bind imported render-target memory")?;

        let view_ci = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .subresource_range(COLOR_RANGE);
        let view = unsafe { device.create_image_view(&view_ci, None) }
            .context("dmabuf render-target view")?;

        // Sampler is unused (a scanout target is never sampled) but kept so this fits `Texture`.
        let sampler_ci = vk::SamplerCreateInfo::default()
            .mag_filter(filter)
            .min_filter(filter)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE);
        let sampler = unsafe { device.create_sampler(&sampler_ci, None) }.context("sampler")?;

        Ok(Texture {
            image,
            view,
            sampler,
            memory,
            width,
            height,
        })
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

        // --- staging buffer with the pixel data ---
        let staging_ci = vk::BufferCreateInfo::default()
            .size(size)
            .usage(vk::BufferUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let staging =
            unsafe { device.create_buffer(&staging_ci, None) }.context("staging buffer")?;
        let sreq = unsafe { device.get_buffer_memory_requirements(staging) };
        let smem = gpu.allocate(
            sreq,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )?;
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
        let ireq = unsafe { device.get_image_memory_requirements(image) };
        let memory = gpu.allocate(ireq, vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
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

        unsafe {
            device.destroy_buffer(staging, None);
            device.free_memory(smem, None);
        }

        let view_ci = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .components(components)
            .subresource_range(COLOR_RANGE);
        let view = unsafe { device.create_image_view(&view_ci, None) }.context("texture view")?;

        let sampler_ci = vk::SamplerCreateInfo::default()
            .mag_filter(filter)
            .min_filter(filter)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE);
        let sampler = unsafe { device.create_sampler(&sampler_ci, None) }.context("sampler")?;

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
