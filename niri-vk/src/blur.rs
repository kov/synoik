//! Dual-Kawase blur chain, ported from niri's GLES `render_helpers/blur.rs`.
//!
//! A pyramid of `passes + 1` levels halving in size: level 0 is full-size (the output), levels
//! 1..=passes shrink. Downsample folds `source → L1 → … → Lₙ`; upsample folds
//! `Lₙ → … → L1 → L0`. Each pass is its own render pass writing one level while sampling another;
//! the render pass leaves each level `SHADER_READ_ONLY_OPTIMAL` and its subpass dependencies order
//! the write of one pass before the sample of the next (and the final write before readback).
//!
//! Blending is off — every pass fully overwrites its destination. Sampling is LINEAR + clamp,
//! matching the reference. `half_pixel` is half a *destination* pixel on the way down and half a
//! *source* pixel on the way up (both in the sampled texture's 0..1 UV space), exactly as niri.
//!
//! Resources are freed explicitly in [`BlurChain::destroy`] (no `Drop` — teardown needs a `&Gpu`).
//! [`BlurChain::new`] unwinds a *failed* build via an internal guard (the compositor now rebuilds a
//! chain at runtime — per output/size/pass change — where an error propagates instead of aborting
//! `main`, so a partial build must not leak). `read_output`'s `?` paths still leak on failure, but
//! that helper is test-only.

use anyhow::{Context, Result};
use ash::vk;

use crate::gpu::Gpu;
use crate::render::{as_bytes, load_module, RENDER_FORMAT};
use crate::texture::Texture;

/// Push constants for the blur taps (matches the GLSL `Push` block; `offset` at 8, 16 bytes).
#[repr(C)]
#[derive(Clone, Copy)]
struct BlurPush {
    half_pixel: [f32; 2],
    offset: f32,
    _pad0: f32,
}

struct Level {
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    framebuffer: vk::Framebuffer,
    /// Descriptor set that samples *this* level (used when this level is a pass's source).
    set: vk::DescriptorSet,
    w: u32,
    h: u32,
}

pub struct BlurChain {
    render_pass: vk::RenderPass,
    sampler: vk::Sampler,
    set_layout: vk::DescriptorSetLayout,
    desc_pool: vk::DescriptorPool,
    pipeline_layout: vk::PipelineLayout,
    down: vk::Pipeline,
    up: vk::Pipeline,
    vert: vk::ShaderModule,
    down_frag: vk::ShaderModule,
    up_frag: vk::ShaderModule,
    /// Level 0 is full-size (the output); levels 1..=passes shrink.
    levels: Vec<Level>,
    /// Descriptor set that samples the external source texture.
    source_set: vk::DescriptorSet,
    passes: usize,
}

const FULLSCREEN_VERT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/fullscreen.vert.spv"));
const DOWN_FRAG: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/blur_down.frag.spv"));
const UP_FRAG: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/blur_up.frag.spv"));

/// Unwind guard for [`BlurChain::new`]: destroys every handle created so far if the build fails
/// partway (each field starts null — `vkDestroy*` is a no-op on a null handle — and is filled as
/// `new` progresses). `armed` is cleared once `new` succeeds and moves the handles into the
/// returned chain. Mirrors [`BlurChain::destroy`]'s teardown order.
struct NewGuard<'a> {
    device: &'a ash::Device,
    armed: bool,
    render_pass: vk::RenderPass,
    sampler: vk::Sampler,
    set_layout: vk::DescriptorSetLayout,
    desc_pool: vk::DescriptorPool,
    pipeline_layout: vk::PipelineLayout,
    down: vk::Pipeline,
    up: vk::Pipeline,
    vert: vk::ShaderModule,
    down_frag: vk::ShaderModule,
    up_frag: vk::ShaderModule,
    levels: Vec<Level>,
}

impl<'a> NewGuard<'a> {
    fn new(device: &'a ash::Device) -> Self {
        NewGuard {
            device,
            armed: true,
            render_pass: vk::RenderPass::null(),
            sampler: vk::Sampler::null(),
            set_layout: vk::DescriptorSetLayout::null(),
            desc_pool: vk::DescriptorPool::null(),
            pipeline_layout: vk::PipelineLayout::null(),
            down: vk::Pipeline::null(),
            up: vk::Pipeline::null(),
            vert: vk::ShaderModule::null(),
            down_frag: vk::ShaderModule::null(),
            up_frag: vk::ShaderModule::null(),
            levels: Vec::new(),
        }
    }
}

