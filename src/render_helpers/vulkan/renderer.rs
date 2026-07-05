use std::fmt;
use std::sync::Arc;

use ash::vk;
use niri_vk::blur::BlurChain;
use niri_vk::gpu::Gpu;
use niri_vk::render::{
    load_module, sampler_set_layout, BorderPush, PostprocessPush, QuadPush, ResizePush, ShadowPush,
    COLOR_RANGE,
};
use niri_vk::shaders::{
    BORDER_FRAG, BORDER_VERT, GRADIENT_FADE_FRAG, POSTPROCESS_FRAG, POSTPROCESS_VERT, QUAD_VERT,
    RESIZE_FRAG, RESIZE_VERT, ROUNDED_TEX_FRAG, SHADOW_FRAG, SHADOW_VERT, SOLID_FRAG, TEX_FRAG,
};
use niri_vk::texture::Texture as NiriTexture;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::sync::SyncPoint;
use smithay::backend::renderer::{
    Bind, ContextId, DebugFlags, ExportMem, ImportMem, Offscreen, Renderer, RendererSuper,
    TextureFilter,
};
use smithay::utils::{Buffer as BufferCoord, Physical, Rectangle, Size, Transform};

use super::custom::{compile_custom, CustomShaderType};
use super::error::VulkanError;
use super::frame::VulkanFrame;
use super::types::{is_rgba8888, VkFramebuffer, VkMapping, VkTexture, IMAGE_VK_FORMAT};
use crate::render_helpers::blur::BlurOptions;

/// One `quad.vert` + material-fragment graphics pipeline with dynamic viewport/scissor (so it is
/// reused across differently-sized targets).
pub(super) struct Pipeline {
    pub(super) pipeline: vk::Pipeline,
    pub(super) layout: vk::PipelineLayout,
    vert: vk::ShaderModule,
    frag: vk::ShaderModule,
}

impl Pipeline {
    unsafe fn destroy(&self, dev: &ash::Device) {
        dev.destroy_pipeline(self.pipeline, None);
        dev.destroy_pipeline_layout(self.layout, None);
        dev.destroy_shader_module(self.vert, None);
        dev.destroy_shader_module(self.frag, None);
    }
}

/// An owned Vulkan renderer implementing Smithay's renderer traits. See the module docs for scope.
pub struct VulkanRenderer {
    pub(super) gpu: Arc<Gpu>,
    context_id: ContextId<VkTexture>,
    pub(super) render_pass: vk::RenderPass,
    pub(super) solid_pipeline: Pipeline,
    pub(super) texture_pipeline: Pipeline,
    pub(super) rounded_texture_pipeline: Pipeline,
    pub(super) gradient_fade_pipeline: Pipeline,
    pub(super) border_pipeline: Pipeline,
    pub(super) shadow_pipeline: Pipeline,
    pub(super) postprocess_pipeline: Pipeline,
    pub(super) resize_pipeline: Pipeline,
    /// Runtime-compiled user animation shaders (niri's `custom_{resize,close,open}`), each built
    /// from a config GLSL snippet by [`Self::set_custom_shader`] and `None` until one is set.
    custom_resize: Option<Pipeline>,
    custom_close: Option<Pipeline>,
    custom_open: Option<Pipeline>,
    sampler_set_layout: vk::DescriptorSetLayout,
    pub(super) command_pool: vk::CommandPool,
    downscale_filter: TextureFilter,
    upscale_filter: TextureFilter,
    debug_flags: DebugFlags,
}

impl VulkanRenderer {
    /// Bring up a fresh device (Venus/lavapipe depending on `VK_DRIVER_FILES`) and build the
    /// renderer. Returns an error (rather than panicking) if no usable Vulkan device is present.
    pub fn new() -> Result<Self, VulkanError> {
        let gpu = Arc::new(Gpu::new()?);
        Self::with_gpu(gpu)
    }

