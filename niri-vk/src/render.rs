//! Offscreen render target + a minimal quad graphics pipeline, built on the [`Gpu`] context.
//!
//! A classic render pass / framebuffer (not dynamic rendering) keeps the spike portable across
//! ICDs without enabling extra features. The target's final layout is `TRANSFER_SRC_OPTIMAL`, so
//! [`RenderTarget::read_back`] can copy straight to a host buffer after the pass.

use anyhow::{Context, Result};
use ash::vk;

use crate::gpu::Gpu;

pub const FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;

pub const COLOR_RANGE: vk::ImageSubresourceRange = vk::ImageSubresourceRange {
    aspect_mask: vk::ImageAspectFlags::COLOR,
    base_mip_level: 0,
    level_count: 1,
    base_array_layer: 0,
    layer_count: 1,
};

/// Push-constant block shared by `quad.vert` and its material fragment stages. `repr(C)` layout
/// matches the GLSL `Push` block (std430 push-constant rules: `color` lands at offset 32,
/// `src_rect` at 48). Materials that don't need a field (e.g. `solid.frag` ignores `src_rect`)
/// simply declare a shorter `Push` block — a shader may access a prefix of the range.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct QuadPush {
    pub origin: [f32; 2],
    pub size: [f32; 2],
    pub target: [f32; 2],
    pub corner_radius: f32,
    pub _pad0: f32,
    pub color: [f32; 4],
    /// Sub-rectangle of the texture to sample, normalized `[u0, v0, du, dv]`; `[0, 0, 1, 1]` is
    /// the full texture. Used by the sampling materials to remap `v_uv` (see `texture.frag`).
    pub src_rect: [f32; 4],
}

pub struct RenderTarget {
    pub image: vk::Image,
    pub memory: vk::DeviceMemory,
    pub view: vk::ImageView,
    pub framebuffer: vk::Framebuffer,
    pub render_pass: vk::RenderPass,
    pub width: u32,
    pub height: u32,
}