impl Drop for NewGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let d = self.device;
        unsafe {
            for level in &self.levels {
                d.destroy_framebuffer(level.framebuffer, None);
                d.destroy_image_view(level.view, None);
                d.destroy_image(level.image, None);
                d.free_memory(level.memory, None);
            }
            d.destroy_pipeline(self.down, None);
            d.destroy_pipeline(self.up, None);
            d.destroy_pipeline_layout(self.pipeline_layout, None);
            d.destroy_shader_module(self.vert, None);
            d.destroy_shader_module(self.down_frag, None);
            d.destroy_shader_module(self.up_frag, None);
            d.destroy_descriptor_pool(self.desc_pool, None);
            d.destroy_descriptor_set_layout(self.set_layout, None);
            d.destroy_sampler(self.sampler, None);
            d.destroy_render_pass(self.render_pass, None);
        }
    }
}

impl BlurChain {
    /// Build the chain to blur `source` (which stays owned by the caller). `passes` is clamped to
    /// at least 1; `source` must be full-size (matches level 0).
    pub fn new(gpu: &Gpu, source: &Texture, passes: usize) -> Result<Self> {
        let _timed = crate::stats::creating();
        let device = &gpu.device;
        let passes = passes.max(1);
        let (width, height) = (source.width, source.height);

        // The compositor rebuilds a chain at runtime (per output/size/pass-count change — see the
        // `BackdropBlur` cache), where a failure no longer aborts the process, so a mid-build error
        // must unwind instead of leaking. Accumulate every created handle in the guard; `disarm` on
        // success. (`vkDestroy*` is a no-op on the null handles the guard starts with.)
        let mut guard = NewGuard::new(device);

        guard.render_pass = create_blur_render_pass(device)?;

        let sampler_ci = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::LINEAR)
            .min_filter(vk::Filter::LINEAR)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE);
        guard.sampler =
            unsafe { device.create_sampler(&sampler_ci, None) }.context("blur sampler")?;

        guard.set_layout = crate::render::sampler_set_layout(gpu)?;

        // One descriptor set per level + one for the external source.
        let count = (passes + 2) as u32;
        let sizes = [vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(count)];
        let dp_ci = vk::DescriptorPoolCreateInfo::default()
            .max_sets(count)
            .pool_sizes(&sizes);
        guard.desc_pool =
            unsafe { device.create_descriptor_pool(&dp_ci, None) }.context("blur desc pool")?;

        // Pipeline layout: set 0 = sampler, push constants = BlurPush (fragment).
        let push = vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(std::mem::size_of::<BlurPush>() as u32);
        let layout_ci = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(std::slice::from_ref(&guard.set_layout))
            .push_constant_ranges(std::slice::from_ref(&push));
        guard.pipeline_layout =
            unsafe { device.create_pipeline_layout(&layout_ci, None) }.context("blur layout")?;

        guard.vert = load_module(device, FULLSCREEN_VERT)?;
        guard.down_frag = load_module(device, DOWN_FRAG)?;
        guard.up_frag = load_module(device, UP_FRAG)?;
        guard.down = build_pipeline(
            gpu,
            guard.render_pass,
            guard.pipeline_layout,
            guard.vert,
            guard.down_frag,
        )?;
        guard.up = build_pipeline(
            gpu,
            guard.render_pass,
            guard.pipeline_layout,
            guard.vert,
            guard.up_frag,
        )?;

        // Create the level pyramid.
        let (mut w, mut h) = (width, height);
        for _ in 0..=passes {
            let level = create_level(
                gpu,
                guard.render_pass,
                guard.sampler,
                guard.set_layout,
                guard.desc_pool,
                w,
                h,
            )?;
            guard.levels.push(level);
            w = (w / 2).max(1);
            h = (h / 2).max(1);
        }

        // Descriptor set that samples the external source (freed with `desc_pool`, no separate
        // destroy — so the guard already covers it).
        let source_set = alloc_sampler_set(
            gpu,
            guard.desc_pool,
            guard.set_layout,
            source.view,
            source.sampler,
        )?;

        guard.armed = false;
        Ok(BlurChain {
            render_pass: guard.render_pass,
            sampler: guard.sampler,
            set_layout: guard.set_layout,
            desc_pool: guard.desc_pool,
            pipeline_layout: guard.pipeline_layout,
            down: guard.down,
            up: guard.up,
            vert: guard.vert,
            down_frag: guard.down_frag,
            up_frag: guard.up_frag,
            levels: std::mem::take(&mut guard.levels),
            source_set,
            passes,
        })
    }

    /// Record the full down+up chain into `cbuf`. Afterwards level 0 holds the blurred output in
    /// `SHADER_READ_ONLY_OPTIMAL`.
    pub fn record(&self, gpu: &Gpu, cbuf: vk::CommandBuffer, offset: f32) {
        self.record_to_level(gpu, cbuf, offset, 0);
    }

    /// As [`Self::record`], but stop the upsample once `stop` holds the result instead of carrying
    /// it all the way to level 0.
    ///
    /// Level 0 is the only full-size level, so it alone costs more than the whole rest of the
    /// pyramid; stopping at level 1 leaves a quarter-size result for the consumer's linear sampler
    /// to finish. Exposed for the cost measurement in this module's tests, which is what decides
    /// whether that trade is worth taking.
    pub fn record_to_level(&self, gpu: &Gpu, cbuf: vk::CommandBuffer, offset: f32, stop: usize) {
        // Downsample: source → L1, L1 → L2, …, L_{passes-1} → L_passes.
        for i in 1..=self.passes {
            let dst = &self.levels[i];
            let src_set = if i == 1 {
                self.source_set
            } else {
                self.levels[i - 1].set
            };
            // half_pixel = half a destination pixel.
            let half_pixel = [0.5 / dst.w as f32, 0.5 / dst.h as f32];
            self.pass(gpu, cbuf, self.down, dst, src_set, half_pixel, offset);
        }

        // Upsample: L_passes → L_{passes-1}, …, L_{stop+1} → L_stop.
        for i in (stop + 1..=self.passes).rev() {
            let src = &self.levels[i];
            let dst = &self.levels[i - 1];
            // half_pixel = half a source pixel.
            let half_pixel = [0.5 / src.w as f32, 0.5 / src.h as f32];
            self.pass(gpu, cbuf, self.up, dst, src.set, half_pixel, offset);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn pass(
        &self,
        gpu: &Gpu,
        cbuf: vk::CommandBuffer,
        pipeline: vk::Pipeline,
        dst: &Level,
        src_set: vk::DescriptorSet,
        half_pixel: [f32; 2],
        offset: f32,
    ) {
        let device = &gpu.device;
        let extent = vk::Extent2D {
            width: dst.w,
            height: dst.h,
        };
        let begin = vk::RenderPassBeginInfo::default()
            .render_pass(self.render_pass)
            .framebuffer(dst.framebuffer)
            .render_area(vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent,
            });
        let push = BlurPush {
            half_pixel,
            offset,
            _pad0: 0.0,
        };
        let viewport = vk::Viewport {
            x: 0.0,
            y: 0.0,
            width: dst.w as f32,
            height: dst.h as f32,
            min_depth: 0.0,
            max_depth: 1.0,
        };
        let scissor = vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent,
        };
        unsafe {
            device.cmd_begin_render_pass(cbuf, &begin, vk::SubpassContents::INLINE);
            device.cmd_set_viewport(cbuf, 0, &[viewport]);
            device.cmd_set_scissor(cbuf, 0, &[scissor]);
            device.cmd_bind_pipeline(cbuf, vk::PipelineBindPoint::GRAPHICS, pipeline);
            device.cmd_bind_descriptor_sets(
                cbuf,
                vk::PipelineBindPoint::GRAPHICS,
                self.pipeline_layout,
                0,
                &[src_set],
                &[],
            );
            device.cmd_push_constants(
                cbuf,
                self.pipeline_layout,
                vk::ShaderStageFlags::FRAGMENT,
                0,
                as_bytes(&push),
            );
            device.cmd_draw(cbuf, 3, 1, 0, 0);
            // A blur pass shades its whole destination level.
            crate::stats::draw(u64::from(extent.width) * u64::from(extent.height));
            device.cmd_end_render_pass(cbuf);
        }
    }

    /// Transition level 0 to transfer-src and copy it back to host RGBA.
    pub fn read_output(&self, gpu: &Gpu, pool: vk::CommandPool) -> Result<Vec<u8>> {
        let device = &gpu.device;
        let out = &self.levels[0];
        let size = (out.w as vk::DeviceSize) * (out.h as vk::DeviceSize) * 4;

        let buf_ci = vk::BufferCreateInfo::default()
            .size(size)
            .usage(vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let buffer = unsafe { device.create_buffer(&buf_ci, None) }.context("blur readback buf")?;
        let req = unsafe { device.get_buffer_memory_requirements(buffer) };
        let mem = gpu.allocate(
            req,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )?;
        unsafe { device.bind_buffer_memory(buffer, mem, 0)? };

        gpu.run_commands(pool, crate::stats::SubmitSite::Blur, |cbuf| unsafe {
            // SHADER_READ_ONLY (render pass final layout) → TRANSFER_SRC. The producing access is
            // the final upsample pass's color-attachment write (L0 is never fragment-sampled), so
            // the source scope must name that — otherwise the transition is only ordered by the
            // submission boundary, and would race the write if readback were ever folded into the
            // chain submission.
            let to_src = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(out.image)
                .subresource_range(crate::render::COLOR_RANGE)
                .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ);
            device.cmd_pipeline_barrier(
                cbuf,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_src],
            );
            let region = vk::BufferImageCopy::default()
                .image_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                })
                .image_extent(vk::Extent3D {
                    width: out.w,
                    height: out.h,
                    depth: 1,
                });
            device.cmd_copy_image_to_buffer(
                cbuf,
                out.image,
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
                .map_memory(mem, 0, size, vk::MemoryMapFlags::empty())
                .context("map blur readback")? as *const u8;
            std::ptr::copy_nonoverlapping(ptr, pixels.as_mut_ptr(), size as usize);
            device.unmap_memory(mem);
            device.destroy_buffer(buffer, None);
            device.free_memory(mem, None);
        }
        Ok(pixels)
    }

    pub fn output_size(&self) -> (u32, u32) {
        (self.levels[0].w, self.levels[0].h)
    }

    /// Record a copy of the blurred output (level 0) into `dst` — a same-size, same-format image
    /// created with `TRANSFER_DST` (and expected in `UNDEFINED` layout) — leaving `dst` in
    /// `SHADER_READ_ONLY_OPTIMAL` so the caller can sample it. Records into `cbuf` after
    /// [`Self::record`], within the same submission. Used by the compositor to lift the blur result
    /// into a sampleable `VkTexture`. `dst_w`/`dst_h` must equal [`Self::output_size`].
    pub fn copy_output_to(
        &self,
        gpu: &Gpu,
        cbuf: vk::CommandBuffer,
        dst: vk::Image,
        dst_w: u32,
        dst_h: u32,
    ) {
        let device = &gpu.device;
        let out = &self.levels[0];
        unsafe {
            // Level 0: SHADER_READ_ONLY (render-pass final layout) → TRANSFER_SRC. The producing
            // access is the final upsample pass's color write (L0 is never fragment-sampled).
            let l0_to_src = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(out.image)
                .subresource_range(crate::render::COLOR_RANGE)
                .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                .dst_access_mask(vk::AccessFlags::TRANSFER_READ);
            // Destination: UNDEFINED → TRANSFER_DST (contents discarded; we overwrite fully).
            let dst_to_dst = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(dst)
                .subresource_range(crate::render::COLOR_RANGE)
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE);
            device.cmd_pipeline_barrier(
                cbuf,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT
                    | vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[l0_to_src, dst_to_dst],
            );

            let region = vk::ImageCopy::default()
                .src_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                })
                .dst_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                })
                .extent(vk::Extent3D {
                    width: dst_w,
                    height: dst_h,
                    depth: 1,
                });
            device.cmd_copy_image(
                cbuf,
                out.image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                dst,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[region],
            );

            // Destination: TRANSFER_DST → SHADER_READ_ONLY so the caller can sample it.
            let dst_to_sampled = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(dst)
                .subresource_range(crate::render::COLOR_RANGE)
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ);
            device.cmd_pipeline_barrier(
                cbuf,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[dst_to_sampled],
            );
        }
    }

    pub fn destroy(&self, gpu: &Gpu) {
        let d = &gpu.device;
        unsafe {
            for level in &self.levels {
                d.destroy_framebuffer(level.framebuffer, None);
                d.destroy_image_view(level.view, None);
                d.destroy_image(level.image, None);
                d.free_memory(level.memory, None);
            }
            d.destroy_pipeline(self.down, None);
            d.destroy_pipeline(self.up, None);
            d.destroy_pipeline_layout(self.pipeline_layout, None);
            d.destroy_shader_module(self.vert, None);
            d.destroy_shader_module(self.down_frag, None);
            d.destroy_shader_module(self.up_frag, None);
            d.destroy_descriptor_pool(self.desc_pool, None);
            d.destroy_descriptor_set_layout(self.set_layout, None);
            d.destroy_sampler(self.sampler, None);
            d.destroy_render_pass(self.render_pass, None);
        }
    }
}

