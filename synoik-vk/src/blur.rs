// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

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

/// Push constants for one direction of the gaussian (matches `blur_gaussian.frag`'s `Push`).
#[repr(C)]
#[derive(Clone, Copy)]
struct GaussianPush {
    direction: [f32; 2],
    /// One texel of the *sampled* texture in UV space.
    pixel_step: f32,
    sigma: f32,
    brightness: f32,
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
    /// GNOME's separable gaussian, and the plain resample its rungs use. `None` when the chain was
    /// built without it — the dual-Kawase path needs neither, and the scratch levels the gaussian
    /// ping-pongs through are not worth allocating for a chain that will never run it.
    gaussian: Option<Gaussian>,
    vert: vk::ShaderModule,
    down_frag: vk::ShaderModule,
    up_frag: vk::ShaderModule,
    /// Level 0 is full-size (the output); levels 1..=passes shrink.
    levels: Vec<Level>,
    /// Descriptor set that samples the external source texture.
    source_set: vk::DescriptorSet,
    passes: usize,
    /// Where the final upsample writes, when the caller gave the chain somewhere of its own.
    ///
    /// Without it the chain's result lands in `levels[0]` and the caller copies it out with
    /// [`Self::copy_output_to`] — a full-size `vkCmdCopyImage` per blurred surface per frame,
    /// invisible in the frame log's coverage figure because a copy is not a draw. With it the last
    /// pass simply renders where the consumer is going to sample, and the copy does not happen.
    ///
    /// The render pass makes this safe rather than clever: `loadOp DONT_CARE` (the pass overwrites
    /// every pixel), `initialLayout UNDEFINED` (no contents to preserve) and
    /// `finalLayout SHADER_READ_ONLY_OPTIMAL` — which is exactly the state `copy_output_to` left
    /// its destination in, so nothing downstream can tell the difference except by timing.
    external_dst: Option<Level>,
}

/// The gaussian path's own pipelines, shaders and ping-pong targets.
///
/// A separable blur has to write H somewhere before reading it for V, and that somewhere must be
/// the *same size* as the level being blurred — which no other level in a halving pyramid is.
/// Hence a scratch twin for each shrinking level.
struct Gaussian {
    scale: vk::Pipeline,
    blur: vk::Pipeline,
    scale_frag: vk::ShaderModule,
    blur_frag: vk::ShaderModule,
    /// Parallel to `levels[1..]`: `scratch[i - 1]` is the same size as `levels[i]`.
    scratch: Vec<Level>,
}

const FULLSCREEN_VERT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/fullscreen.vert.spv"));
const DOWN_FRAG: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/blur_down.frag.spv"));
const UP_FRAG: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/blur_up.frag.spv"));
const SCALE_FRAG: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/blur_scale.frag.spv"));
const GAUSSIAN_FRAG: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/blur_gaussian.frag.spv"));

/// `MAX_RADIUS` (`shell-blur-effect.c:54`) — the radius above which the source is halved again.
const MAX_RADIUS: f64 = 12.;
/// `MIN_DOWNSCALE_SIZE` (`:53`) — halving stops before either side gets this small.
const MIN_DOWNSCALE_SIZE: f64 = 256.;

