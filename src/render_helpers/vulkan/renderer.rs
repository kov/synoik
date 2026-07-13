use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use ash::vk;
use niri_vk::blur::BlurChain;
use niri_vk::gpu::{DeviceSelector, Gpu};
use niri_vk::render::{
    load_module, sampler_set_layout, BorderPush, ClippedTexturePush, PostprocessPush, QuadPush,
    ResizePush, ShadowPush, COLOR_RANGE,
};
use niri_vk::shaders::{
    BORDER_FRAG, BORDER_VERT, CLIPPED_TEX_FRAG, GRADIENT_FADE_FRAG, POSTPROCESS_FRAG,
    POSTPROCESS_VERT, QUAD_VERT, RESIZE_FRAG, RESIZE_VERT, ROUNDED_TEX_FRAG, SHADOW_FRAG,
    SHADOW_VERT, SOLID_FRAG, TEX_FRAG,
};
use niri_vk::texture::Texture as NiriTexture;
use smithay::backend::allocator::dmabuf::{Dmabuf, WeakDmabuf};
use smithay::backend::allocator::format::FormatSet;
use smithay::backend::allocator::{Buffer as _, Format, Fourcc, Modifier};
use smithay::backend::renderer::sync::SyncPoint;
use smithay::backend::renderer::{
    Bind, ContextId, DebugFlags, ExportMem, ImportMem, Offscreen, Renderer, RendererSuper, Texture,
    TextureFilter,
};
use smithay::utils::{Buffer as BufferCoord, Physical, Rectangle, Size, Transform};

use super::custom::{compile_custom, CustomShaderType};
use super::error::VulkanError;
use super::frame::VulkanFrame;
use super::types::{
    import_format, is_rgba8888, VkFramebuffer, VkMapping, VkTexture, IMAGE_VK_FORMAT,
};
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
    /// A render pass **compatible** with [`Self::render_pass`] (same attachment format/samples and
    /// single subpass, so the same pipelines bind) but with `load_op = LOAD` and
    /// `initial_layout = TRANSFER_SRC_OPTIMAL`: the pass a [`VulkanFrame`] restarts on after a
    /// mid-frame capture (`VulkanFrame::capture_region`). Preserves what was drawn before the
    /// capture instead of discarding it, so framebuffer effects (backdrop blur) can grab the
    /// scene-so-far and keep compositing on top of it.
    pub(super) continuation_render_pass: vk::RenderPass,
    pub(super) solid_pipeline: Pipeline,
    pub(super) texture_pipeline: Pipeline,
    pub(super) rounded_texture_pipeline: Pipeline,
    pub(super) clipped_texture_pipeline: Pipeline,
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
    /// Reused staging buffer for shm-client texture uploads (grown on demand), so the shm cache's
    /// hit path refreshes a client buffer every commit without churning a mappable allocation.
    shm_staging: niri_vk::texture::Staging,
    /// The host-visible buffer every readback copies into, grown on demand and reused.
    ///
    /// This used to be a fresh `VkBuffer` + `HOST_VISIBLE` allocation per call. On Venus
    /// host-visible memory is a virtio-gpu blob, and shm screencopy reads back **every frame** —
    /// exactly the per-frame mappable-blob churn that exhausts the host pool and aborts the
    /// session. Grow-only, so a steady stream of same-size reads allocates once; a larger read
    /// grows it and smaller ones then reuse the larger buffer.
    readback_staging_buffer: niri_vk::texture::Staging,
    /// Count of readback host-buffer (re)allocations (test-only): the no-churn invariant is that
    /// this stops growing once the largest read size has been seen. See
    /// `vulkan_repeated_readbacks_reuse_the_host_buffer`.
    #[cfg(test)]
    readback_buffer_allocs: usize,
    downscale_filter: TextureFilter,
    upscale_filter: TextureFilter,
    debug_flags: DebugFlags,
    /// Reused R8G8B8A8 shadows for the present-blit path, keyed by target size and kept across
    /// frames so `bind` does not allocate a full-screen device image every frame (which exhausts
    /// host memory on Venus under sustained rendering).
    ///
    /// Keyed by size rather than held in a single slot because **several differently-sized
    /// `Argb8888`/`Xrgb8888` targets are bound within one frame**: the scanout buffer of each
    /// output, a screencast buffer (window casts are sized to the window's bbox, and a rotated
    /// output's cast is transform-sized), and any screencopy region. A single slot would evict and
    /// reallocate on every bind as those alternate — the exact per-frame churn that aborts Venus.
    ///
    /// Bounded by [`MAX_PRESENT_BLIT_SHADOWS`], evicting least-recently-used, so a stream of new
    /// sizes (an interactively resized window cast renegotiates its size as it grows) cannot grow
    /// the cache without bound; the sizes actually bound each frame stay resident and never
    /// reallocate. Reuse and eviction are safe because rendering is synchronous (`finish`
    /// CPU-waits, so no shadow is in flight when the next bind reuses or drops it).
    present_blit_shadows: HashMap<(u32, u32), ShadowEntry>,
    /// Monotonic tick stamped onto a [`ShadowEntry`] each time it is used, so eviction can pick
    /// the least-recently-used shadow. Bumped once per `present_blit_shadow_for` call.
    shadow_clock: u64,
    /// Count of present-blit shadow images actually allocated (test-only observability): the
    /// invariant a steady-state render loop must hold is that this stops growing once each bound
    /// size has been seen. Cache *hits* are invisible to a pixel assertion, so this counter is
    /// what pins the no-churn invariant — see
    /// `vulkan_alternating_present_blit_sizes_reuse_shadows`.
    #[cfg(test)]
    present_blit_shadow_allocs: usize,
    /// Staging images for a **converting** readback: when a caller wants bytes in an order the
    /// source image doesn't hold (an `Xrgb8888` shm pool read off an `R8G8B8A8` offscreen), we
    /// blit through one of these — `vkCmdBlitImage` reorders the channels on the GPU, so no
    /// CPU swizzle.
    ///
    /// Cached for the same reason the present-blit shadows are: shm screencopy can fire every
    /// frame, and allocating a `VkImage` per frame exhausts the Venus blob pool and aborts the
    /// session. Keyed by size **and** format; LRU-evicted under [`MAX_READBACK_STAGING`]. Reuse
    /// and eviction are safe because rendering is synchronous.
    readback_staging: HashMap<(u32, u32, i32), StagingEntry>,
    /// Monotonic tick for [`Self::readback_staging`]'s LRU, bumped once per lookup.
    staging_clock: u64,
    /// Count of readback staging images actually allocated (test-only): pins the no-churn
    /// invariant, exactly as `present_blit_shadow_allocs` does — a cache *hit* is invisible to a
    /// pixel assertion. See `vulkan_repeated_converting_readbacks_reuse_staging`.
    #[cfg(test)]
    readback_staging_allocs: usize,
    /// Imported scanout dmabuf targets, keyed by buffer identity. `DrmCompositor` cycles a small
    /// fixed set of GBM buffers and re-binds one every frame; **importing** a dmabuf on Venus
    /// creates a host-side resource, so re-importing per frame churns those resources until the
    /// guest↔host ring is marked `FATAL` and `vn_ring_submit` aborts (no guest OOM — the guest
    /// image is freed cleanly each frame). Cache each imported target and reuse it across frames;
    /// entries whose buffer was freed are evicted. Reuse is safe for the same reason as the
    /// shadow: `finish` CPU-waits, so no import is in flight when the next frame reuses it.
    /// Mirrors `GlesRenderer`'s `buffers` bound-dmabuf cache.
    dmabuf_target_cache: HashMap<WeakDmabuf, VkTexture>,
    /// Imported **client** sampled dmabufs, keyed by buffer identity. smithay clears its
    /// per-surface texture on *every* new-buffer commit (even a recycled `wl_buffer`), so an
    /// animating dmabuf client (e.g. a WebGL page) would otherwise re-run the full
    /// `import_dmabuf_sampled` — a fresh `vkAllocateMemory` import +
    /// image/view/sampler/descriptor set + a fenced acquire barrier — every frame. On Venus
    /// that per-frame host-resource churn is exactly what pressures the guest↔host ring into
    /// the `FATAL`/alive-expiry abort (see `DEVMAC-SESSION-confusion.md`). Clients recycle a
    /// small buffer pool, so cache the imported texture per buffer and reuse it; a hit only needs
    /// the acquire barrier (`VkTexture::record_reacquire_dmabuf`) to pick up the freshly produced
    /// content — no allocation. Entries whose buffer was freed are evicted lazily on lookup. This
    /// is the sampled-import dual of [`Self::dmabuf_target_cache`] (which needs no re-acquire
    /// because the compositor, not an external producer, writes the scanout target).
    /// Reuse/eviction is safe for the same reason as the other caches: rendering is
    /// synchronous (`finish` CPU-waits), so no cached image is in flight when the next frame
    /// reuses or drops it; pipelined present would need per-texture fence tracking + deferred
    /// destruction here.
    dmabuf_import_cache: HashMap<WeakDmabuf, VkTexture>,

    /// Cache-hit client dmabuf-imports awaiting their re-acquire barrier (Part 2: fold the barrier
    /// into the frame submit instead of a per-commit standalone submit + fence-wait). Populated on
    /// a hit in [`Self::import_dmabuf_as_texture`]; drained by [`super::VulkanFrame::begin`],
    /// which records each barrier into the frame's command buffer BEFORE its render pass so
    /// the acquire rides the frame's single submit — the wait is the existing `finish()` park,
    /// so no extra submit/park per animating surface (the Venus ring pressure Part 1 started
    /// to unwind).
    ///
    /// Invariant: a [`super::VulkanFrame`] mutably borrows the renderer, so no import (hence no
    /// push) can happen while a frame exists — every push therefore precedes some `begin()`, and
    /// every `begin()` drains. Any future non-frame consumer of client textures MUST drain first.
    /// Entries hold a `VkTexture` clone, pinning the image (and its dmabuf import) until drained;
    /// if damage tracking skips a frame after elements were built, a pending entry is simply
    /// drained (harmless extra acquire) by the next real `begin()`. De-duped by image so a surface
    /// imported twice before a frame records the barrier once.
    pending_dmabuf_acquires: Vec<VkTexture>,
}

