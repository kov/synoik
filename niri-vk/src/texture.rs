//! A sampled texture: device-local image uploaded from host RGBA via a staging buffer, plus a
//! view and sampler. This is the infrastructure both the blur passes and the glyph atlas reuse.

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
        Self::upload(gpu, pool, width, height, rgba, FORMAT, 4, filter)
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
            .usage(
                vk::ImageUsageFlags::COLOR_ATTACHMENT
                    | vk::ImageUsageFlags::SAMPLED
                    | vk::ImageUsageFlags::TRANSFER_SRC,
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