impl RenderTarget {
    pub fn new(gpu: &Gpu, width: u32, height: u32) -> Result<Self> {
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
            .usage(vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image =
            unsafe { device.create_image(&image_ci, None) }.context("create target image")?;

        let req = unsafe { device.get_image_memory_requirements(image) };
        let index =
            gpu.find_memory_type(req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(index);
        let memory = unsafe { device.allocate_memory(&alloc, None) }.context("target memory")?;
        unsafe { device.bind_image_memory(image, memory, 0)? };

        let view_ci = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(FORMAT)
            .subresource_range(COLOR_RANGE);
        let view = unsafe { device.create_image_view(&view_ci, None) }.context("target view")?;

        let render_pass = create_render_pass(device)?;

        let fb_ci = vk::FramebufferCreateInfo::default()
            .render_pass(render_pass)
            .attachments(std::slice::from_ref(&view))
            .width(width)
            .height(height)
            .layers(1);
        let framebuffer =
            unsafe { device.create_framebuffer(&fb_ci, None) }.context("framebuffer")?;

        Ok(RenderTarget {
            image,
            memory,
            view,
            framebuffer,
            render_pass,
            width,
            height,
        })
    }

    pub fn extent(&self) -> vk::Extent2D {
        vk::Extent2D {
            width: self.width,
            height: self.height,
        }
    }

    /// Begin the render pass with `clear` as the load-clear color.
    pub fn begin(&self, gpu: &Gpu, cbuf: vk::CommandBuffer, clear: [f32; 4]) {
        let clears = [vk::ClearValue {
            color: vk::ClearColorValue { float32: clear },
        }];
        let begin = vk::RenderPassBeginInfo::default()
            .render_pass(self.render_pass)
            .framebuffer(self.framebuffer)
            .render_area(vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent: self.extent(),
            })
            .clear_values(&clears);
        unsafe {
            gpu.device
                .cmd_begin_render_pass(cbuf, &begin, vk::SubpassContents::INLINE);
        }
    }

    /// Copy the rendered image (already in `TRANSFER_SRC_OPTIMAL`) into host memory as RGBA8.
    pub fn read_back(&self, gpu: &Gpu, pool: vk::CommandPool) -> Result<Vec<u8>> {
        let device = &gpu.device;
        let size = (self.width as vk::DeviceSize) * (self.height as vk::DeviceSize) * 4;

        let buf_ci = vk::BufferCreateInfo::default()
            .size(size)
            .usage(vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let buffer = unsafe { device.create_buffer(&buf_ci, None) }.context("readback buffer")?;
        let req = unsafe { device.get_buffer_memory_requirements(buffer) };
        let index = gpu.find_memory_type(
            req.memory_type_bits,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )?;
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(index);
        let buf_mem = unsafe { device.allocate_memory(&alloc, None) }.context("readback memory")?;
        unsafe { device.bind_buffer_memory(buffer, buf_mem, 0)? };

        gpu.run_commands(pool, |cbuf| unsafe {
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
            device.cmd_copy_image_to_buffer(
                cbuf,
                self.image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                buffer,
                &[region],
            );
            let host = vk::MemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ);
            device.cmd_pipeline_barrier(
                cbuf,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::HOST,
                vk::DependencyFlags::empty(),
                &[host],
                &[],
                &[],
            );
        })?;

        let mut pixels = vec![0u8; size as usize];
        unsafe {
            let ptr = device
                .map_memory(buf_mem, 0, size, vk::MemoryMapFlags::empty())
                .context("map readback")? as *const u8;
            std::ptr::copy_nonoverlapping(ptr, pixels.as_mut_ptr(), size as usize);
            device.unmap_memory(buf_mem);
            device.destroy_buffer(buffer, None);
            device.free_memory(buf_mem, None);
        }
        Ok(pixels)
    }

    pub fn destroy(&self, gpu: &Gpu) {
        unsafe {
            let d = &gpu.device;
            d.destroy_framebuffer(self.framebuffer, None);
            d.destroy_render_pass(self.render_pass, None);
            d.destroy_image_view(self.view, None);
            d.destroy_image(self.image, None);
            d.free_memory(self.memory, None);
        }
    }
}

fn create_render_pass(device: &ash::Device) -> Result<vk::RenderPass> {
    let attachment = vk::AttachmentDescription::default()
        .format(FORMAT)
        .samples(vk::SampleCountFlags::TYPE_1)
        .load_op(vk::AttachmentLoadOp::CLEAR)
        .store_op(vk::AttachmentStoreOp::STORE)
        .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
        .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .final_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL);
    let color_ref = vk::AttachmentReference::default()
        .attachment(0)
        .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);
    let subpass = vk::SubpassDescription::default()
        .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
        .color_attachments(std::slice::from_ref(&color_ref));
    // Order the color writes after the layout transition, and the later transfer-read after the
    // color writes (so read_back sees a complete image).
    let deps = [
        vk::SubpassDependency::default()
            .src_subpass(vk::SUBPASS_EXTERNAL)
            .dst_subpass(0)
            .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .src_access_mask(vk::AccessFlags::empty())
            .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE),
        vk::SubpassDependency::default()
            .src_subpass(0)
            .dst_subpass(vk::SUBPASS_EXTERNAL)
            .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .dst_stage_mask(vk::PipelineStageFlags::TRANSFER)
            .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
            .dst_access_mask(vk::AccessFlags::TRANSFER_READ),
    ];
    let ci = vk::RenderPassCreateInfo::default()
        .attachments(std::slice::from_ref(&attachment))
        .subpasses(std::slice::from_ref(&subpass))
        .dependencies(&deps);
    unsafe { device.create_render_pass(&ci, None) }.context("create render pass")
}

/// A graphics pipeline for `quad.vert` + one material fragment stage. All share the [`QuadPush`]
/// push-constant range, so switching materials mid-pass is just a rebind + re-push + redraw.
pub struct QuadPipeline {
    pub layout: vk::PipelineLayout,
    pub pipeline: vk::Pipeline,
    vert: vk::ShaderModule,
    frag: vk::ShaderModule,
}