/// How many differently-sized present-blit shadows to keep. Comfortably covers what a live session
/// binds within one frame (a scanout buffer per output, a screencast buffer, a screencopy region)
/// while bounding the memory a churn of new sizes can pin — each shadow is a full target-sized
/// device image. See [`VulkanRenderer::present_blit_shadows`].
const MAX_PRESENT_BLIT_SHADOWS: usize = 8;

/// Cap on cached readback staging images. Only a couple of sizes are ever live (the output size for
/// screencopy, and the small cursor bitmap), so this is generous. See
/// [`VulkanRenderer::readback_staging`].
const MAX_READBACK_STAGING: usize = 4;

/// A cached present-blit shadow plus the tick it was last used on (for LRU eviction).
#[derive(Debug)]
struct ShadowEntry {
    texture: VkTexture,
    last_used: u64,
}

/// A cached readback staging image. See [`VulkanRenderer::readback_staging`].
struct StagingEntry {
    texture: VkTexture,
    last_used: u64,
}

impl VulkanRenderer {
    /// Bring up a fresh device (Venus/lavapipe depending on `VK_DRIVER_FILES`) and build the
    /// renderer, picking the best device by type rank. Returns an error (rather than panicking) if
    /// no usable Vulkan device is present.
    ///
    /// For headless and test use. A real session must go through [`Self::for_drm_render_node`], so
    /// that we render on the device we advertise to clients.
    pub fn new() -> Result<Self, VulkanError> {
        let gpu = Arc::new(Gpu::new()?);
        Self::with_gpu(gpu)
    }

    /// Bring up the renderer on the device backing this DRM **render** node — the one we advertise
    /// to clients in dmabuf feedback, and therefore the one their buffers will be allocated for.
    pub fn for_drm_render_node(major: u32, minor: u32) -> Result<Self, VulkanError> {
        let gpu = Arc::new(Gpu::with_selector(DeviceSelector::DrmRenderNode {
            major,
            minor,
        })?);
        Self::with_gpu(gpu)
    }