    fn with_gpu(gpu: Arc<Gpu>) -> Result<Self, VulkanError> {
        let render_pass = create_render_pass(&gpu.device)?;
        let sampler_set_layout = sampler_set_layout(&gpu)?;
        let quad_push = std::mem::size_of::<QuadPush>() as u32;
        let sampler = std::slice::from_ref(&sampler_set_layout);
        // Straight-alpha materials (output non-premultiplied color).
        let solid_pipeline = build_pipeline(
            &gpu,
            render_pass,
            QUAD_VERT,
            SOLID_FRAG,
            &[],
            quad_push,
            false,
        )?;
        let texture_pipeline = build_pipeline(
            &gpu,
            render_pass,
            QUAD_VERT,
            TEX_FRAG,
            sampler,
            quad_push,
            false,
        )?;
        let rounded_texture_pipeline = build_pipeline(
            &gpu,
            render_pass,
            QUAD_VERT,
            ROUNDED_TEX_FRAG,
            sampler,
            quad_push,
            false,
        )?;
        let gradient_fade_pipeline = build_pipeline(
            &gpu,
            render_pass,
            QUAD_VERT,
            GRADIENT_FADE_FRAG,
            sampler,
            quad_push,
            false,
        )?;
        // The border/shadow materials output premultiplied color and sample nothing (no set).
        let border_pipeline = build_pipeline(
            &gpu,
            render_pass,
            BORDER_VERT,
            BORDER_FRAG,
            &[],
            std::mem::size_of::<BorderPush>() as u32,
            true,
        )?;
        let shadow_pipeline = build_pipeline(
            &gpu,
            render_pass,
            SHADOW_VERT,
            SHADOW_FRAG,
            &[],
            std::mem::size_of::<ShadowPush>() as u32,
            true,
        )?;
        // Postprocess-and-clip samples a texture (set 0) and outputs premultiplied color.
        let postprocess_pipeline = build_pipeline(
            &gpu,
            render_pass,
            POSTPROCESS_VERT,
            POSTPROCESS_FRAG,
            sampler,
            std::mem::size_of::<PostprocessPush>() as u32,
            true,
        )?;
        // Resize cross-fade samples two textures (set 0 = prev, set 1 = next), premultiplied out.
        let resize_pipeline = build_pipeline(
            &gpu,
            render_pass,
            RESIZE_VERT,
            RESIZE_FRAG,
            &[sampler_set_layout, sampler_set_layout],
            std::mem::size_of::<ResizePush>() as u32,
            true,
        )?;
        let command_pool = {
            let ci = vk::CommandPoolCreateInfo::default()
                .queue_family_index(gpu.queue_family)
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
            unsafe { gpu.device.create_command_pool(&ci, None) }?
        };

        Ok(VulkanRenderer {
            gpu,
            context_id: ContextId::new(),
            render_pass,
            solid_pipeline,
            texture_pipeline,
            rounded_texture_pipeline,
            gradient_fade_pipeline,
            border_pipeline,
            shadow_pipeline,
            postprocess_pipeline,
            resize_pipeline,
            custom_resize: None,
            custom_close: None,
            custom_open: None,
            sampler_set_layout,
            command_pool,
            downscale_filter: TextureFilter::Linear,
            upscale_filter: TextureFilter::Linear,
            debug_flags: DebugFlags::empty(),
        })
    }

    /// The device this renderer runs on (e.g. `"Virtio-GPU Venus (Apple M4 Pro)"`).
    pub fn device_name(&self) -> &str {
        &self.gpu.device_name
    }

    /// A clone of this renderer's context identity, shared by its frames.
    pub(super) fn ctx_id(&self) -> ContextId<VkTexture> {
        self.context_id.clone()
    }