/// GNOME's downscale cascade: keep halving while the radius is still large *and* the texture is
/// still big enough to spend (`calculate_downscale_factor`, `shell-blur-effect.c:289-314`, "the
/// algorithm used by Firefox").
///
/// The point is that a wide blur does not need a big buffer: at radius 90 on 1920x1080 this lands
/// on 240x135, where the remaining sigma is ~5.6 and the shader takes 19 taps per direction instead
/// of 271. Blurring at full size would be correct and unaffordable.
///
/// Returns `log2` of the factor, i.e. how many pyramid rungs down to go.
pub fn downscale_levels(width: u32, height: u32, radius: f64) -> usize {
    let (mut w, mut h, mut r) = (f64::from(width), f64::from(height), radius);
    let mut levels = 0;
    while r > MAX_RADIUS && w > MIN_DOWNSCALE_SIZE && h > MIN_DOWNSCALE_SIZE {
        levels += 1;
        w /= 2.;
        h /= 2.;
        r /= 2.;
    }
    levels
}

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
    scale: vk::Pipeline,
    gauss: vk::Pipeline,
    scale_frag: vk::ShaderModule,
    gauss_frag: vk::ShaderModule,
    levels: Vec<Level>,
    scratch: Vec<Level>,
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
            scale: vk::Pipeline::null(),
            gauss: vk::Pipeline::null(),
            scale_frag: vk::ShaderModule::null(),
            gauss_frag: vk::ShaderModule::null(),
            levels: Vec::new(),
            scratch: Vec::new(),
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
            for level in self.levels.iter().chain(&self.scratch) {
                d.destroy_framebuffer(level.framebuffer, None);
                d.destroy_image_view(level.view, None);
                d.destroy_image(level.image, None);
                crate::devmem::untrack(level.memory);
                d.free_memory(level.memory, None);
            }
            d.destroy_pipeline(self.down, None);
            d.destroy_pipeline(self.up, None);
            d.destroy_pipeline(self.scale, None);
            d.destroy_pipeline(self.gauss, None);
            d.destroy_pipeline_layout(self.pipeline_layout, None);
            d.destroy_shader_module(self.vert, None);
            d.destroy_shader_module(self.down_frag, None);
            d.destroy_shader_module(self.up_frag, None);
            d.destroy_shader_module(self.scale_frag, None);
            d.destroy_shader_module(self.gauss_frag, None);
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
        Self::build(gpu, source, passes, false)
    }

    /// As [`Self::new`], plus the pipelines and scratch levels [`Self::record_gaussian`] needs.
    pub fn new_with_gaussian(gpu: &Gpu, source: &Texture, passes: usize) -> Result<Self> {
        Self::build(gpu, source, passes, true)
    }

    fn build(gpu: &Gpu, source: &Texture, passes: usize, gaussian: bool) -> Result<Self> {
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

        // One descriptor set per level + one for the external source, doubled (less level 0) when
        // the gaussian's scratch twins are along.
        let count = (passes + 2 + if gaussian { passes } else { 0 }) as u32;
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
            // Both blocks share one layout, so the range is the larger of the two: the Kawase
            // shaders read the first 16 bytes, the gaussian the first 20.
            .size(std::mem::size_of::<BlurPush>().max(std::mem::size_of::<GaussianPush>()) as u32);
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

        if gaussian {
            guard.scale_frag = load_module(device, SCALE_FRAG)?;
            guard.gauss_frag = load_module(device, GAUSSIAN_FRAG)?;
            guard.scale = build_pipeline(
                gpu,
                guard.render_pass,
                guard.pipeline_layout,
                guard.vert,
                guard.scale_frag,
            )?;
            guard.gauss = build_pipeline(
                gpu,
                guard.render_pass,
                guard.pipeline_layout,
                guard.vert,
                guard.gauss_frag,
            )?;
            // A twin for every shrinking level; level 0 is the full-size output and is never
            // ping-ponged through.
            for i in 1..=passes {
                let (w, h) = (guard.levels[i].w, guard.levels[i].h);
                let twin = create_level(
                    gpu,
                    guard.render_pass,
                    guard.sampler,
                    guard.set_layout,
                    guard.desc_pool,
                    w,
                    h,
                )?;
                guard.scratch.push(twin);
            }
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
        let gaussian = gaussian.then(|| Gaussian {
            scale: guard.scale,
            blur: guard.gauss,
            scale_frag: guard.scale_frag,
            blur_frag: guard.gauss_frag,
            scratch: std::mem::take(&mut guard.scratch),
        });
        Ok(BlurChain {
            gaussian,
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
            external_dst: None,
        })
    }

    /// Point the final upsample at `dst` — an image the caller owns, the same size as level 0 and
    /// created with `COLOR_ATTACHMENT` usage — instead of leaving the result in level 0 for the
    /// caller to copy out.
    ///
    /// This is the difference between one full-size `vkCmdCopyImage` per blurred surface per frame
    /// and none. The copy never showed up in the frame log's coverage figure (a copy is not a
    /// draw), so it was pure invisible bandwidth: at a 2238x1258 intermediate it moves ~11 MB in
    /// and ~11 MB out, per effect, every frame.
    ///
    /// Safe because the chain's render pass already describes exactly the contract the copy
    /// provided: `loadOp DONT_CARE` and `initialLayout UNDEFINED` (the pass overwrites every
    /// pixel of the destination, so there is nothing to preserve and no transition to make), and
    /// `finalLayout SHADER_READ_ONLY_OPTIMAL` (where `copy_output_to` left it). The chain owns
    /// only the framebuffer it creates here; the image, its view and its memory stay the
    /// caller's, and [`Self::destroy`] respects that.
    ///
    /// `stop`-ping the upsample early ([`Self::record_to_level`]) ignores this: a caller that
    /// wants a partial result wants it where partial results have always been.
    pub fn set_external_dst(
        &mut self,
        gpu: &Gpu,
        view: vk::ImageView,
        w: u32,
        h: u32,
    ) -> Result<()> {
        let fb_ci = vk::FramebufferCreateInfo::default()
            .render_pass(self.render_pass)
            .attachments(std::slice::from_ref(&view))
            .width(w)
            .height(h)
            .layers(1);
        let framebuffer = unsafe { gpu.device.create_framebuffer(&fb_ci, None) }
            .context("blur external destination fb")?;
        if let Some(old) = self.external_dst.replace(Level {
            image: vk::Image::null(),
            memory: vk::DeviceMemory::null(),
            view,
            framebuffer,
            set: vk::DescriptorSet::null(),
            w,
            h,
        }) {
            unsafe { gpu.device.destroy_framebuffer(old.framebuffer, None) };
        }
        Ok(())
    }

    /// Whether the final upsample writes the caller's own image (see [`Self::set_external_dst`]),
    /// in which case there is nothing to copy out of level 0.
    pub fn has_external_dst(&self) -> bool {
        self.external_dst.is_some()
    }

    /// Record the full down+up chain into `cbuf`. Afterwards level 0 holds the blurred output in
    /// `SHADER_READ_ONLY_OPTIMAL` — or, with an external destination set, that image does.
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
            // The last pass writes the caller's own image when it gave us one, so the result does
            // not have to be copied out of level 0 afterwards. Only when the upsample really is
            // running to completion: a caller that stops early wants the partial result where it
            // has always been.
            let dst = match (&self.external_dst, i) {
                (Some(ext), 1) if stop == 0 => ext,
                _ => &self.levels[i - 1],
            };
            // half_pixel = half a source pixel.
            let half_pixel = [0.5 / src.w as f32, 0.5 / src.h as f32];
            self.pass(gpu, cbuf, self.up, dst, src.set, half_pixel, offset);
        }
    }

    /// Record GNOME's blur: `sigma = radius / 2`, evaluated separably on a downscaled copy
    /// (`ShellBlurEffect`, `shell-blur-effect.c:425-447`; `ClutterBlur`, `clutter-blur.c:339-370`).
    ///
    /// GNOME cascades *two* downscales — its own, then ClutterBlur's — but they collapse: the first
    /// stops once `radius / f <= 12`, which is exactly `sigma <= 6`, which is the second's own
    /// threshold. So one cascade is not a simplification, it is the same arithmetic.
    ///
    /// `brightness` is `ShellBlurEffect`'s multiply, folded into the second direction rather than
    /// given a pass of its own.
    ///
    /// Afterwards level 0 holds the result in `SHADER_READ_ONLY_OPTIMAL`, as with [`Self::record`].
    /// Does nothing without the pipelines — build the chain with [`Self::new_with_gaussian`].
    pub fn record_gaussian(
        &self,
        gpu: &Gpu,
        cbuf: vk::CommandBuffer,
        radius: f64,
        brightness: f32,
    ) {
        let Some(g) = &self.gaussian else {
            return;
        };
        let full = &self.levels[0];

        // How far down the pyramid to work, capped by what this chain has — and floored at one
        // rung, because the horizontal pass has to land in a target the same size as the level it
        // blurs and only the shrinking levels have such a twin. That floor bites only for a radius
        // of 12 or less, where GNOME would blur at full size with sigma <= 6; one halving with
        // sigma <= 3 is the next step of GNOME's own cascade and is visually the same. Giving
        // level 0 a full-size twin to be exact about it would double the chain's largest
        // allocation to serve a radius nothing asks for.
        let want = downscale_levels(full.w, full.h, radius);
        let k = want.clamp(1, self.passes);
        // The radius that survives the descent — and with it the sigma the shader runs.
        let sigma = (radius / f64::from(1u32 << k) / 2.) as f32;

        // Descend: source → L1 → … → Lk, one halving per rung.
        for i in 1..=k {
            let src_set = if i == 1 {
                self.source_set
            } else {
                self.levels[i - 1].set
            };
            self.pass_gaussian(gpu, cbuf, g.scale, &self.levels[i], src_set, None);
        }

        // Blur at the working size, ping-ponging through that level's twin.
        let (work, scratch, work_set) = (&self.levels[k], &g.scratch[k - 1], self.levels[k].set);

        // Horizontal into the twin, vertical back — `pixel_step` is one texel of whatever is being
        // sampled, which is the same size both ways.
        let step = |w: u32| 1.0 / w as f32;
        self.pass_gaussian(
            gpu,
            cbuf,
            g.blur,
            scratch,
            work_set,
            Some(GaussianPush {
                direction: [1., 0.],
                pixel_step: step(work.w),
                sigma,
                brightness: 1.,
            }),
        );
        self.pass_gaussian(
            gpu,
            cbuf,
            g.blur,
            work,
            scratch.set,
            Some(GaussianPush {
                direction: [0., 1.],
                // Sampling `scratch`, which is `work`'s size by construction.
                pixel_step: 1.0 / scratch.h as f32,
                sigma,
                brightness,
            }),
        );

        // And back up to full size in one magnifying draw, which is what GNOME's compositing of the
        // small texture over the actor's box amounts to.
        self.pass_gaussian(gpu, cbuf, g.scale, full, self.levels[k].set, None);
    }

    /// One gaussian-path pass. `push` is `None` for the resample rungs, which read no constants.
    fn pass_gaussian(
        &self,
        gpu: &Gpu,
        cbuf: vk::CommandBuffer,
        pipeline: vk::Pipeline,
        dst: &Level,
        src_set: vk::DescriptorSet,
        push: Option<GaussianPush>,
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
        unsafe {
            device.cmd_begin_render_pass(cbuf, &begin, vk::SubpassContents::INLINE);
            device.cmd_bind_pipeline(cbuf, vk::PipelineBindPoint::GRAPHICS, pipeline);
            let viewport = vk::Viewport {
                x: 0.,
                y: 0.,
                width: dst.w as f32,
                height: dst.h as f32,
                min_depth: 0.,
                max_depth: 1.,
            };
            device.cmd_set_viewport(cbuf, 0, &[viewport]);
            device.cmd_set_scissor(
                cbuf,
                0,
                &[vk::Rect2D {
                    offset: vk::Offset2D { x: 0, y: 0 },
                    extent,
                }],
            );
            device.cmd_bind_descriptor_sets(
                cbuf,
                vk::PipelineBindPoint::GRAPHICS,
                self.pipeline_layout,
                0,
                &[src_set],
                &[],
            );
            if let Some(push) = push {
                device.cmd_push_constants(
                    cbuf,
                    self.pipeline_layout,
                    vk::ShaderStageFlags::FRAGMENT,
                    0,
                    as_bytes(&push),
                );
            }
            device.cmd_draw(cbuf, 3, 1, 0, 0);
            device.cmd_end_render_pass(cbuf);
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
            crate::stats::draw(
                crate::stats::DrawSite::Blur,
                u64::from(extent.width) * u64::from(extent.height),
            );
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
            crate::stats::barriers(1);
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
            crate::stats::barriers(1);
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
            crate::devmem::untrack(mem);
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
            crate::stats::barriers(1);
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
            crate::stats::barriers(1);
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
            // The external destination is a framebuffer over somebody else's image: destroy the
            // framebuffer, never the image, view or memory behind it.
            if let Some(ext) = &self.external_dst {
                d.destroy_framebuffer(ext.framebuffer, None);
            }
            let scratch = self.gaussian.iter().flat_map(|g| g.scratch.iter());
            for level in self.levels.iter().chain(scratch) {
                d.destroy_framebuffer(level.framebuffer, None);
                d.destroy_image_view(level.view, None);
                d.destroy_image(level.image, None);
                crate::devmem::untrack(level.memory);
                d.free_memory(level.memory, None);
            }
            if let Some(g) = &self.gaussian {
                d.destroy_pipeline(g.scale, None);
                d.destroy_pipeline(g.blur, None);
                d.destroy_shader_module(g.scale_frag, None);
                d.destroy_shader_module(g.blur_frag, None);
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
    crate::stats::descriptor_allocs(1);
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
    crate::stats::descriptor_writes(1);
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

    /// Passes the compositor actually configures (`synoik_config::Blur::default`).
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

    /// GNOME's downscale cascade, against the numbers it actually produces.
    ///
    /// Pure arithmetic, so this runs without a device — and it is worth pinning on its own, because
    /// the cascade is what makes a radius-90 blur affordable and getting it wrong is invisible
    /// except as a frame-time regression.
    #[test]
    fn the_downscale_cascade_matches_gnomes() {
        // `BLUR_RADIUS = 90` on a 1080p output: three halvings to 240x135, where the surviving
        // radius is 11.25 — just under the 12 that would buy a fourth.
        assert_eq!(downscale_levels(1920, 1080, 90.), 3);
        // A small radius needs no descent at all.
        assert_eq!(downscale_levels(1920, 1080, 12.), 0);
        assert_eq!(downscale_levels(1920, 1080, 13.), 1);
        // The size guard eventually wins over the radius, however wide the blur — but it is tested
        // *before* each halving, so a side is allowed to end up under the threshold, just not to
        // start under it. 300 -> 150 and stop; 600 -> 300 -> 150 and stop; 256 never moves.
        assert_eq!(downscale_levels(300, 300, 1000.), 1);
        assert_eq!(downscale_levels(600, 600, 1000.), 2);
        assert_eq!(downscale_levels(256, 256, 1000.), 0);
    }

    /// The gaussian spreads an edge, in proportion to its radius, and honours brightness.
    ///
    /// The discriminating assertions are the *comparisons*, not "it changed": a blur that ignored
    /// sigma entirely would still smear the edge and still pass a single-radius check. What cannot
    /// survive a wrong kernel is a wide radius spreading further than a narrow one while both leave
    /// the far corners alone.
    #[test]
    fn the_gaussian_spreads_an_edge_by_its_radius() {
        let Ok(gpu) = Gpu::new() else {
            eprintln!("skipping the_gaussian_spreads_an_edge_by_its_radius: no Vulkan device");
            return;
        };
        let pool_ci = vk::CommandPoolCreateInfo::default().queue_family_index(gpu.queue_family);
        let pool = unsafe { gpu.device.create_command_pool(&pool_ci, None) }.expect("pool");

        const W: u32 = 512;
        const H: u32 = 512;
        // A hard vertical edge: black left of centre, white right of it. Grey, so the reading does
        // not depend on channel order.
        let mut src = vec![0u8; (W * H * 4) as usize];
        for y in 0..H {
            for x in 0..W {
                let v = if x < W / 2 { 0 } else { 255 };
                let i = ((y * W + x) * 4) as usize;
                src[i..i + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }

        // Sample the middle row of the result and report how wide the transition is.
        let spread_of = |radius: f64, brightness: f32| -> (usize, u8, u8) {
            let source =
                Texture::from_rgba(&gpu, pool, W, H, &src, vk::Filter::LINEAR).expect("source");
            let chain = BlurChain::new_with_gaussian(&gpu, &source, PASSES).expect("chain");
            gpu.run_commands(pool, crate::stats::SubmitSite::Blur, |cbuf| {
                chain.record_gaussian(&gpu, cbuf, radius, brightness);
            })
            .expect("submit");
            let out = chain.read_output(&gpu, pool).expect("read");
            let row = H / 2;
            let at = |x: u32| out[((row * W + x) * 4) as usize];
            // How many pixels are neither near-black nor near-white: the edge's reach.
            let transition = (0..W).filter(|&x| at(x) > 8 && at(x) < 200).count();
            let result = (transition, at(0), at(W - 1));
            chain.destroy(&gpu);
            source.destroy(&gpu);
            result
        };

        let (narrow, narrow_left, narrow_right) = spread_of(16., 1.);
        let (wide, wide_left, wide_right) = spread_of(90., 1.);

        assert!(narrow > 0, "a blur that does not blur");
        assert!(
            wide > narrow * 2,
            "a wider radius must reach further: {narrow} vs {wide}"
        );
        // The far edges are untouched — a blur, not a fade to grey.
        assert!(
            narrow_left < 8 && wide_left < 8,
            "the black side stayed black"
        );
        assert!(
            narrow_right > 200 && wide_right > 200,
            "the white side stayed white"
        );

        // Brightness is a plain multiply on the result (`shell-blur-effect.c:47-51`).
        let (_, _, dim_right) = spread_of(90., 0.5);
        let half = f32::from(wide_right) * 0.5;
        assert!(
            (f32::from(dim_right) - half).abs() < 12.,
            "brightness 0.5 should halve {wide_right}, got {dim_right}"
        );

        unsafe { gpu.device.destroy_command_pool(pool, None) };
    }

    /// Where the blur's cost actually goes, so the optimization is chosen from
    /// measurement rather than from the pass count.
    ///
    /// Ignored because it needs the real device and takes seconds; run it with
    /// `cargo test -p synoik-vk blur_cost -- --ignored --nocapture`.
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
            "size", "px", "record+copy", "record→dst", "record→L1"
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
            // The shipping path: the final upsample renders straight into `dst`, so there is no
            // copy to pay for. This is the column `record+copy` is here to be compared against.
            let mut direct = BlurChain::new(&gpu, &source, PASSES).expect("chain (direct)");
            direct
                .set_external_dst(&gpu, dst.view, w, h)
                .expect("external dst");
            let no_copy = per_rep(
                time_submit(&gpu, pool, |cbuf| {
                    for _ in 0..reps() {
                        direct.record(&gpu, cbuf, OFFSET);
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

            direct.destroy(&gpu);
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
    /// Run with `cargo test -p synoik-vk blur_submit_overhead -- --ignored --nocapture`.
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