    fn with_gpu(gpu: Arc<Gpu>) -> Result<Self, VulkanError> {
        let render_pass = create_render_pass(&gpu.device)?;
        let continuation_render_pass = create_continuation_render_pass(&gpu.device)?;
        let sampler_set_layout = sampler_set_layout(&gpu)?;
        // Our largest built-in push block (PostprocessPush, 208 B) exceeds the 128 B spec minimum;
        // fail loudly on an ICD that can't hold it rather than eat a pipeline-layout VUID later.
        let max_push = unsafe { gpu.instance.get_physical_device_properties(gpu.phys) }
            .limits
            .max_push_constants_size;
        if (std::mem::size_of::<PostprocessPush>() as u32) > max_push {
            return Err(VulkanError::Unsupported(
                "device push-constant budget too small for the built-in materials",
            ));
        }
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
        // Clipped-surface: samples a texture (set 0) and clips it to a rounded geometry. Like the
        // texture material it blends straight-alpha (attenuates only the alpha for the AA edge).
        let clipped_texture_pipeline = build_pipeline(
            &gpu,
            render_pass,
            QUAD_VERT,
            CLIPPED_TEX_FRAG,
            sampler,
            std::mem::size_of::<ClippedTexturePush>() as u32,
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
            continuation_render_pass,
            solid_pipeline,
            texture_pipeline,
            rounded_texture_pipeline,
            clipped_texture_pipeline,
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
            shm_staging: niri_vk::texture::Staging::new(),
            readback_staging_buffer: niri_vk::texture::Staging::new_readback(),
            #[cfg(test)]
            readback_buffer_allocs: 0,
            downscale_filter: TextureFilter::Linear,
            upscale_filter: TextureFilter::Linear,
            debug_flags: DebugFlags::empty(),
            present_blit_shadows: HashMap::new(),
            shadow_clock: 0,
            readback_staging: HashMap::new(),
            staging_clock: 0,
            #[cfg(test)]
            present_blit_shadow_allocs: 0,
            #[cfg(test)]
            readback_staging_allocs: 0,
            dmabuf_target_cache: HashMap::new(),
            dmabuf_import_cache: HashMap::new(),
            pending_dmabuf_acquires: Vec::new(),
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
    /// Fed live from the config by [`Self::set_custom_resize_shader`] and friends (the
    /// owned-renderer side of the `shaders::set_custom_*_program` calls at the GLES
    /// install/reload sites).
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

    /// Whether the user installed a custom shader in `ty`'s slot. The wired animation elements
    /// branch on this to draw via `render_custom_*` (the user override) instead of the built-in
    /// effect.
    pub(crate) fn has_custom_shader(&self, ty: CustomShaderType) -> bool {
        self.custom_pipeline(ty).is_some()
    }

    /// Install (or clear, with `None`) the user's custom **resize** animation shader — the owned
    /// renderer's dual of [`crate::render_helpers::shaders::set_custom_resize_program`], called
    /// from the same config install/reload sites. A compile error is logged and leaves the
    /// previous shader in place (graceful degrade), matching the GLES path.
    pub(crate) fn set_custom_resize_shader(&mut self, src: Option<&str>) {
        self.install_custom_shader(CustomShaderType::Resize, src);
    }

    /// Install (or clear) the user's custom **close** animation shader. See
    /// [`Self::set_custom_resize_shader`].
    pub(crate) fn set_custom_close_shader(&mut self, src: Option<&str>) {
        self.install_custom_shader(CustomShaderType::Close, src);
    }

    /// Install (or clear) the user's custom **open** animation shader. See
    /// [`Self::set_custom_resize_shader`].
    pub(crate) fn set_custom_open_shader(&mut self, src: Option<&str>) {
        self.install_custom_shader(CustomShaderType::Open, src);
    }

    fn install_custom_shader(&mut self, ty: CustomShaderType, src: Option<&str>) {
        if let Err(err) = self.set_custom_shader(ty, src) {
            warn!("error installing custom {ty:?} shader on the Vulkan renderer: {err}");
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

    /// Import a single-plane client dmabuf as a sampled [`VkTexture`] (the [`ImportDma`] path). The
    /// buffer's DRM format must be one of the 8888 byte orders [`import_format`] handles, with the
    /// LINEAR modifier (all Venus exposes) — clients are advertised exactly [`dmabuf_formats`], so
    /// a mismatch is a misbehaving client, not the common path. The image is acquired from the
    /// FOREIGN queue family and left in `SHADER_READ_ONLY_OPTIMAL`, wrapped with a descriptor
    /// set so a frame can sample it like any other texture.
    pub(super) fn import_dmabuf_as_texture(
        &mut self,
        dmabuf: &Dmabuf,
    ) -> Result<VkTexture, VulkanError> {
        // PRODUCER SYNC — this is an ownership *acquire* barrier (FOREIGN queue family →
        // ours), NOT a readiness wait on the client's producing GPU fence. That's fine:
        // producer readiness is guaranteed UPSTREAM, at commit time, renderer-agnostically.
        // `State::add_default_dmabuf_pre_commit_hook` (and the mapped-toplevel hook in
        // `handlers/xdg_shell.rs`) hold the surface commit until either the client's
        // `linux-drm-syncobj-v1` acquire timeline point signals, or — for implicit-sync
        // clients — smithay's `Dmabuf::generate_blocker(Interest::READ)` observes the buffer's
        // producing (write/exclusive) fence via `poll(2)` on the plane fds. So by the time this
        // import runs the buffer is producer-complete; there is nothing to wait on here. This
        // matches mutter and smithay's anvil (commit-time gating, not render-time waits). The
        // Venus implicit fence is trustworthy on this VM (the `sync_spike` proved a virtio_gpu
        // dma_fence reflects host-GPU completion; GLES daily-drives tear-free on the same
        // blocker). See `docs/fork/explicit-sync.md`.
        //
        // This becomes a real render-side gap ONLY if the fork later (a) advertises explicit
        // sync while *skipping* the commit-time acquire blocker (non-standard; neither mutter
        // nor anvil does this), or (b) drops the synchronous `finish()` CPU-wait to pipeline
        // present — at which point acquire would ride a wait-semaphore and release an exported
        // `VkFence`→`SYNC_FD` (the bridge the `sync_spike` de-risked). Neither is true today.
        if dmabuf.num_planes() != 1 {
            return Err(VulkanError::Unsupported("multi-planar dmabuf import"));
        }
        let format = dmabuf.format();
        if format.modifier != Modifier::Linear {
            return Err(VulkanError::Other(format!(
                "dmabuf import: only the LINEAR modifier is supported, got {:?}",
                format.modifier
            )));
        }
        let Some((vk_format, alpha_one)) = import_format(format.code) else {
            return Err(VulkanError::UnsupportedFormat(format.code));
        };
        let (w, h) = (dmabuf.width(), dmabuf.height());
        // Single plane (checked above).
        let fd = dmabuf.handles().next().expect("one plane");
        let offset = dmabuf.offsets().next().expect("one plane");
        let stride = dmabuf.strides().next().expect("one plane");
        let filter = match self.upscale_filter {
            TextureFilter::Linear => vk::Filter::LINEAR,
            TextureFilter::Nearest => vk::Filter::NEAREST,
        };
        // Cache hit: the client recommitted a buffer we already imported (recycled from its pool).
        // Reuse the imported image/view/sampler/descriptor set — the image's memory IS the shared
        // dmabuf, so the new content is already there. The image still needs a re-acquire barrier
        // (re-take ownership from FOREIGN + invalidate the sampler caches) before it is sampled
        // again, but we do NOT run it here on its own submit: queue the texture so the next
        // `VulkanFrame::begin` records the barrier into the frame's command buffer, riding the
        // frame submit (see `pending_dmabuf_acquires`). This is ownership/visibility only —
        // producer readiness is guaranteed upstream by the commit-time acquire blocker
        // (above).
        if let Some(tex) = self.cached_dmabuf_import(dmabuf) {
            // A stable buffer identity implies immutable geometry/format, so a cached entry can
            // only mismatch the live dmabuf if smithay ever reused a `WeakDmabuf` key
            // across buffers.
            debug_assert!(
                tex.size() == Size::from((w as i32, h as i32)) && tex.format() == Some(format.code),
                "cached dmabuf import metadata must match its stable buffer identity",
            );
            // De-dupe: a surface imported twice before a frame drains needs the barrier recorded
            // only once (a redundant self-acquire-while-owned is a tolerated no-op but wastes a
            // Venus ring op — the very thing this path exists to reduce).
            if !self
                .pending_dmabuf_acquires
                .iter()
                .any(|t| t.image() == tex.image())
            {
                self.pending_dmabuf_acquires.push(tex.clone());
            }
            return Ok(tex);
        }
        let tex = NiriTexture::import_dmabuf_sampled(
            &self.gpu,
            self.command_pool,
            w,
            h,
            fd,
            offset,
            stride,
            format.modifier.into(),
            vk_format,
            alpha_one,
            filter,
        )?;
        let (desc_pool, set) = self.make_texture_set(&tex)?;
        let tex = VkTexture::new(
            self.gpu.clone(),
            tex,
            desc_pool,
            set,
            w,
            h,
            format.code,
            false,
        );
        self.dmabuf_import_cache.insert(dmabuf.weak(), tex.clone());
        Ok(tex)
    }

    /// Copy a `w×h` region of `tex`'s image into a tight host `Vec<u8>`, 4 bytes per pixel. Used by
    /// [`ExportMem::copy_framebuffer`]. Transitions the image to `TRANSFER_SRC_OPTIMAL` first if
    /// the tracked layout says it is elsewhere (e.g. `SHADER_READ_ONLY_OPTIMAL` after it was
    /// sampled).
    ///
    /// The bytes come back in `tex`'s own channel order — unless `via` is given, in which case the
    /// region is first blitted into that staging image and copied out of *it*. Blitting between
    /// images of different formats performs a format conversion (Vulkan spec, "Image Copies with
    /// Scaling"), so an `R8G8B8A8` source through a `B8G8R8A8` staging image yields BGRA bytes with
    /// no CPU pass over the pixels. Same-size, `NEAREST`: an exact copy, and it needs no linear
    /// filter support.
    fn download_region(
        &mut self,
        tex: &VkTexture,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        via: Option<&VkTexture>,
    ) -> Result<Vec<u8>, VulkanError> {
        let size = (w as vk::DeviceSize) * (h as vk::DeviceSize) * 4;
        let image = tex.image();
        let old_layout = tex.layout();

        // Reuse the host-visible readback buffer rather than allocating a mappable blob per call.
        #[cfg(test)]
        let grew = size > self.readback_staging_buffer.capacity();
        self.readback_staging_buffer.ensure(&self.gpu, size)?;
        #[cfg(test)]
        if grew {
            self.readback_buffer_allocs += 1;
        }
        let buffer = self.readback_staging_buffer.buffer();

        let dev = &self.gpu.device;

        // Without a staging image we copy straight out of `tex` at the region's origin; with one we
        // blit the region into it first and then copy the whole staging image from its origin.
        let (copy_from, copy_x, copy_y) = match via {
            None => (image, x, y),
            Some(staging) => (staging.image(), 0, 0),
        };

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

            if let Some(staging) = via {
                transition_image(
                    dev,
                    cbuf,
                    staging.image(),
                    staging.layout(),
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::AccessFlags::TRANSFER_WRITE,
                    vk::PipelineStageFlags::TRANSFER,
                );

                let layers = vk::ImageSubresourceLayers {
                    aspect_mask: vk::ImageAspectFlags::COLOR,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                };
                let blit = vk::ImageBlit::default()
                    .src_subresource(layers)
                    .src_offsets([
                        vk::Offset3D { x, y, z: 0 },
                        vk::Offset3D {
                            x: x + w as i32,
                            y: y + h as i32,
                            z: 1,
                        },
                    ])
                    .dst_subresource(layers)
                    .dst_offsets([
                        vk::Offset3D { x: 0, y: 0, z: 0 },
                        vk::Offset3D {
                            x: w as i32,
                            y: h as i32,
                            z: 1,
                        },
                    ]);
                dev.cmd_blit_image(
                    cbuf,
                    image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    staging.image(),
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[blit],
                    vk::Filter::NEAREST,
                );

                transition_image(
                    dev,
                    cbuf,
                    staging.image(),
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
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
                .image_offset(vk::Offset3D {
                    x: copy_x,
                    y: copy_y,
                    z: 0,
                })
                .image_extent(vk::Extent3D {
                    width: w,
                    height: h,
                    depth: 1,
                });
            dev.cmd_copy_image_to_buffer(
                cbuf,
                copy_from,
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
        if let Some(staging) = via {
            staging.set_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL);
        }

        Ok(self
            .readback_staging_buffer
            .read(&self.gpu, size as usize)?)
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
            self.shm_staging.destroy(dev);
            self.readback_staging_buffer.destroy(dev);
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
            dev.destroy_render_pass(self.continuation_render_pass, None);
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
        Ok(VkFramebuffer::new(target.clone()))
    }
}

impl Bind<Dmabuf> for VulkanRenderer {
    /// Bind a (GBM-allocated) dmabuf as a render target — the KMS-scanout path (Stage 3). Imports
    /// the dmabuf's memory as a `VkImage`; a frame then renders into it (directly for RGBA-order
    /// buffers, or via a shadow + present-blit for `Argb8888`/`Xrgb8888` planes) so a display
    /// controller can scan it out. A fresh import per bind (no cache yet — a follow-up); the
    /// returned framebuffer owns the import(s) and frees them on drop.
    fn bind<'a>(&mut self, target: &'a mut Dmabuf) -> Result<VkFramebuffer<'a>, VulkanError> {
        self.import_dmabuf_target(target)
    }
}

impl VulkanRenderer {
    /// Import a single-plane dmabuf as a scanout [`VkFramebuffer`]. Two shapes:
    ///
    /// - `Abgr8888`/`Xbgr8888` ([`is_rgba8888`]) match the owned renderer's `R8G8B8A8`-order render
    ///   pass, so a frame renders **straight into** the dmabuf.
    /// - `Argb8888`/`Xrgb8888` (the common KMS primary-plane byte order — `B8G8R8A8`) do not, so we
    ///   render into an R8G8B8A8 shadow (reusing the render pass + all pipelines) and blit it into
    ///   the dmabuf on `finish`, the blit reordering RGBA→BGRA. This avoids a whole second
    ///   `B8G8R8A8` render pass + pipeline set at the cost of one full-frame blit.
    fn import_dmabuf_target<'a>(
        &mut self,
        dmabuf: &Dmabuf,
    ) -> Result<VkFramebuffer<'a>, VulkanError> {
        let fourcc = dmabuf.format().code;
        if dmabuf.num_planes() != 1 {
            return Err(VulkanError::Other(format!(
                "dmabuf scanout target must be single-plane, got {}",
                dmabuf.num_planes()
            )));
        }
        let (w, h) = (dmabuf.width(), dmabuf.height());
        let modifier: u64 = dmabuf.format().modifier.into();
        let fd = dmabuf
            .handles()
            .next()
            .ok_or_else(|| VulkanError::Other("dmabuf has no plane fd".into()))?;
        let offset = dmabuf.offsets().next().unwrap_or(0);
        let stride = dmabuf.strides().next().unwrap_or(0);
        let filter = match self.upscale_filter {
            TextureFilter::Linear => vk::Filter::LINEAR,
            TextureFilter::Nearest => vk::Filter::NEAREST,
        };

        if is_rgba8888(fourcc) {
            // Direct: the dmabuf's byte order matches the render pass, render into it in place.
            // Reuse the cached import for this buffer if we already made one (re-importing every
            // frame aborts Venus — see `dmabuf_target_cache`).
            let buffer = match self.cached_dmabuf_target(dmabuf) {
                Some(buffer) => buffer,
                None => {
                    let tex = NiriTexture::import_dmabuf_render_target(
                        &self.gpu,
                        w,
                        h,
                        fd,
                        offset,
                        stride,
                        modifier,
                        IMAGE_VK_FORMAT,
                        vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::TRANSFER_SRC,
                        filter,
                    )?;
                    let framebuffer = self.dmabuf_framebuffer(&tex, w, h)?;
                    let buffer = VkTexture::new_dmabuf_target(
                        self.gpu.clone(),
                        tex,
                        framebuffer,
                        w,
                        h,
                        fourcc,
                    );
                    self.dmabuf_target_cache
                        .insert(dmabuf.weak(), buffer.clone());
                    buffer
                }
            };
            return Ok(VkFramebuffer::new(buffer));
        }

        // Present-blit: the plane's byte order differs from the render pass. `import_format` maps
        // `Argb8888`/`Xrgb8888` → `B8G8R8A8_UNORM`; anything else is unsupported.
        let Some((present_format, _opaque)) = import_format(fourcc) else {
            return Err(VulkanError::UnsupportedFormat(fourcc));
        };

        // Present: the imported dmabuf as a blit destination (`TRANSFER_DST`), reported with the
        // real scanout `fourcc`. Cached across frames per buffer — see `dmabuf_target_cache`.
        let present = match self.cached_dmabuf_target(dmabuf) {
            Some(present) => present,
            None => {
                let present_tex = NiriTexture::import_dmabuf_render_target(
                    &self.gpu,
                    w,
                    h,
                    fd,
                    offset,
                    stride,
                    modifier,
                    present_format,
                    // TRANSFER_DST for the blit; TRANSFER_SRC so the scanout buffer can be read
                    // back (ExportMem / the scanout test).
                    vk::ImageUsageFlags::COLOR_ATTACHMENT
                        | vk::ImageUsageFlags::TRANSFER_DST
                        | vk::ImageUsageFlags::TRANSFER_SRC,
                    filter,
                )?;
                let present =
                    VkTexture::new_present_target(self.gpu.clone(), present_tex, w, h, fourcc);
                self.dmabuf_target_cache
                    .insert(dmabuf.weak(), present.clone());
                present
            }
        };

        // Shadow: an R8G8B8A8 render target (reuses the render pass + every pipeline). Its `format`
        // is the RGBA byte order the render pass actually writes. Cached across frames — see
        // `present_blit_shadow_for`.
        let shadow = self.present_blit_shadow_for(w, h, filter)?;

        Ok(VkFramebuffer::new_with_present(shadow, present))
    }

    /// The cached imported target for `dmabuf` (present-blit `present`, or a direct render target),
    /// if one is live; also evicts entries whose buffer was freed. `None` means the caller must
    /// import and then `dmabuf_target_cache.insert` it. See [`Self::dmabuf_target_cache`].
    fn cached_dmabuf_target(&mut self, dmabuf: &Dmabuf) -> Option<VkTexture> {
        self.dmabuf_target_cache.retain(|weak, _| !weak.is_gone());
        self.dmabuf_target_cache.get(&dmabuf.weak()).cloned()
    }

    /// The cached sampled-import texture for a client `dmabuf`, if live; also evicts entries whose
    /// buffer was freed. `None` means the caller must import and `dmabuf_import_cache.insert` it.
    /// See [`Self::dmabuf_import_cache`].
    fn cached_dmabuf_import(&mut self, dmabuf: &Dmabuf) -> Option<VkTexture> {
        self.dmabuf_import_cache.retain(|weak, _| !weak.is_gone());
        self.dmabuf_import_cache.get(&dmabuf.weak()).cloned()
    }

    /// Live entry count of the client sampled-dmabuf import cache (test-only observability).
    #[cfg(test)]
    pub(super) fn dmabuf_import_cache_len(&self) -> usize {
        self.dmabuf_import_cache.len()
    }

    /// Record every queued client-dmabuf re-acquire barrier into `cbuf` (a frame's command buffer),
    /// clearing the queue. Called by [`super::VulkanFrame::begin`] before the render pass begins
    /// (barriers must be recorded outside a render pass). Each barrier rides the frame's single
    /// submit, so the acquire is no longer a standalone submit + fence-wait. See
    /// [`Self::pending_dmabuf_acquires`].
    pub(super) fn record_pending_dmabuf_acquires(&mut self, cbuf: vk::CommandBuffer) {
        for tex in self.pending_dmabuf_acquires.drain(..) {
            tex.record_reacquire_dmabuf(cbuf);
        }
    }

    /// Count of client-dmabuf imports awaiting a deferred re-acquire barrier (test-only: asserts a
    /// frame drained them). See [`Self::pending_dmabuf_acquires`].
    #[cfg(test)]
    pub(super) fn pending_dmabuf_acquires_len(&self) -> usize {
        self.pending_dmabuf_acquires.len()
    }

    /// The reused present-blit shadow sized `w`×`h`, allocating one only when no shadow of that
    /// size is cached. Returns an `Arc`-clone: the caller's [`VkFramebuffer`] holds one reference
    /// and the renderer's cache the other, so dropping the frame does not free the image — it is
    /// reused next frame, and an eviction here cannot pull an image out from under a live frame.
    ///
    /// This keeps `bind` from allocating a target-sized device image every frame (the memory churn
    /// that aborts Venus under sustained rendering). Safe because rendering is synchronous
    /// (`finish` CPU-waits), so no shadow is read or written by two frames at once — including
    /// the shadow two *different* consumers of the same size share. See
    /// [`Self::present_blit_shadows`].
    /// The cached staging image for a converting readback of `w`×`h` into `want`'s byte order,
    /// allocating on a miss and LRU-evicting to stay under [`MAX_READBACK_STAGING`].
    fn readback_staging_for(
        &mut self,
        w: u32,
        h: u32,
        want: Fourcc,
    ) -> Result<VkTexture, VulkanError> {
        let (format, _ignores_alpha) =
            import_format(want).ok_or(VulkanError::UnsupportedFormat(want))?;

        self.staging_clock += 1;
        let now = self.staging_clock;
        let key = (w, h, format.as_raw());

        if let Some(entry) = self.readback_staging.get_mut(&key) {
            entry.last_used = now;
            return Ok(entry.texture.clone());
        }

        // Transfer-only: never rendered into, never sampled. `new_present_target` is the VkTexture
        // shape with no framebuffer and no descriptor set, which is exactly that.
        let tex = NiriTexture::new_transfer_image(&self.gpu, w, h, format)?;
        let staging = VkTexture::new_present_target(self.gpu.clone(), tex, w, h, want);
        #[cfg(test)]
        {
            self.readback_staging_allocs += 1;
        }

        // Same reasoning as the present-blit shadows: evicting a size that is read back every frame
        // would reallocate it every frame, which is the Venus blob churn this cache prevents. If
        // this logs steadily, the cap is too low.
        if self.readback_staging.len() >= MAX_READBACK_STAGING {
            if let Some(&lru) = self
                .readback_staging
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key)
            {
                debug!("evicting readback staging image {}x{}", lru.0, lru.1);
                self.readback_staging.remove(&lru);
            }
        }

        self.readback_staging.insert(
            key,
            StagingEntry {
                texture: staging.clone(),
                last_used: now,
            },
        );
        Ok(staging)
    }

    fn present_blit_shadow_for(
        &mut self,
        w: u32,
        h: u32,
        filter: vk::Filter,
    ) -> Result<VkTexture, VulkanError> {
        self.shadow_clock += 1;
        let now = self.shadow_clock;

        if let Some(entry) = self.present_blit_shadows.get_mut(&(w, h)) {
            entry.last_used = now;
            return Ok(entry.texture.clone());
        }

        let shadow_tex = NiriTexture::new_color_target(&self.gpu, w, h, filter)?;
        let framebuffer = self.dmabuf_framebuffer(&shadow_tex, w, h)?;
        let shadow = VkTexture::new_dmabuf_target(
            self.gpu.clone(),
            shadow_tex,
            framebuffer,
            w,
            h,
            Fourcc::Abgr8888,
        );
        #[cfg(test)]
        {
            self.present_blit_shadow_allocs += 1;
        }

        // Evict the least-recently-used shadow first, so a churn of new sizes stays bounded. Only
        // ever over the cap by one (we insert one per miss), so a single eviction suffices.
        //
        // Evicting a size that is still bound every frame would put us back to reallocating it on
        // the next frame — the churn this cache exists to prevent — so say so: if this logs every
        // frame, more than `MAX_PRESENT_BLIT_SHADOWS` sizes are live and the cap is too low.
        if self.present_blit_shadows.len() >= MAX_PRESENT_BLIT_SHADOWS {
            if let Some(&lru) = self
                .present_blit_shadows
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(size, _)| size)
            {
                debug!(
                    "evicting the {}x{} present-blit shadow to make room for {w}x{h}",
                    lru.0, lru.1,
                );
                self.present_blit_shadows.remove(&lru);
            }
        }
        self.present_blit_shadows.insert(
            (w, h),
            ShadowEntry {
                texture: shadow.clone(),
                last_used: now,
            },
        );
        Ok(shadow)
    }