    /// Compile a user animation shader from GLSL `src` and install it in the `ty` slot (or clear
    /// the slot with `None`), destroying the previous pipeline. The owned-renderer equivalent
    /// of niri's `set_custom_{resize,close,open}_program`: on a compile error it returns `Err`
    /// (with the glslang log) and leaves the previous slot untouched — a bad snippet never
    /// panics or replaces a working shader. The built-in resize crossfade lives separately in
    /// `render_resize`; this slot only holds user overrides.
    ///
    /// The live config path (feeding these) is Stage 3; today only the tests call this.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn set_custom_shader(
        &mut self,
        ty: CustomShaderType,
        src: Option<&str>,
    ) -> Result<(), VulkanError> {
        let new = match src {
            None => None,
            Some(src) => {
                let compiled = compile_custom(ty, src)?;
                // Guard the push budget: CustomResizePush is the first block over the 128-byte spec
                // minimum, so a clean error beats a pipeline-layout VUID on an exotic device.
                let max_push = unsafe {
                    self.gpu
                        .instance
                        .get_physical_device_properties(self.gpu.phys)
                }
                .limits
                .max_push_constants_size;
                if compiled.push_size > max_push {
                    return Err(VulkanError::CustomShader(format!(
                        "custom {ty:?} shader needs {} push-constant bytes, device allows {max_push}",
                        compiled.push_size,
                    )));
                }
                let set_layouts = vec![self.sampler_set_layout; compiled.sampler_count as usize];
                let pipeline = build_pipeline(
                    &self.gpu,
                    self.render_pass,
                    &compiled.vert_spv,
                    &compiled.frag_spv,
                    &set_layouts,
                    compiled.push_size,
                    true,
                )?;
                Some(pipeline)
            }
        };

        // Swap in the new slot value, then destroy the old pipeline. `&mut self` here means no
        // frame can be recording (a `VulkanFrame` borrows the renderer mutably) and every
        // submit fence-waits in `finish`, so the old pipeline is guaranteed idle — no
        // `device_wait_idle` needed.
        let old = std::mem::replace(self.custom_slot_mut(ty), new);
        if let Some(old) = old {
            unsafe { old.destroy(&self.gpu.device) };
        }
        Ok(())
    }

    fn custom_slot_mut(&mut self, ty: CustomShaderType) -> &mut Option<Pipeline> {
        match ty {
            CustomShaderType::Resize => &mut self.custom_resize,
            CustomShaderType::Close => &mut self.custom_close,
            CustomShaderType::Open => &mut self.custom_open,
        }
    }

    /// The compiled pipeline for a custom shader slot, if one is installed.
    pub(super) fn custom_pipeline(&self, ty: CustomShaderType) -> Option<&Pipeline> {
        match ty {
            CustomShaderType::Resize => self.custom_resize.as_ref(),
            CustomShaderType::Close => self.custom_close.as_ref(),
            CustomShaderType::Open => self.custom_open.as_ref(),
        }
    }

    /// Allocate a one-set descriptor pool and bind `tex`'s image+sampler at set 0, binding 0.
    fn make_texture_set(
        &self,
        tex: &NiriTexture,
    ) -> Result<(vk::DescriptorPool, vk::DescriptorSet), VulkanError> {
        let dev = &self.gpu.device;
        let sizes = [vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)];
        let pool = unsafe {
            dev.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(1)
                    .pool_sizes(&sizes),
                None,
            )
        }?;
        let layouts = [self.sampler_set_layout];
        let set = unsafe {
            dev.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(pool)
                    .set_layouts(&layouts),
            )
        }?[0];
        let image_info = [vk::DescriptorImageInfo::default()
            .sampler(tex.sampler)
            .image_view(tex.view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
        let write = vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(&image_info);
        unsafe { dev.update_descriptor_sets(std::slice::from_ref(&write), &[]) };
        Ok((pool, set))
    }

    /// Copy a `w×h` region of `tex`'s image into a host `Vec<u8>` of tight RGBA8. Used by
    /// [`ExportMem::copy_framebuffer`]. Transitions the image to `TRANSFER_SRC_OPTIMAL` first if
    /// the tracked layout says it is elsewhere (e.g. `SHADER_READ_ONLY_OPTIMAL` after it was
    /// sampled).
    fn download_region(
        &self,
        tex: &VkTexture,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>, VulkanError> {
        let dev = &self.gpu.device;
        let size = (w as vk::DeviceSize) * (h as vk::DeviceSize) * 4;
        let image = tex.image();
        let old_layout = tex.layout();

        let buf_ci = vk::BufferCreateInfo::default()
            .size(size)
            .usage(vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let buffer = unsafe { dev.create_buffer(&buf_ci, None) }?;
        let req = unsafe { dev.get_buffer_memory_requirements(buffer) };
        let mem = self.gpu.allocate(
            req,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )?;
        unsafe { dev.bind_buffer_memory(buffer, mem, 0) }?;

        self.gpu.run_commands(self.command_pool, |cbuf| unsafe {
            if old_layout != vk::ImageLayout::TRANSFER_SRC_OPTIMAL {
                transition_image(
                    dev,
                    cbuf,
                    image,
                    old_layout,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    vk::AccessFlags::TRANSFER_READ,
                    vk::PipelineStageFlags::TRANSFER,
                );
            }
            let region = vk::BufferImageCopy::default()
                .image_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                })
                .image_offset(vk::Offset3D { x, y, z: 0 })
                .image_extent(vk::Extent3D {
                    width: w,
                    height: h,
                    depth: 1,
                });
            dev.cmd_copy_image_to_buffer(
                cbuf,
                image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                buffer,
                &[region],
            );
            let host = vk::MemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ);
            dev.cmd_pipeline_barrier(
                cbuf,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::HOST,
                vk::DependencyFlags::empty(),
                &[host],
                &[],
                &[],
            );
        })?;
        tex.set_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL);

        let mut pixels = vec![0u8; size as usize];
        unsafe {
            let ptr = dev.map_memory(mem, 0, size, vk::MemoryMapFlags::empty())? as *const u8;
            std::ptr::copy_nonoverlapping(ptr, pixels.as_mut_ptr(), size as usize);
            dev.unmap_memory(mem);
            dev.destroy_buffer(buffer, None);
            dev.free_memory(mem, None);
        }
        Ok(pixels)
    }

    /// Transition an offscreen [`VkTexture`] into `SHADER_READ_ONLY_OPTIMAL` so it can be sampled
    /// after being rendered into (the offscreen-snapshot / blur / clipped-surface bridge). No-op if
    /// it is already sampleable. Runs as its own fence-waited submission, matching this renderer's
    /// synchronous per-submit model; call it once, between finishing the offscreen render and using
    /// the texture as a draw source. Reached generically via
    /// [`crate::render_helpers::renderer::OffscreenRenderer::make_offscreen_sampleable`].
    pub(crate) fn make_sampleable(&self, tex: &VkTexture) -> Result<(), VulkanError> {
        let old_layout = tex.layout();
        if old_layout == vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL {
            return Ok(());
        }
        let image = tex.image();
        self.gpu.run_commands(self.command_pool, |cbuf| unsafe {
            transition_image(
                &self.gpu.device,
                cbuf,
                image,
                old_layout,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                vk::AccessFlags::SHADER_READ,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
            );
        })?;
        tex.set_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        Ok(())
    }

    /// Blur `source` with the dual-kawase [`BlurChain`] and return the result as a fresh,
    /// sampleable offscreen [`VkTexture`] the same size as `source` — the owned-renderer
    /// equivalent of niri's GLES `Blur` (the `FramebufferEffectElement` backdrop blur).
    /// `source` must be sampleable (`SHADER_READ_ONLY_OPTIMAL`): imported textures are; an
    /// offscreen must go through [`Self::make_sampleable`] first.
    ///
    /// Builds a transient blur chain per call (unoptimized — the render pass, pipelines and level
    /// pyramid are rebuilt each time); the eventual live `FramebufferEffectElement` consumer will
    /// cache it. The chain records the down/up passes plus a copy of its output into `output`, then
    /// this fence-waits and hands back `output` in `SHADER_READ_ONLY_OPTIMAL`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn render_blur(
        &mut self,
        source: &VkTexture,
        options: BlurOptions,
    ) -> Result<VkTexture, VulkanError> {
        let (w, h) = source.extent();
        let output = self.create_buffer(Fourcc::Abgr8888, Size::from((w as i32, h as i32)))?;

        let gpu = self.gpu.clone();
        let pool = self.command_pool;
        let passes = (options.passes as usize).clamp(1, 31);
        let chain = BlurChain::new(&gpu, source.niri_texture(), passes)?;
        let recorded = gpu.run_commands(pool, |cbuf| {
            chain.record(&gpu, cbuf, options.offset as f32);
            chain.copy_output_to(&gpu, cbuf, output.image(), w, h);
        });
        // Free the transient chain regardless of whether recording/submission succeeded.
        chain.destroy(&gpu);
        recorded?;

        output.set_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        Ok(output)
    }
}