impl QuadPipeline {
    pub fn new(
        gpu: &Gpu,
        render_pass: vk::RenderPass,
        extent: vk::Extent2D,
        vert_spv: &[u8],
        frag_spv: &[u8],
        set_layouts: &[vk::DescriptorSetLayout],
    ) -> Result<Self> {
        let device = &gpu.device;
        let vert = load_module(device, vert_spv)?;
        let frag = load_module(device, frag_spv)?;

        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(vert)
                .name(c"main"),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(frag)
                .name(c"main"),
        ];

        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
        let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_LIST);

        let viewports = [vk::Viewport {
            x: 0.0,
            y: 0.0,
            width: extent.width as f32,
            height: extent.height as f32,
            min_depth: 0.0,
            max_depth: 1.0,
        }];
        let scissors = [vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent,
        }];
        let viewport_state = vk::PipelineViewportStateCreateInfo::default()
            .viewports(&viewports)
            .scissors(&scissors);

        let raster = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(vk::CullModeFlags::NONE)
            .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
            .line_width(1.0);
        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);

        // Standard straight-alpha over-compositing.
        let blend_attachment = vk::PipelineColorBlendAttachmentState::default()
            .blend_enable(true)
            .src_color_blend_factor(vk::BlendFactor::SRC_ALPHA)
            .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .color_blend_op(vk::BlendOp::ADD)
            .src_alpha_blend_factor(vk::BlendFactor::ONE)
            .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .alpha_blend_op(vk::BlendOp::ADD)
            .color_write_mask(vk::ColorComponentFlags::RGBA);
        let blend = vk::PipelineColorBlendStateCreateInfo::default()
            .attachments(std::slice::from_ref(&blend_attachment));

        let push_range = vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(std::mem::size_of::<QuadPush>() as u32);
        let layout_ci = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(set_layouts)
            .push_constant_ranges(std::slice::from_ref(&push_range));
        let layout = unsafe { device.create_pipeline_layout(&layout_ci, None) }
            .context("pipeline layout")?;

        let pipeline_ci = vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&input_assembly)
            .viewport_state(&viewport_state)
            .rasterization_state(&raster)
            .multisample_state(&multisample)
            .color_blend_state(&blend)
            .layout(layout)
            .render_pass(render_pass)
            .subpass(0);
        let pipeline = unsafe {
            device.create_graphics_pipelines(vk::PipelineCache::null(), &[pipeline_ci], None)
        }
        .map_err(|(_, e)| e)
        .context("create graphics pipeline")?[0];

        Ok(QuadPipeline {
            layout,
            pipeline,
            vert,
            frag,
        })
    }

    /// Bind, (optionally) bind descriptor set 0, push `quad`'s params, and draw the quad.
    pub fn draw(
        &self,
        gpu: &Gpu,
        cbuf: vk::CommandBuffer,
        quad: &QuadPush,
        set: Option<vk::DescriptorSet>,
    ) {
        unsafe {
            gpu.device
                .cmd_bind_pipeline(cbuf, vk::PipelineBindPoint::GRAPHICS, self.pipeline);
            if let Some(set) = set {
                gpu.device.cmd_bind_descriptor_sets(
                    cbuf,
                    vk::PipelineBindPoint::GRAPHICS,
                    self.layout,
                    0,
                    &[set],
                    &[],
                );
            }
            gpu.device.cmd_push_constants(
                cbuf,
                self.layout,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                as_bytes(quad),
            );
            gpu.device.cmd_draw(cbuf, 6, 1, 0, 0);
        }
    }

    pub fn destroy(&self, gpu: &Gpu) {
        unsafe {
            let d = &gpu.device;
            d.destroy_pipeline(self.pipeline, None);
            d.destroy_pipeline_layout(self.layout, None);
            d.destroy_shader_module(self.vert, None);
            d.destroy_shader_module(self.frag, None);
        }
    }
}

pub fn load_module(device: &ash::Device, spv: &[u8]) -> Result<vk::ShaderModule> {
    let code = ash::util::read_spv(&mut std::io::Cursor::new(spv)).context("read SPIR-V")?;
    let ci = vk::ShaderModuleCreateInfo::default().code(&code);
    unsafe { device.create_shader_module(&ci, None) }.context("create shader module")
}

/// Descriptor set layout for a single combined image sampler at binding 0 (fragment stage) —
/// what the textured / glyph-atlas materials sample from.
pub fn sampler_set_layout(gpu: &Gpu) -> Result<vk::DescriptorSetLayout> {
    let binding = vk::DescriptorSetLayoutBinding::default()
        .binding(0)
        .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
        .descriptor_count(1)
        .stage_flags(vk::ShaderStageFlags::FRAGMENT);
    let ci = vk::DescriptorSetLayoutCreateInfo::default().bindings(std::slice::from_ref(&binding));
    unsafe { gpu.device.create_descriptor_set_layout(&ci, None) }.context("descriptor set layout")
}

/// View a `repr(C)` POD as bytes for `cmd_push_constants`.
pub fn as_bytes<T: Copy>(v: &T) -> &[u8] {
    unsafe { std::slice::from_raw_parts((v as *const T).cast::<u8>(), std::mem::size_of::<T>()) }
}