    /// How many present-blit shadow images have been allocated (test-only observability).
    /// See [`Self::present_blit_shadow_allocs`].
    #[cfg(test)]
    pub(super) fn present_blit_shadow_allocs(&self) -> usize {
        self.present_blit_shadow_allocs
    }

    #[cfg(test)]
    pub(super) fn readback_staging_allocs(&self) -> usize {
        self.readback_staging_allocs
    }

    #[cfg(test)]
    pub(super) fn readback_buffer_allocs(&self) -> usize {
        self.readback_buffer_allocs
    }

    fn dmabuf_framebuffer(
        &self,
        tex: &NiriTexture,
        w: u32,
        h: u32,
    ) -> Result<vk::Framebuffer, VulkanError> {
        let fb_ci = vk::FramebufferCreateInfo::default()
            .render_pass(self.render_pass)
            .attachments(std::slice::from_ref(&tex.view))
            .width(w)
            .height(h)
            .layers(1);
        unsafe { self.gpu.device.create_framebuffer(&fb_ci, None) }.map_err(VulkanError::from)
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

/// The DRM formats the owned renderer imports as client buffers: the four 8888 byte orders,
/// **LINEAR modifier only** (all Venus exposes for them). This is advertised to clients as dmabuf
/// feedback so they allocate buffers [`VulkanRenderer::import_dmabuf_as_texture`] can import; the
/// tty backend uses it in place of the GLES renderer's formats on the Vulkan path.
pub fn dmabuf_formats() -> FormatSet {
    [
        Fourcc::Argb8888,
        Fourcc::Xrgb8888,
        Fourcc::Abgr8888,
        Fourcc::Xbgr8888,
    ]
    .into_iter()
    .map(|code| Format {
        code,
        modifier: Modifier::Linear,
    })
    .collect()
}

impl VulkanRenderer {
    /// The shm-cache hit path: re-upload `data` (tightly-packed `w*h*4` bytes) into `tex`'s
    /// existing image, reusing the renderer's staging buffer — no image or staging allocation.
    /// Safe against a concurrent sample because every VulkanRenderer submit is immediately
    /// fence-waited (the renderer is fully synchronous), so no in-flight frame can be sampling
    /// `tex` here; if async submission is ever added this needs per-texture fence tracking.
    pub(super) fn reupload_shm(&mut self, tex: &VkTexture, data: &[u8]) -> Result<(), VulkanError> {
        // `reupload_full` copies the image's full w*h extent out of the staging, so short data
        // would be an out-of-bounds staging read. The sole caller only reaches here on a
        // size-matched cache hit with a 32bpp shm buffer; this pins that contract rather
        // than guarding at runtime.
        debug_assert_eq!(
            data.len(),
            (tex.size().w as usize) * (tex.size().h as usize) * 4,
            "reupload_shm data must be the texture's w*h*4 (32bpp) extent",
        );
        self.shm_staging.ensure(&self.gpu, data.len() as u64)?;
        self.shm_staging.write(&self.gpu, data)?;
        tex.reupload_shm(self.command_pool, &self.shm_staging)?;
        Ok(())
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
        let Some((vk_format, alpha_one)) = import_format(format) else {
            return Err(VulkanError::UnsupportedFormat(format));
        };
        let (w, h) = (size.w.max(0) as u32, size.h.max(0) as u32);
        // `ImportMem`'s contract is tightly packed `w*h*4` bytes (`import_shm_buffer` repacks
        // strided shm into this shape before calling here).
        let expected = (w as usize) * (h as usize) * 4;
        if data.len() < expected {
            return Err(VulkanError::Other(format!(
                "import_memory: {} bytes for {w}x{h}, need {expected}",
                data.len()
            )));
        }
        let filter = match self.upscale_filter {
            TextureFilter::Linear => vk::Filter::LINEAR,
            TextureFilter::Nearest => vk::Filter::NEAREST,
        };
        let tex = NiriTexture::from_bytes_32bpp(
            &self.gpu,
            self.command_pool,
            w,
            h,
            &data[..expected],
            vk_format,
            alpha_one,
            filter,
        )?;
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
        // ARGB/XRGB (BGRA byte order) are what most toolkits send over wl_shm; ABGR/XBGR too.
        Box::new(
            [
                Fourcc::Argb8888,
                Fourcc::Xrgb8888,
                Fourcc::Abgr8888,
                Fourcc::Xbgr8888,
            ]
            .into_iter(),
        )
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
        // Any 32-bpp 8888 order — RGBA (Abgr/Xbgr) for the offscreen/direct path, BGRA (Argb/Xrgb)
        // for a present-blit scanout buffer, or for a BGRA consumer reading an RGBA frame back.
        if import_format(format).is_none() {
            return Err(VulkanError::UnsupportedFormat(format));
        }

        // On the present-blit path the bytes actually scanned out live in the dmabuf (`present`),
        // not the R8G8B8A8 shadow — read the real target.
        let source = target.present.as_ref().unwrap_or(&target.buffer);
        let w = region.size.w.max(0) as u32;
        let h = region.size.h.max(0) as u32;

        // `download_region` copies raw bytes, so they arrive in the *source image's* order. If the
        // caller wants the other order, blit through a staging image of that format on the way out
        // and let the GPU reorder the channels — no CPU pass over the pixels. (Reading the source's
        // own order is the common case and stays a plain copy.)
        let source_order = source.format().map(is_rgba8888);
        let want_order = is_rgba8888(format);
        let via = match source_order {
            Some(order) if order != want_order => Some(self.readback_staging_for(w, h, format)?),
            _ => None,
        };

        let data = self.download_region(source, region.loc.x, region.loc.y, w, h, via.as_ref())?;
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

/// The subpass dependencies shared by [`create_render_pass`] and
/// [`create_continuation_render_pass`]. They **must be byte-identical** between the two passes:
/// render-pass compatibility (Vulkan spec §8.2, and enforced field-by-field by the validation
/// layers) requires matching subpass dependencies, and the continuation pass relies on being
/// compatible with the base pass so pipelines built against the base pass bind in it. Building both
/// from this one helper is what guarantees they can't drift apart.
///
/// The masks are the union of what either pass needs:
/// - EXTERNAL→0 also orders a preceding capture blit (`VulkanFrame::capture_region`, `TRANSFER` /
///   `TRANSFER_READ`) and the continuation pass's `LOAD` (`COLOR_ATTACHMENT_READ`) before this
///   pass's color use. Widening these on the base pass (whose attachment starts `UNDEFINED`, first
///   use) only enlarges the ordering scope — harmless.
/// - 0→EXTERNAL makes color writes available to the following transfer read (the readback /
///   present-blit, or the next capture blit).
fn split_compatible_deps() -> [vk::SubpassDependency; 2] {
    [
        vk::SubpassDependency::default()
            .src_subpass(vk::SUBPASS_EXTERNAL)
            .dst_subpass(0)
            .src_stage_mask(
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT | vk::PipelineStageFlags::TRANSFER,
            )
            .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .src_access_mask(
                vk::AccessFlags::COLOR_ATTACHMENT_WRITE | vk::AccessFlags::TRANSFER_READ,
            )
            .dst_access_mask(
                vk::AccessFlags::COLOR_ATTACHMENT_WRITE | vk::AccessFlags::COLOR_ATTACHMENT_READ,
            ),
        vk::SubpassDependency::default()
            .src_subpass(0)
            .dst_subpass(vk::SUBPASS_EXTERNAL)
            .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
            .dst_stage_mask(vk::PipelineStageFlags::TRANSFER)
            .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
            .dst_access_mask(vk::AccessFlags::TRANSFER_READ),
    ]
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
    // Byte-identical to the continuation pass — see `split_compatible_deps`.
    let deps = split_compatible_deps();
    let ci = vk::RenderPassCreateInfo::default()
        .attachments(std::slice::from_ref(&attachment))
        .subpasses(std::slice::from_ref(&subpass))
        .dependencies(&deps);
    unsafe { dev.create_render_pass(&ci, None) }.map_err(VulkanError::from)
}

/// The **continuation** render pass used after a mid-frame capture (see
/// [`VulkanRenderer::continuation_render_pass`] and `VulkanFrame::capture_region`). It is
/// render-pass *compatible* with [`create_render_pass`] — identical attachment format/samples, a
/// single one-color-attachment subpass, and (crucially) the same [`split_compatible_deps`], so
/// every pipeline built against the base pass binds unchanged — but differs in the ops that don't
/// affect compatibility: `load_op = LOAD` and `initial_layout = TRANSFER_SRC_OPTIMAL` (the capture
/// leaves the target there), so the scene-so-far is preserved rather than discarded. `final_layout`
/// stays `TRANSFER_SRC_OPTIMAL` so `finish`/readback/a further capture see the same layout the base
/// pass produces.
fn create_continuation_render_pass(dev: &ash::Device) -> Result<vk::RenderPass, VulkanError> {
    let attachment = vk::AttachmentDescription::default()
        .format(IMAGE_VK_FORMAT)
        .samples(vk::SampleCountFlags::TYPE_1)
        .load_op(vk::AttachmentLoadOp::LOAD)
        .store_op(vk::AttachmentStoreOp::STORE)
        .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
        .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
        .initial_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
        .final_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL);
    let color_ref = vk::AttachmentReference::default()
        .attachment(0)
        .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);
    let subpass = vk::SubpassDescription::default()
        .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
        .color_attachments(std::slice::from_ref(&color_ref));
    let deps = split_compatible_deps();
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
    // vk handles have no RAII: each fallible step past `vert` must destroy what precedes it before
    // bailing, or a failed pipeline build (e.g. a user shader rejected at pipeline-creation time
    // via `set_custom_shader`) leaks a shader module / pipeline layout.
    let vert = load_module(dev, vert_spv)?;
    let frag = match load_module(dev, frag_spv) {
        Ok(frag) => frag,
        Err(e) => {
            unsafe { dev.destroy_shader_module(vert, None) };
            return Err(e.into());
        }
    };

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
    let layout = match unsafe {
        dev.create_pipeline_layout(
            &vk::PipelineLayoutCreateInfo::default()
                .set_layouts(set_layouts)
                .push_constant_ranges(std::slice::from_ref(&push_range)),
            None,
        )
    } {
        Ok(layout) => layout,
        Err(e) => {
            unsafe {
                dev.destroy_shader_module(vert, None);
                dev.destroy_shader_module(frag, None);
            }
            return Err(e.into());
        }
    };

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
    let pipeline = match unsafe {
        dev.create_graphics_pipelines(vk::PipelineCache::null(), &[pipeline_ci], None)
    } {
        Ok(pipelines) => pipelines[0],
        Err((_, e)) => {
            unsafe {
                dev.destroy_pipeline_layout(layout, None);
                dev.destroy_shader_module(vert, None);
                dev.destroy_shader_module(frag, None);
            }
            return Err(VulkanError::from(e));
        }
    };

    Ok(Pipeline {
        pipeline,
        layout,
        vert,
        frag,
    })
}