/// The `(src_access, src_stage)` masks for a layout transition *out of* `old` — the write/stage
/// that must complete before the new layout is usable. Covers the layouts an offscreen
/// [`VkTexture`] passes through in this renderer's synchronous lifecycle.
fn src_masks_for(old: vk::ImageLayout) -> (vk::AccessFlags, vk::PipelineStageFlags) {
    match old {
        vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL => (
            vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
            vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
        ),
        vk::ImageLayout::TRANSFER_SRC_OPTIMAL => (
            vk::AccessFlags::TRANSFER_READ,
            vk::PipelineStageFlags::TRANSFER,
        ),
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL => (
            vk::AccessFlags::SHADER_READ,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
        ),
        // UNDEFINED (and anything else): no prior contents to preserve.
        _ => (
            vk::AccessFlags::empty(),
            vk::PipelineStageFlags::TOP_OF_PIPE,
        ),
    }
}

/// Record a single-image layout transition barrier into `cbuf`. Prior hazards are already resolved
/// by this renderer's fence-per-submit model, so the source masks come only from `old`'s layout.
#[allow(clippy::too_many_arguments)]
unsafe fn transition_image(
    dev: &ash::Device,
    cbuf: vk::CommandBuffer,
    image: vk::Image,
    old: vk::ImageLayout,
    new: vk::ImageLayout,
    dst_access: vk::AccessFlags,
    dst_stage: vk::PipelineStageFlags,
) {
    let (src_access, src_stage) = src_masks_for(old);
    let barrier = vk::ImageMemoryBarrier::default()
        .old_layout(old)
        .new_layout(new)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(COLOR_RANGE)
        .src_access_mask(src_access)
        .dst_access_mask(dst_access);
    dev.cmd_pipeline_barrier(
        cbuf,
        src_stage,
        dst_stage,
        vk::DependencyFlags::empty(),
        &[],
        &[],
        std::slice::from_ref(&barrier),
    );
}