fn create_blur_render_pass(device: &ash::Device) -> Result<vk::RenderPass> {
    // Contents are fully overwritten each pass, so load is DONT_CARE; final layout is shader-read
    // so the level can immediately serve as the next pass's source.
    let attachment = vk::AttachmentDescription::default()
        .format(RENDER_FORMAT)
        .samples(vk::SampleCountFlags::TYPE_1)
        .load_op(vk::AttachmentLoadOp::DONT_CARE)
        .store_op(vk::AttachmentStoreOp::STORE)
        .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
        .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .final_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
    let color_ref = vk::AttachmentReference::default()
        .attachment(0)
        .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);
    let subpass = vk::SubpassDescription::default()
        .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
        .color_attachments(std::slice::from_ref(&color_ref));
    let deps = [
        // A prior pass's write to (and sampling of) a level must finish before this pass touches
        // it — covers the read-after-write of the source and the write-after-read overwrite. The
        // TRANSFER scope also orders the previous frame's `copy_output_to` read of level 0 (a
        // transfer read, when a cached chain is re-recorded frame-over-frame) before this frame's
        // overwrite; without it that WAR would rest on the caller's per-submit fence alone.
        vk::SubpassDependency::default()
            .src_subpass(vk::SUBPASS_EXTERNAL)
            .dst_subpass(0)
            .src_stage_mask(
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT
                    | vk::PipelineStageFlags::FRAGMENT_SHADER
                    | vk::PipelineStageFlags::TRANSFER,
            )
            .src_access_mask(
                vk::AccessFlags::COLOR_ATTACHMENT_WRITE
                    | vk::AccessFlags::SHADER_READ
                    | vk::AccessFlags::TRANSFER_READ,
            )
            .dst_stage_mask(
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT
                    | vk::PipelineStageFlags::FRAGMENT_SHADER,
            )
            .dst_access_mask(
                vk::AccessFlags::COLOR_ATTACHMENT_WRITE | vk::AccessFlags::SHADER_READ,
            ),
        // This pass's write must be visible to a later sample or the final transfer read.
        vk::SubpassDependency::default()
            .src_subpass(0)
            .dst_subpass(vk::SUBPASS_EXTERNAL)
            .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
            .dst_stage_mask(
                vk::PipelineStageFlags::FRAGMENT_SHADER | vk::PipelineStageFlags::TRANSFER,
            )
            .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::TRANSFER_READ),
    ];
    let ci = vk::RenderPassCreateInfo::default()
        .attachments(std::slice::from_ref(&attachment))
        .subpasses(std::slice::from_ref(&subpass))
        .dependencies(&deps);
    unsafe { device.create_render_pass(&ci, None) }.context("blur render pass")
}