impl Drop for VulkanRenderer {
    fn drop(&mut self) {
        unsafe {
            let dev = &self.gpu.device;
            let _ = dev.device_wait_idle();
            self.solid_pipeline.destroy(dev);
            self.texture_pipeline.destroy(dev);
            self.rounded_texture_pipeline.destroy(dev);
            self.gradient_fade_pipeline.destroy(dev);
            self.border_pipeline.destroy(dev);
            self.shadow_pipeline.destroy(dev);
            self.postprocess_pipeline.destroy(dev);
            self.resize_pipeline.destroy(dev);
            // Custom pipelines' layouts reference the shared sampler set layout, so free them
            // first.
            for pipeline in [&self.custom_resize, &self.custom_close, &self.custom_open]
                .into_iter()
                .flatten()
            {
                pipeline.destroy(dev);
            }
            dev.destroy_descriptor_set_layout(self.sampler_set_layout, None);
            dev.destroy_render_pass(self.render_pass, None);
            dev.destroy_command_pool(self.command_pool, None);
        }
    }
}

impl fmt::Debug for VulkanRenderer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VulkanRenderer")
            .field("device", &self.gpu.device_name)
            .field("debug_flags", &self.debug_flags)
            .finish()
    }
}

impl RendererSuper for VulkanRenderer {
    type Error = VulkanError;
    type TextureId = VkTexture;
    type Framebuffer<'buffer> = VkFramebuffer<'buffer>;
    type Frame<'frame, 'buffer>
        = VulkanFrame<'frame, 'buffer>
    where
        'buffer: 'frame;
}

impl Renderer for VulkanRenderer {
    fn context_id(&self) -> ContextId<VkTexture> {
        self.context_id.clone()
    }

    fn downscale_filter(&mut self, filter: TextureFilter) -> Result<(), VulkanError> {
        self.downscale_filter = filter;
        Ok(())
    }

    fn upscale_filter(&mut self, filter: TextureFilter) -> Result<(), VulkanError> {
        self.upscale_filter = filter;
        Ok(())
    }

    fn set_debug_flags(&mut self, flags: DebugFlags) {
        self.debug_flags = flags;
    }

    fn debug_flags(&self) -> DebugFlags {
        self.debug_flags
    }

    fn render<'frame, 'buffer>(
        &'frame mut self,
        framebuffer: &'frame mut VkFramebuffer<'buffer>,
        output_size: Size<i32, Physical>,
        dst_transform: Transform,
    ) -> Result<VulkanFrame<'frame, 'buffer>, VulkanError>
    where
        'buffer: 'frame,
    {
        VulkanFrame::begin(self, framebuffer, output_size, dst_transform)
    }

    fn wait(&mut self, sync: &SyncPoint) -> Result<(), VulkanError> {
        sync.wait().map_err(|_| VulkanError::SyncInterrupted)
    }
}

impl Bind<VkTexture> for VulkanRenderer {
    fn bind<'a>(&mut self, target: &'a mut VkTexture) -> Result<VkFramebuffer<'a>, VulkanError> {
        // Only offscreen textures (created by `create_buffer`) carry a render-pass framebuffer.
        if target.framebuffer().is_none() {
            return Err(VulkanError::Unsupported(
                "binding an imported (non-renderable) texture as a target",
            ));
        }
        Ok(VkFramebuffer { buffer: target })
    }
}

impl Offscreen<VkTexture> for VulkanRenderer {
    fn create_buffer(
        &mut self,
        format: Fourcc,
        size: Size<i32, BufferCoord>,
    ) -> Result<VkTexture, VulkanError> {
        if !is_rgba8888(format) {
            return Err(VulkanError::UnsupportedFormat(format));
        }
        let (w, h) = (size.w.max(0) as u32, size.h.max(0) as u32);
        let filter = match self.upscale_filter {
            TextureFilter::Linear => vk::Filter::LINEAR,
            TextureFilter::Nearest => vk::Filter::NEAREST,
        };

        // A blank, sampleable color-attachment image (see `Texture::new_color_target`), plus a
        // render-pass framebuffer over its view and a descriptor set so it can be re-sampled once
        // rendered into — the offscreen-snapshot / blur / clipped-surface bridge.
        let tex = NiriTexture::new_color_target(&self.gpu, w, h, filter)?;
        let (desc_pool, set) = self.make_texture_set(&tex)?;

        let fb_ci = vk::FramebufferCreateInfo::default()
            .render_pass(self.render_pass)
            .attachments(std::slice::from_ref(&tex.view))
            .width(w)
            .height(h)
            .layers(1);
        let framebuffer = unsafe { self.gpu.device.create_framebuffer(&fb_ci, None) }?;

        Ok(VkTexture::new_offscreen(
            self.gpu.clone(),
            tex,
            desc_pool,
            set,
            framebuffer,
            w,
            h,
            format,
        ))
    }
}

impl ImportMem for VulkanRenderer {
    fn import_memory(
        &mut self,
        data: &[u8],
        format: Fourcc,
        size: Size<i32, BufferCoord>,
        flipped: bool,
    ) -> Result<VkTexture, VulkanError> {
        if !is_rgba8888(format) {
            return Err(VulkanError::UnsupportedFormat(format));
        }
        let (w, h) = (size.w.max(0) as u32, size.h.max(0) as u32);
        let filter = match self.upscale_filter {
            TextureFilter::Linear => vk::Filter::LINEAR,
            TextureFilter::Nearest => vk::Filter::NEAREST,
        };
        let tex = NiriTexture::from_rgba(&self.gpu, self.command_pool, w, h, data, filter)?;
        let (desc_pool, set) = self.make_texture_set(&tex)?;
        Ok(VkTexture::new(
            self.gpu.clone(),
            tex,
            desc_pool,
            set,
            w,
            h,
            format,
            flipped,
        ))
    }

    fn update_memory(
        &mut self,
        _texture: &VkTexture,
        _data: &[u8],
        _region: Rectangle<i32, BufferCoord>,
    ) -> Result<(), VulkanError> {
        Err(VulkanError::Unsupported("update_memory"))
    }

    fn mem_formats(&self) -> Box<dyn Iterator<Item = Fourcc>> {
        Box::new([Fourcc::Abgr8888, Fourcc::Xbgr8888].into_iter())
    }
}

impl ExportMem for VulkanRenderer {
    type TextureMapping = VkMapping;

    fn copy_framebuffer(
        &mut self,
        target: &VkFramebuffer<'_>,
        region: Rectangle<i32, BufferCoord>,
        format: Fourcc,
    ) -> Result<VkMapping, VulkanError> {
        if !is_rgba8888(format) {
            return Err(VulkanError::UnsupportedFormat(format));
        }
        let w = region.size.w.max(0) as u32;
        let h = region.size.h.max(0) as u32;
        let data = self.download_region(&*target.buffer, region.loc.x, region.loc.y, w, h)?;
        Ok(VkMapping {
            data,
            width: w,
            height: h,
            format,
        })
    }