fn create_level(
    gpu: &Gpu,
    render_pass: vk::RenderPass,
    sampler: vk::Sampler,
    set_layout: vk::DescriptorSetLayout,
    desc_pool: vk::DescriptorPool,
    w: u32,
    h: u32,
) -> Result<Level> {
    let device = &gpu.device;
    let image_ci = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(RENDER_FORMAT)
        .extent(vk::Extent3D {
            width: w,
            height: h,
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
    let image = unsafe { device.create_image(&image_ci, None) }.context("blur level image")?;
    let req = unsafe { device.get_image_memory_requirements(image) };
    let memory = gpu.allocate(req, vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
    unsafe { device.bind_image_memory(image, memory, 0)? };

    let view_ci = vk::ImageViewCreateInfo::default()
        .image(image)
        .view_type(vk::ImageViewType::TYPE_2D)
        .format(RENDER_FORMAT)
        .subresource_range(crate::render::COLOR_RANGE);
    let view = unsafe { device.create_image_view(&view_ci, None) }.context("blur level view")?;

    let fb_ci = vk::FramebufferCreateInfo::default()
        .render_pass(render_pass)
        .attachments(std::slice::from_ref(&view))
        .width(w)
        .height(h)
        .layers(1);
    let framebuffer =
        unsafe { device.create_framebuffer(&fb_ci, None) }.context("blur level fb")?;

    let set = alloc_sampler_set(gpu, desc_pool, set_layout, view, sampler)?;

    Ok(Level {
        image,
        memory,
        view,
        framebuffer,
        set,
        w,
        h,
    })
}

fn alloc_sampler_set(
    gpu: &Gpu,
    desc_pool: vk::DescriptorPool,
    set_layout: vk::DescriptorSetLayout,
    view: vk::ImageView,
    sampler: vk::Sampler,
) -> Result<vk::DescriptorSet> {
    let device = &gpu.device;
    let alloc = vk::DescriptorSetAllocateInfo::default()
        .descriptor_pool(desc_pool)
        .set_layouts(std::slice::from_ref(&set_layout));
    let set = unsafe { device.allocate_descriptor_sets(&alloc) }.context("blur set")?[0];
    let img = vk::DescriptorImageInfo::default()
        .sampler(sampler)
        .image_view(view)
        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
    let write = vk::WriteDescriptorSet::default()
        .dst_set(set)
        .dst_binding(0)
        .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
        .image_info(std::slice::from_ref(&img));
    unsafe { device.update_descriptor_sets(&[write], &[]) };
    Ok(set)
}

fn build_pipeline(
    gpu: &Gpu,
    render_pass: vk::RenderPass,
    layout: vk::PipelineLayout,
    vert: vk::ShaderModule,
    frag: vk::ShaderModule,
) -> Result<vk::Pipeline> {
    let device = &gpu.device;
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
    // Viewport/scissor are dynamic because each level is a different size.
    let viewport_state = vk::PipelineViewportStateCreateInfo::default()
        .viewport_count(1)
        .scissor_count(1);
    let dynamic_states = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
    let dynamic = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic_states);
    let raster = vk::PipelineRasterizationStateCreateInfo::default()
        .polygon_mode(vk::PolygonMode::FILL)
        .cull_mode(vk::CullModeFlags::NONE)
        .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
        .line_width(1.0);
    let multisample = vk::PipelineMultisampleStateCreateInfo::default()
        .rasterization_samples(vk::SampleCountFlags::TYPE_1);
    // No blending — each pass overwrites its destination.
    let blend_attachment = vk::PipelineColorBlendAttachmentState::default()
        .blend_enable(false)
        .color_write_mask(vk::ColorComponentFlags::RGBA);
    let blend = vk::PipelineColorBlendStateCreateInfo::default()
        .attachments(std::slice::from_ref(&blend_attachment));

    let ci = vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vertex_input)
        .input_assembly_state(&input_assembly)
        .viewport_state(&viewport_state)
        .rasterization_state(&raster)
        .multisample_state(&multisample)
        .color_blend_state(&blend)
        .dynamic_state(&dynamic)
        .layout(layout)
        .render_pass(render_pass)
        .subpass(0);
    let pipeline =
        unsafe { device.create_graphics_pipelines(vk::PipelineCache::null(), &[ci], None) }
            .map_err(|(_, e)| e)
            .context("blur pipeline")?[0];
    Ok(pipeline)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::texture::Texture;

    /// How many times each variant is recorded into a single command buffer. One
    /// submit for the lot, so the per-submit round trip (§9.3 of
    /// `docs/fork/venus-cost.md`: ~1.2ms of guest-side polling, most of it
    /// `clock_nanosleep`) is paid once and divided away instead of landing on
    /// every sample.
    ///
    /// Override with `BLUR_COST_REPS=1` to measure the live shape instead — one
    /// blur per submit. Worth doing once to see *why* the default batches: at 1
    /// the readings stop being monotonic in size (a 8.3Mpx blur "costing" more
    /// than a 18.7Mpx one), because a lone wait carries several milliseconds of
    /// the polling tax and its 291us quantization. That noise is the reason a
    /// single live `N blur in Xms` line cannot be read as the blur's GPU cost.
    fn reps() -> u32 {
        std::env::var("BLUR_COST_REPS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8)
    }

    /// Passes the compositor actually configures (`niri_config::Blur::default`).
    const PASSES: usize = 3;
    const OFFSET: f32 = 3.0;

    /// Sizes to sweep. The last is roughly the 5.3x-output overview frame that
    /// §3.8 of `docs/fork/venus-cost.md` measured at `1 blur in 13.63ms`.
    const SIZES: &[(u32, u32)] = &[
        (1920, 1080),
        (2560, 1440),
        (3840, 2160),
        (4480, 2520),
        (5760, 3240),
        (7680, 4320),
        (8832, 4968),
    ];

    /// Where the blur's cost actually goes, so the optimization is chosen from
    /// measurement rather than from the pass count.
    ///
    /// Ignored because it needs the real device and takes seconds; run it with
    /// `cargo test -p niri-vk blur_cost -- --ignored --nocapture`.
    ///
    /// The three variants isolate the two candidate savings:
    ///
    /// - `record+copy` is what ships: the whole pyramid, then a full-size image copy lifting level
    ///   0 into the caller's sampleable texture.
    /// - `record` drops that copy — what rendering the last upsample *directly* into the caller's
    ///   texture would cost.
    /// - `record→L1` also stops the upsample one level short, leaving the result at quarter size
    ///   for the consumer's sampler to finish. It skips the single most expensive pass in the chain
    ///   (level 0 is the only full-size one) and would shrink the copy to a quarter if the copy
    ///   stayed.
    #[test]
    #[ignore = "measures blur cost on the real device; run explicitly"]
    fn blur_cost_by_variant_and_size() {
        let Ok(gpu) = Gpu::new() else {
            eprintln!("skipping blur_cost_by_variant_and_size: no Vulkan device");
            return;
        };
        let pool_ci = vk::CommandPoolCreateInfo::default().queue_family_index(gpu.queue_family);
        let pool = unsafe { gpu.device.create_command_pool(&pool_ci, None) }.expect("pool");

        // An empty submit, to subtract the round trip from every reading below.
        let empty = time_submit(&gpu, pool, |_| {});
        println!("empty submit (subtracted below): {:.3}ms", ms(empty));
        println!(
            "{:<12} {:>10} {:>10} {:>10} {:>10}",
            "size", "px", "record+copy", "record", "record→L1"
        );

        for &(w, h) in SIZES {
            let src = noise(w, h);
            let source =
                Texture::from_rgba(&gpu, pool, w, h, &src, vk::Filter::LINEAR).expect("source");
            let dst = Texture::new_color_target(&gpu, w, h, vk::Filter::LINEAR).expect("dst");
            let chain = BlurChain::new(&gpu, &source, PASSES).expect("chain");

            let full = per_rep(
                time_submit(&gpu, pool, |cbuf| {
                    for _ in 0..reps() {
                        chain.record(&gpu, cbuf, OFFSET);
                        chain.copy_output_to(&gpu, cbuf, dst.image, w, h);
                    }
                }),
                empty,
            );
            let no_copy = per_rep(
                time_submit(&gpu, pool, |cbuf| {
                    for _ in 0..reps() {
                        chain.record(&gpu, cbuf, OFFSET);
                    }
                }),
                empty,
            );
            let to_l1 = per_rep(
                time_submit(&gpu, pool, |cbuf| {
                    for _ in 0..reps() {
                        chain.record_to_level(&gpu, cbuf, OFFSET, 1);
                    }
                }),
                empty,
            );

            println!(
                "{:<12} {:>10.2} {:>9.3}ms {:>9.3}ms {:>9.3}ms",
                format!("{w}x{h}"),
                (w as f64) * (h as f64) / 1e6,
                ms(full),
                ms(no_copy),
                ms(to_l1),
            );

            chain.destroy(&gpu);
            dst.destroy(&gpu);
            source.destroy(&gpu);
        }

        unsafe { gpu.device.destroy_command_pool(pool, None) };
    }

    /// What the blur's *own submit* costs, as against riding a command buffer
    /// that was going to be submitted anyway.
    ///
    /// The compositor gives every blur its own fence-waited submission
    /// (`EffectBlur::run`, `BackdropBlur::run_blur`). That is a round trip, and
    /// on this stack a round trip is not free even when the work is: the wait
    /// polls in ~291us steps (`venus-cost.md` §11.2), so a wait rounds up. This
    /// prices the same total GPU work delivered both ways — N blurs in one
    /// submit, versus N blurs in N submits — which is exactly the choice between
    /// folding the blur into the frame's command buffer and leaving it standing
    /// alone.
    ///
    /// Run with `cargo test -p niri-vk blur_submit_overhead -- --ignored --nocapture`.
    #[test]
    #[ignore = "measures blur cost on the real device; run explicitly"]
    fn blur_submit_overhead() {
        let Ok(gpu) = Gpu::new() else {
            eprintln!("skipping blur_submit_overhead: no Vulkan device");
            return;
        };
        let pool_ci = vk::CommandPoolCreateInfo::default().queue_family_index(gpu.queue_family);
        let pool = unsafe { gpu.device.create_command_pool(&pool_ci, None) }.expect("pool");
        const N: u32 = 8;

        println!(
            "{:<12} {:>10} {:>12} {:>12} {:>12}",
            "size", "px", "1 submit", "N submits", "per-submit"
        );
        for &(w, h) in SIZES {
            let src = noise(w, h);
            let source =
                Texture::from_rgba(&gpu, pool, w, h, &src, vk::Filter::LINEAR).expect("source");
            let dst = Texture::new_color_target(&gpu, w, h, vk::Filter::LINEAR).expect("dst");
            let chain = BlurChain::new(&gpu, &source, PASSES).expect("chain");

            let one = |cbuf| {
                chain.record(&gpu, cbuf, OFFSET);
                chain.copy_output_to(&gpu, cbuf, dst.image, w, h);
            };
            // All N in one command buffer: the round trip paid once.
            let batched = time_submit(&gpu, pool, |cbuf| {
                for _ in 0..N {
                    one(cbuf);
                }
            });
            // The same N blurs, each with its own submit and fence wait — what
            // the compositor does today.
            let mut separate = Duration::MAX;
            for _ in 0..3 {
                let started = Instant::now();
                for _ in 0..N {
                    gpu.run_commands(pool, crate::stats::SubmitSite::Blur, one)
                        .expect("submit");
                }
                separate = separate.min(started.elapsed());
            }

            println!(
                "{:<12} {:>10.2} {:>10.3}ms {:>10.3}ms {:>10.3}ms",
                format!("{w}x{h}"),
                (w as f64) * (h as f64) / 1e6,
                ms(batched),
                ms(separate),
                ms(separate.saturating_sub(batched)) / f64::from(N),
            );

            chain.destroy(&gpu);
            dst.destroy(&gpu);
            source.destroy(&gpu);
        }

        unsafe { gpu.device.destroy_command_pool(pool, None) };
    }

    fn time_submit(
        gpu: &Gpu,
        pool: vk::CommandPool,
        record: impl Fn(vk::CommandBuffer),
    ) -> Duration {
        // One warm-up submit: the first use of a pipeline/image pays host-side
        // costs that say nothing about the steady-state frame.
        gpu.run_commands(pool, crate::stats::SubmitSite::Blur, &record)
            .expect("warm-up");
        let mut best = Duration::MAX;
        for _ in 0..3 {
            let started = Instant::now();
            gpu.run_commands(pool, crate::stats::SubmitSite::Blur, &record)
                .expect("submit");
            best = best.min(started.elapsed());
        }
        best
    }

    /// A high-entropy source, because a flat one is not a fair test: this GPU
    /// compresses render targets losslessly, so a uniform image moves a fraction
    /// of the memory a real desktop scene does and the blur comes out looking
    /// several times cheaper than it is.
    fn noise(w: u32, h: u32) -> Vec<u8> {
        let mut out = vec![0u8; (w as usize) * (h as usize) * 4];
        let mut state = 0x2545_f491_4f6c_dd1du64;
        for px in out.chunks_exact_mut(4) {
            // xorshift64* — cheap, deterministic, and white enough that nothing
            // downstream can compress it.
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            let v = state.wrapping_mul(0x2545_f491_4f6c_dd1d).to_le_bytes();
            px.copy_from_slice(&[v[0], v[1], v[2], 0xff]);
        }
        out
    }

    fn per_rep(total: Duration, empty: Duration) -> Duration {
        total.saturating_sub(empty) / reps()
    }

    fn ms(d: Duration) -> f64 {
        d.as_secs_f64() * 1000.
    }
}