    fn copy_texture(
        &mut self,
        _texture: &VkTexture,
        _region: Rectangle<i32, BufferCoord>,
        _format: Fourcc,
    ) -> Result<VkMapping, VulkanError> {
        Err(VulkanError::Unsupported("copy_texture"))
    }

    fn can_read_texture(&mut self, _texture: &VkTexture) -> Result<bool, VulkanError> {
        Ok(false)
    }

    fn map_texture<'a>(&mut self, texture_mapping: &'a VkMapping) -> Result<&'a [u8], VulkanError> {
        Ok(&texture_mapping.data)
    }
}

/// The offscreen render pass: one `R8G8B8A8_UNORM` color attachment, contents discarded on load
/// (callers clear explicitly) and left in `TRANSFER_SRC_OPTIMAL` so [`ExportMem`] can read it back.
fn create_render_pass(dev: &ash::Device) -> Result<vk::RenderPass, VulkanError> {
    let attachment = vk::AttachmentDescription::default()
        .format(IMAGE_VK_FORMAT)
        .samples(vk::SampleCountFlags::TYPE_1)
        .load_op(vk::AttachmentLoadOp::DONT_CARE)
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
    unsafe { dev.create_render_pass(&ci, None) }.map_err(VulkanError::from)
}

/// Build a `vert_spv` + `frag_spv` pipeline with dynamic viewport/scissor against `render_pass`.
/// `push_size` is the pipeline's push-constant range size; `premultiplied` selects the source
/// color blend factor (`ONE` for shaders that output premultiplied color like the border/shadow
/// materials, `SRC_ALPHA` for straight-alpha materials like solid/texture).
fn build_pipeline(
    gpu: &Gpu,
    render_pass: vk::RenderPass,
    vert_spv: &[u8],
    frag_spv: &[u8],
    set_layouts: &[vk::DescriptorSetLayout],
    push_size: u32,
    premultiplied: bool,
) -> Result<Pipeline, VulkanError> {
    let dev = &gpu.device;
    let vert = load_module(dev, vert_spv)?;
    let frag = load_module(dev, frag_spv)?;

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

    // Viewport/scissor are set dynamically per frame; these placeholders just fix the counts to 1.
    let viewports = [vk::Viewport::default()];
    let scissors = [vk::Rect2D::default()];
    let viewport_state = vk::PipelineViewportStateCreateInfo::default()
        .viewports(&viewports)
        .scissors(&scissors);
    let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
    let dynamic_state =
        vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);

    let raster = vk::PipelineRasterizationStateCreateInfo::default()
        .polygon_mode(vk::PolygonMode::FILL)
        .cull_mode(vk::CullModeFlags::NONE)
        .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
        .line_width(1.0);
    let multisample = vk::PipelineMultisampleStateCreateInfo::default()
        .rasterization_samples(vk::SampleCountFlags::TYPE_1);

    let src_color_factor = if premultiplied {
        vk::BlendFactor::ONE
    } else {
        vk::BlendFactor::SRC_ALPHA
    };
    let blend_attachment = vk::PipelineColorBlendAttachmentState::default()
        .blend_enable(true)
        .src_color_blend_factor(src_color_factor)
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
        .size(push_size);
    let layout = unsafe {
        dev.create_pipeline_layout(
            &vk::PipelineLayoutCreateInfo::default()
                .set_layouts(set_layouts)
                .push_constant_ranges(std::slice::from_ref(&push_range)),
            None,
        )
    }?;

    let pipeline_ci = vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vertex_input)
        .input_assembly_state(&input_assembly)
        .viewport_state(&viewport_state)
        .rasterization_state(&raster)
        .multisample_state(&multisample)
        .color_blend_state(&blend)
        .dynamic_state(&dynamic_state)
        .layout(layout)
        .render_pass(render_pass)
        .subpass(0);
    let pipeline =
        unsafe { dev.create_graphics_pipelines(vk::PipelineCache::null(), &[pipeline_ci], None) }
            .map_err(|(_, e)| VulkanError::from(e))?[0];

    Ok(Pipeline {
        pipeline,
        layout,
        vert,
        frag,
    })
}
