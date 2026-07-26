use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use ash::vk;
use niri_vk::blur::BlurChain;
use niri_vk::gpu::{DeviceSelector, Gpu, ModifierSupport};
use niri_vk::render::{
    load_module, sampler_set_layout, BorderPush, ClippedTexturePush, PostprocessPush, QuadPush,
    ResizePush, ShadowPush, COLOR_RANGE,
};
use niri_vk::shaders::{
    BORDER_FRAG, BORDER_VERT, CLIPPED_TEX_FRAG, GRADIENT_FADE_FRAG, POSTPROCESS_FRAG,
    POSTPROCESS_VERT, QUAD_VERT, RESIZE_FRAG, RESIZE_VERT, ROUNDED_TEX_FRAG, SDF_FRAG, SHADOW_FRAG,
    SHADOW_VERT, SOLID_FRAG, TEXT_FRAG, TEXT_VERT, TEX_FRAG,
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
use tracing::warn;

use super::blur_chain::SharedBlurChain;
use super::custom::{compile_custom, CustomShaderType};
use super::error::VulkanError;
use super::fence::VkSubmitFence;
use super::frame::VulkanFrame;
use super::types::{
    import_format, is_rgba8888, GlyphRun, VkFramebuffer, VkMapping, VkTexture, IMAGE_VK_FORMAT,
};
use crate::render_helpers::blur::BlurOptions;

/// One host-memory buffer to import in a batch ([`VulkanRenderer::import_memory_batch`]): tight
/// `w*h*4` bytes, its DRM `Fourcc`, the buffer size, and whether it is y-flipped.
pub type MemImportItem<'a> = (&'a [u8], Fourcc, Size<i32, BufferCoord>, bool);

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
    pub(super) sdf_rect_pipeline: Pipeline,
    pub(super) texture_pipeline: Pipeline,
    pub(super) rounded_texture_pipeline: Pipeline,
    pub(super) clipped_texture_pipeline: Pipeline,
    pub(super) gradient_fade_pipeline: Pipeline,
    pub(super) border_pipeline: Pipeline,
    pub(super) shadow_pipeline: Pipeline,
    pub(super) postprocess_pipeline: Pipeline,
    pub(super) resize_pipeline: Pipeline,
    /// The glyph material (`text.vert`/`text.frag`): samples an R8 coverage atlas at set 0. Used
    /// only when rendering UI chrome into an offscreen (identity transform); see [`TEXT_VERT`].
    pub(super) text_pipeline: Pipeline,
    /// The long-lived text stack (font system + scaler cache) behind [`Self::build_glyph_run`], so
    /// chrome redraws reshape a string without rescanning the system fonts each time.
    text_ctx: niri_vk::text::TextContext,
    /// Shaped single-line runs, keyed by `(text, px bits, bold)`. See
    /// [`Self::build_glyph_run_weighted`].
    glyph_runs: HashMap<(String, u32, bool), GlyphRun>,
    /// The image behind [`text_ctx`](Self::text_ctx)'s residency index, `None` until the first
    /// run. See [`Self::absorb_glyphs`].
    glyph_atlas: Option<GlyphAtlasImage>,
    /// Newly rasterized glyphs waiting to be copied into [`Self::glyph_atlas`], and the atlas
    /// generation they were placed in.
    ///
    /// Shaping used to upload per *line*: a frame that shaped thirteen new strings made thirteen
    /// submits into the one atlas image, ~1 ms each on this stack, where a round trip costs that
    /// much whatever it carries. They queue here instead and go in one submit at
    /// [`Self::flush_glyph_uploads`]. The generation is carried because a queued glyph's
    /// coordinates only mean anything in the atlas it was placed in.
    pending_glyphs: Vec<niri_vk::text::PendingGlyph>,
    pending_glyph_generation: u64,
    /// Bumped whenever the glyph residency is thrown away after a failed upload. Anything holding
    /// a *baked* texture has to notice: a bake that drew blank glyphs is cached under a key its
    /// widget will not change (`ui::widget::BakeCache`), so without this the text stays blank for
    /// as long as that widget's content does — not for one frame. Read by the bake caches the way
    /// they already read `context_id`.
    text_epoch: u64,
    /// Runtime-compiled user animation shaders (niri's `custom_{resize,close,open}`), each built
    /// from a config GLSL snippet by [`Self::set_custom_shader`] and `None` until one is set.
    custom_resize: Option<Pipeline>,
    custom_close: Option<Pipeline>,
    custom_open: Option<Pipeline>,
    sampler_set_layout: vk::DescriptorSetLayout,
    pub(super) command_pool: vk::CommandPool,
    /// Timestamp queries bracketing each submit, when `NIRI_FRAME_LOG=…,gpu` asked
    /// for GPU timing and the device can answer. `None` otherwise, which is the
    /// normal case — see [`GpuTimer`].
    gpu_timer: Option<GpuTimer>,
    /// Submits left running, oldest first. See [`Self::retire_completed`].
    in_flight: Vec<InFlightSubmit>,
    /// Whether the frame being finished is the one going to KMS, set by the tty backend around
    /// `DrmCompositor::render_frame`. Only that frame has somewhere to hand its fence; a
    /// screencopy or screencast render into a dmabuf looks identical from in here, and deferring
    /// those would hand an unfinished buffer to a consumer that does not expect one.
    finish_may_defer: bool,
    /// Whether this renderer defers at all, defaulting to what the session asked for. A field
    /// rather than a bare environment read so a test can exercise the path.
    defer_scanout: bool,
    /// The host-visible buffer every readback copies into, grown on demand and reused.
    ///
    /// This used to be a fresh `VkBuffer` + `HOST_VISIBLE` allocation per call. On Venus
    /// host-visible memory is a virtio-gpu blob, and shm screencopy reads back **every frame** —
    /// per-frame mappable-blob churn, which costs real host time and host pool pressure.
    /// (It used to *abort* the session; that was fixed at the VMM level in 2026-07, so this is
    /// now a performance and resource argument, not a stability one — but it is still the
    /// reason this is cached.) Grow-only, so a steady stream of same-size reads allocates once;
    /// a larger read grows it and smaller ones then reuse the larger buffer.
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
    /// reallocate on every bind as those alternate — exactly the per-frame Venus blob churn these
    /// caches exist to avoid (costly on the host; no longer an abort — see
    /// `readback_staging_buffer`).
    ///
    /// Bounded by [`MAX_PRESENT_BLIT_SHADOWS`], evicting least-recently-used, so a stream of new
    /// sizes (an interactively resized window cast renegotiates its size as it grows) cannot grow
    /// the cache without bound; the sizes actually bound each frame stay resident and never
    /// reallocate. Reuse and eviction are safe because a deferred frame's record holds its target
    /// ([`InFlightSubmit::_targets`]): evicting a shadow removes it from this map but the image
    /// lives until the submit retires. (Before that record existed, the guarantee was that
    /// rendering is synchronous — which stopped being true when the scanout submit learned to
    /// defer.)
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
    /// frame, and allocating a `VkImage` per frame churns the Venus blob pool (host cost and
    /// pressure; no longer an abort — see `readback_staging_buffer`).
    /// Keyed by size **and** format; LRU-evicted under [`MAX_READBACK_STAGING`]. Reuse
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
    /// creates a host-side resource, so re-importing per frame churns those resources on the host
    /// (this used to drive the guest↔host ring to `FATAL` and abort in `vn_ring_submit`; fixed at
    /// the VMM level 2026-07, so the remaining cost is host time and resource pressure).
    /// Cache each imported target and reuse it across frames;
    /// entries whose buffer was freed are evicted. Reuse and eviction are safe for the same reason
    /// as the shadow: a deferred frame's record holds what it renders into
    /// ([`InFlightSubmit::_targets`]). Mirrors `GlesRenderer`'s `buffers` bound-dmabuf cache.
    dmabuf_target_cache: HashMap<WeakDmabuf, VkTexture>,
    /// Imported **client** sampled dmabufs, keyed by buffer identity. smithay clears its
    /// per-surface texture on *every* new-buffer commit (even a recycled `wl_buffer`), so an
    /// animating dmabuf client (e.g. a WebGL page) would otherwise re-run the full
    /// `import_dmabuf_sampled` — a fresh `vkAllocateMemory` import +
    /// image/view/sampler/descriptor set + a fenced acquire barrier — every frame. That per-frame
    /// host-resource churn is expensive on the Venus host (and used to pressure the guest↔host
    /// ring into a `FATAL`/alive-expiry abort — fixed at the VMM level 2026-07; the historical
    /// analysis is in `DEVMAC-SESSION-confusion.md`). Clients recycle a
    /// small buffer pool, so cache the imported texture per buffer and reuse it; a hit only needs
    /// the acquire barrier (`VkTexture::record_reacquire_dmabuf`) to pick up the freshly produced
    /// content — no allocation. Entries whose buffer was freed are evicted lazily on lookup. This
    /// is the sampled-import dual of [`Self::dmabuf_target_cache`] (which needs no re-acquire
    /// because the compositor, not an external producer, writes the scanout target).
    /// Reuse/eviction is safe because a frame that samples one of these has already retained it
    /// (`VulkanFrame::retain` → [`InFlightSubmit::_held`]), so dropping the cache entry only
    /// releases *our* reference — the image lives until the submit that reads it retires.
    dmabuf_import_cache: HashMap<WeakDmabuf, VkTexture>,

    /// `(format, modifier)` pairs we have already warned about being absent from the driver's
    /// modifier list. See [`Self::check_modifier`] — the warning is per pair, not per import,
    /// since a buffer pool cycling through the caches would otherwise reprint it forever.
    warned_modifiers: HashSet<(vk::Format, u64)>,

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

    /// Host-staged texture uploads whose GPU copy has not been recorded yet. Drained by
    /// [`super::VulkanFrame::begin`] into the frame's own command buffer, in the same slot (and
    /// for the same reason) as [`Self::pending_dmabuf_acquires`] and the glyph uploads.
    ///
    /// `import_memory` used to submit **and block** per texture. A live seat frame was measured at
    /// `9 upload in 16.22ms` moving 1.0 MiB — the bytes cost 0.24 ms of that, so the other 16 ms
    /// was nine round trips. Each blocking wait also idles the guest↔host ring past its 1 ms
    /// timeout, so the *next* submit pays a ~1 ms wake on top (`docs/fork/venus-cost.md` §9.4):
    /// the waits were buying each other.
    ///
    /// Invariant, identical to the acquires': a [`super::VulkanFrame`] mutably borrows the
    /// renderer, so no import can happen while a frame exists — every push therefore precedes some
    /// `begin()`, and every `begin()` drains.
    ///
    /// What makes deferring *safe* rather than merely ordered is that nothing reads an imported
    /// texture's contents outside a frame: `ExportMem::copy_texture` is `Unsupported` here and
    /// `can_read_texture` is `false`, so every read goes through `copy_framebuffer` — i.e. through
    /// something a frame rendered, after this drained. Should either of those ever be implemented,
    /// it must record these copies (or submit them) before reading, or it will read a blank image.
    ///
    /// A queue left undrained (elements built, then damage tracking skips the frame) is harmless:
    /// the entries simply wait for the next `begin`, and if the renderer dies first they free
    /// their staging and leave a blank image behind — never an invalid one. It cannot grow without
    /// bound while undrained either — see [`PendingTextureUpload`].
    pending_texture_uploads: Vec<PendingTextureUpload>,

    /// Freshly created offscreens waiting for the barrier that makes them sampleable, drained into
    /// the next frame's command buffer by [`super::VulkanFrame::begin`]. See
    /// [`Self::make_sampleable`], which is where the "fresh" qualifier is decided and defended.
    pending_sampleable: Vec<VkTexture>,

    /// Blurs queued while collecting elements, drained into the next frame's command buffer by
    /// [`super::VulkanFrame::begin`]. Same slot, same invariant and same reason as
    /// [`Self::pending_texture_uploads`]: the frame is submitting anyway, and on this stack a
    /// submit costs a host round trip whatever it carries.
    pending_blurs: Vec<PendingBlur>,

    /// The staging every queued upload's pixels are written into: one shared, grow-only buffer
    /// rewound per frame, rather than a mappable blob per upload. See
    /// [`niri_vk::staging::StagingPool`] — the per-upload version ran the Venus host out of blobs
    /// two minutes into a live session.
    staging_pool: niri_vk::staging::StagingPool,
}

/// A blur waiting for a frame to record it, with everything it names held alive.
///
/// The xray effect buffer builds its blur while collecting elements, where no command buffer is
/// open — so unlike the backdrop blur (which is invoked mid-frame and records straight into the
/// gap `capture_region` opens) it has to be queued. See [`VulkanRenderer::queue_blur`].
struct PendingBlur {
    chain: Arc<SharedBlurChain>,
    /// What the chain samples, and what it writes. Both held for the usual reason: the copy and
    /// the passes name these images, and the submit that runs them has not happened yet.
    source: VkTexture,
    output: VkTexture,
    offset: f32,
}

/// One staged texture upload waiting for a frame to record its copy, **with its destination held
/// alive**.
///
/// The `VkTexture` is here for the reason `record_pending_dmabuf_acquires` returns its images:
/// recording stores a handle, so the destination has to outlive the *submit*, not the recording
/// (`docs/fork/frame-submit-discipline.md`). [`niri_vk::texture::StagedTexture`] deliberately
/// borrows its image by handle and owns only the staging half, so without this reference the only
/// thing keeping the image alive is whoever else happens to hold the texture — the shm cache in
/// the surface's `data_map`, or the element being drawn. A client that commits and then destroys
/// its surface before the next frame drops both, and `begin` then records a copy into a destroyed
/// `VkImage`, which poisons the whole frame's command buffer. That is undefined behavior, so it
/// surfaces as whatever the driver felt like doing: on the live seat it came back as
/// `ERROR_OUT_OF_HOST_MEMORY` from every later allocation, and only the validation layer named it.
struct PendingTextureUpload {
    /// The destination, held strongly. See the type docs — this reference is the invariant.
    tex: VkTexture,
    staged: niri_vk::texture::StagedTexture,
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

/// Cap on cached glyph runs. Each pins its own R8 coverage atlas (a few tens of KB
/// at panel/label sizes), and the live set is the strings actually on screen — every
/// panel label, dash tooltip and app name at once is well under this. See
/// [`VulkanRenderer::build_glyph_run_weighted`].
const GLYPH_RUN_CACHE_CAP: usize = 256;

/// The persistent glyph-atlas image, paired with the generation of the residency index that
/// describes it. Recreated (never resized in place) when that generation moves; runs built
/// against an older image keep it alive through their own reference.
struct GlyphAtlasImage {
    texture: VkTexture,
    generation: u64,
    side: u32,
}

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
        // Every material below blends premultiplied-over; see `build_pipeline`'s alpha convention.
        let solid_pipeline =
            build_pipeline(&gpu, render_pass, QUAD_VERT, SOLID_FRAG, &[], quad_push)?;
        // Rounded solid-color fill: the `sdf_rect.frag` box-SDF material, samples nothing (no set)
        // and shares `QuadPush` (uses only origin/size/corner_radius/color) — the toolkit's
        // rounded-rect primitive (tile/pill/menu backgrounds).
        let sdf_rect_pipeline =
            build_pipeline(&gpu, render_pass, QUAD_VERT, SDF_FRAG, &[], quad_push)?;
        let texture_pipeline =
            build_pipeline(&gpu, render_pass, QUAD_VERT, TEX_FRAG, sampler, quad_push)?;
        let rounded_texture_pipeline = build_pipeline(
            &gpu,
            render_pass,
            QUAD_VERT,
            ROUNDED_TEX_FRAG,
            sampler,
            quad_push,
        )?;
        // Clipped-surface: samples a texture (set 0) and clips it to a rounded geometry.
        let clipped_texture_pipeline = build_pipeline(
            &gpu,
            render_pass,
            QUAD_VERT,
            CLIPPED_TEX_FRAG,
            sampler,
            std::mem::size_of::<ClippedTexturePush>() as u32,
        )?;
        let gradient_fade_pipeline = build_pipeline(
            &gpu,
            render_pass,
            QUAD_VERT,
            GRADIENT_FADE_FRAG,
            sampler,
            quad_push,
        )?;
        // The border/shadow materials sample nothing (no set).
        let border_pipeline = build_pipeline(
            &gpu,
            render_pass,
            BORDER_VERT,
            BORDER_FRAG,
            &[],
            std::mem::size_of::<BorderPush>() as u32,
        )?;
        let shadow_pipeline = build_pipeline(
            &gpu,
            render_pass,
            SHADOW_VERT,
            SHADOW_FRAG,
            &[],
            std::mem::size_of::<ShadowPush>() as u32,
        )?;
        // Postprocess-and-clip samples a texture (set 0).
        let postprocess_pipeline = build_pipeline(
            &gpu,
            render_pass,
            POSTPROCESS_VERT,
            POSTPROCESS_FRAG,
            sampler,
            std::mem::size_of::<PostprocessPush>() as u32,
        )?;
        // Resize cross-fade samples two textures (set 0 = prev, set 1 = next).
        let resize_pipeline = build_pipeline(
            &gpu,
            render_pass,
            RESIZE_VERT,
            RESIZE_FRAG,
            &[sampler_set_layout, sampler_set_layout],
            std::mem::size_of::<ResizePush>() as u32,
        )?;
        // The glyph material samples the R8 coverage atlas (set 0), coverage-modulating the tint.
        let text_pipeline = build_pipeline(
            &gpu,
            render_pass,
            TEXT_VERT,
            TEXT_FRAG,
            sampler,
            std::mem::size_of::<niri_vk::render::TextPush>() as u32,
        )?;
        let command_pool = {
            let ci = vk::CommandPoolCreateInfo::default()
                .queue_family_index(gpu.queue_family)
                .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER);
            unsafe { gpu.device.create_command_pool(&ci, None) }?
        };

        let gpu_for_timer = Arc::clone(&gpu);
        let staging_pool = niri_vk::staging::StagingPool::new(&gpu);
        Ok(VulkanRenderer {
            gpu,
            context_id: ContextId::new(),
            render_pass,
            continuation_render_pass,
            solid_pipeline,
            sdf_rect_pipeline,
            texture_pipeline,
            rounded_texture_pipeline,
            clipped_texture_pipeline,
            gradient_fade_pipeline,
            border_pipeline,
            shadow_pipeline,
            postprocess_pipeline,
            resize_pipeline,
            text_pipeline,
            text_ctx: niri_vk::text::TextContext::new(),
            glyph_runs: HashMap::new(),
            glyph_atlas: None,
            pending_glyphs: Vec::new(),
            pending_glyph_generation: 0,
            text_epoch: 0,
            custom_resize: None,
            custom_close: None,
            custom_open: None,
            sampler_set_layout,
            command_pool,
            gpu_timer: GpuTimer::if_requested(&gpu_for_timer)?,
            in_flight: Vec::new(),
            finish_may_defer: false,
            defer_scanout: deferred_scanout_requested(),
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
            warned_modifiers: HashSet::new(),
            pending_dmabuf_acquires: Vec::new(),
            pending_texture_uploads: Vec::new(),
            pending_blurs: Vec::new(),
            pending_sampleable: Vec::new(),
            staging_pool,
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
    /// Fed live from the config by [`Self::set_custom_resize_shader`] and friends.
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
                )?;
                Some(pipeline)
            }
        };

        // Swap in the new slot value, then destroy the old pipeline. `&mut self` means no frame is
        // recording (a `VulkanFrame` borrows the renderer mutably), but a *submitted* frame can
        // still be in flight and its command buffer may have bound this pipeline — and a pipeline
        // is not ref-counted, so nothing can hold it the way `InFlightSubmit` holds textures.
        // Drain first. This runs on a config reload, not per frame.
        let old = std::mem::replace(self.custom_slot_mut(ty), new);
        if let Some(old) = old {
            self.drain_in_flight();
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

    /// Install (or clear, with `None`) the user's custom **resize** animation shader, from the
    /// config install/reload sites. A compile error is logged and leaves the previous shader in
    /// place (graceful degrade).
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
        // Free the pool if set allocation fails (e.g. host-OOM under Venus pressure) — a bare `?`
        // here would orphan it, and the batch path calls this once per icon.
        let set = match unsafe {
            dev.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(pool)
                    .set_layouts(&layouts),
            )
        } {
            Ok(sets) => sets[0],
            Err(err) => {
                unsafe { dev.destroy_descriptor_pool(pool, None) };
                return Err(err.into());
            }
        };
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

    /// The device this renderer draws on, for the few producers that build GPU resources off the
    /// render thread ([`niri_vk::staging::HostStaging`], filled by the wallpaper decoder). Handing
    /// out the `Arc` is what keeps the device alive for as long as such a producer holds one.
    pub fn gpu(&self) -> &Arc<niri_vk::gpu::Gpu> {
        &self.gpu
    }

    /// Import a texture whose pixels a **worker thread already wrote into device-visible memory**
    /// ([`niri_vk::staging::HostStaging`]). The counterpart of `import_memory`, minus the part that
    /// costs: the multi-megabyte host write happened on the producer's thread, so all that is left
    /// here is creating the image, recording the copy and submitting it.
    ///
    /// Errors if the staging belongs to a *different* device — a renderer recreated under a decode
    /// in flight. That is not recoverable here (there is no host copy left to fall back to), so the
    /// caller re-requests the decode.
    pub fn import_host_staging(
        &mut self,
        staging: &niri_vk::staging::HostStaging,
        format: Fourcc,
        size: Size<i32, BufferCoord>,
    ) -> Result<VkTexture, VulkanError> {
        if !staging.belongs_to(&self.gpu) {
            return Err(VulkanError::Other(
                "staged pixels belong to a device this renderer replaced".to_owned(),
            ));
        }
        let Some((vk_format, alpha_one)) = import_format(format) else {
            return Err(VulkanError::UnsupportedFormat(format));
        };
        let (w, h) = (size.w.max(0) as u32, size.h.max(0) as u32);
        if w == 0 || h == 0 {
            return Err(VulkanError::Other(format!(
                "import_host_staging: zero extent {w}x{h}"
            )));
        }
        let filter = match self.upscale_filter {
            TextureFilter::Linear => vk::Filter::LINEAR,
            TextureFilter::Nearest => vk::Filter::NEAREST,
        };
        let tex = NiriTexture::from_host_staging(
            &self.gpu,
            self.command_pool,
            staging,
            w,
            h,
            vk_format,
            alpha_one,
            filter,
        )
        .map_err(|e| VulkanError::Other(format!("staged upload: {e:#}")))?;
        // `NiriTexture` has no `Drop`, so free it if the descriptor set fails (matches
        // `import_memory`).
        let (desc_pool, set) = match self.make_texture_set(&tex) {
            Ok(v) => v,
            Err(err) => {
                tex.destroy(&self.gpu);
                return Err(err);
            }
        };
        Ok(VkTexture::new(
            self.gpu.clone(),
            tex,
            desc_pool,
            set,
            w,
            h,
            format,
            false,
        ))
    }

    /// Import many host-memory buffers as textures at once — the overview app grid's ~24 icons on
    /// first open. Each item is `(tight w*h*4 bytes, its `Fourcc`, size, flipped)`; the returned
    /// textures are in the same order.
    ///
    /// **This is N ordinary imports now, and that is the point.** The batch existed because the
    /// per-texture path cost a submit and a fence wait each, on top of a staging buffer each —
    /// create, allocate, bind, map, unmap, five host round trips per icon, measured at 65% of the
    /// app-grid open frame. Both halves are properties of the ordinary path today rather than of a
    /// batch: [`Self::import_memory`] stages into the shared pool (one buffer, N offsets) and
    /// queues its copy for the next frame's command buffer, which costs no submit at all. Twenty
    /// four icons imported one at a time and twenty four imported "as a batch" are the same GPU
    /// work, so the batch is only a validation-and-order convenience.
    ///
    /// Every item is checked before any is imported, so a bad one fails the call rather than
    /// leaving half a page uploaded.
    pub fn import_memory_batch(
        &mut self,
        items: &[MemImportItem],
    ) -> Result<Vec<VkTexture>, VulkanError> {
        for (data, format, size, _flipped) in items {
            if import_format(*format).is_none() {
                return Err(VulkanError::UnsupportedFormat(*format));
            }
            let (w, h) = (size.w.max(0) as u32, size.h.max(0) as u32);
            // A zero extent would build an invalid (0-size) image; reject it up front rather than
            // trip a validation error deeper in.
            if w == 0 || h == 0 {
                return Err(VulkanError::Other(format!(
                    "import_memory_batch: zero extent {w}x{h}"
                )));
            }
            let expected = (w as usize) * (h as usize) * 4;
            if data.len() < expected {
                return Err(VulkanError::Other(format!(
                    "import_memory_batch: {} bytes for {w}x{h}, need {expected}",
                    data.len()
                )));
            }
        }

        let mut out = Vec::with_capacity(items.len());
        for (data, format, size, flipped) in items {
            out.push(self.import_memory(data, *format, *size, *flipped)?);
        }
        Ok(out)
    }

    /// Shape and rasterize `text` at `px` pixels-per-em into a [`GlyphRun`] — an R8 coverage atlas
    /// wrapped as a sampleable [`VkTexture`] plus the per-glyph placements. Reuses the renderer's
    /// long-lived [`text_ctx`](Self::text_ctx), so a chrome redraw reshapes the string without
    /// rescanning the system fonts. Draw it with [`VulkanFrame::render_glyphs`].
    // Non-test dead until the panel draw-layer (increment 3) calls it; exercised now by the
    // `vulkan_render_glyphs_rasterizes_coverage` test.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn build_glyph_run(&mut self, text: &str, px: f32) -> Result<GlyphRun, VulkanError> {
        self.build_glyph_run_weighted(text, px, false)
    }

    /// Like [`Self::build_glyph_run`], but rasterizes bold when `bold` — the panel clock draws
    /// `font-weight: bold` to match GNOME's `panel_button`.
    ///
    /// Cached by `(text, px, bold)`: a [`GlyphRun`] is immutable and cheap to clone
    /// (ref-counted atlas), and an identical string re-shaped at the same size can
    /// only produce an identical run. Building one costs a shape, a fresh atlas
    /// image and the upload submit behind it, so the labels that do not change
    /// between frames — every one but the clock — should pay that once.
    pub(crate) fn build_glyph_run_weighted(
        &mut self,
        text: &str,
        px: f32,
        bold: bool,
    ) -> Result<GlyphRun, VulkanError> {
        // `px` by bits: it is a float only because font sizes scale, and two sizes
        // that compare equal rasterize identically.
        let key = (text.to_owned(), px.to_bits(), bold);
        if let Some(run) = self.glyph_runs.get(&key) {
            return Ok(run.clone());
        }

        let (shaped, atlas) = self.shape_line(text, px, bold)?;
        let run = GlyphRun::new(
            atlas.0,
            shaped.glyphs,
            shaped.spans,
            atlas.1,
            (shaped.baseline, shaped.ascent, shaped.descent),
        );

        // Bounded by clearing wholesale: a clock showing seconds mints one never-reused
        // key per second. Entries no longer pin an atlas image of their own — they share
        // the persistent one — so this bounds bookkeeping, not GPU memory.
        if self.glyph_runs.len() >= GLYPH_RUN_CACHE_CAP {
            self.glyph_runs.clear();
        }
        self.glyph_runs.insert(key, run.clone());
        Ok(run)
    }

    /// Lay out a styled, center-aligned paragraph (each [`TextSpan`](niri_vk::text::TextSpan)
    /// carries its own family/weight/size) wrapped to `wrap_px`, into a single [`GlyphRun`] —
    /// placements spanning every line, against the shared coverage atlas. This is the
    /// dialog/notification text path; draw it with [`VulkanFrame::render_glyphs`] at the block
    /// origin. Reuses the renderer's long-lived [`text_ctx`](Self::text_ctx).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn build_glyph_paragraph(
        &mut self,
        spans: &[niri_vk::text::TextSpan],
        wrap_px: f32,
        base_px: f32,
    ) -> Result<GlyphRun, VulkanError> {
        let gpu = self.gpu.clone();
        let pool = self.command_pool;
        let (shaped, pending) = self.text_ctx.shape_paragraph(spans, wrap_px, base_px)?;
        let (atlas, side) = self.absorb_glyphs(&gpu, pool, pending)?;
        Ok(GlyphRun::new(
            atlas,
            shaped.glyphs,
            shaped.spans,
            side,
            (shaped.baseline, shaped.ascent, shaped.descent),
        ))
    }

    /// Shape one line and make sure its glyphs are in the atlas image, returning the run and the
    /// `(atlas, side)` it resolved against.
    fn shape_line(
        &mut self,
        text: &str,
        px: f32,
        bold: bool,
    ) -> Result<(niri_vk::text::ShapedRun, (VkTexture, u32)), VulkanError> {
        // Split the disjoint borrows: shaping needs `&mut text_ctx`, uploading needs `&gpu`.
        let gpu = self.gpu.clone();
        let pool = self.command_pool;
        let (shaped, pending) = self.text_ctx.shape_line_weighted(text, px, bold)?;
        let atlas = self.absorb_glyphs(&gpu, pool, pending)?;
        Ok((shaped, atlas))
    }

    /// Bring the atlas image in line with the residency index and upload `pending`, returning the
    /// image and its side.
    ///
    /// The generation check comes **first and always**: a run that exhausted the atlas grew it
    /// mid-shape, and its slots refer to the new, larger image — uploading them into the old one
    /// would scribble at the wrong coordinates. Existing [`GlyphRun`]s keep their own reference to
    /// the image they were built against, so replacing ours never invalidates them.
    ///
    /// With nothing pending — the steady state once the alphabet in use is resident — this does no
    /// GPU work at all. That is the point: re-shaping a clock every second stops costing a round
    /// trip.
    fn absorb_glyphs(
        &mut self,
        gpu: &Arc<Gpu>,
        pool: vk::CommandPool,
        pending: Vec<niri_vk::text::PendingGlyph>,
    ) -> Result<(VkTexture, u32), VulkanError> {
        let side = self.text_ctx.atlas().side();
        let generation = self.text_ctx.atlas().generation();

        let stale = self
            .glyph_atlas
            .as_ref()
            .is_none_or(|atlas| atlas.generation != generation);
        if stale {
            // Anything queued belongs to the image about to be replaced, and its coordinates mean
            // nothing in the new one. Put it where it was meant to go first: a `GlyphRun` already
            // handed to a caller holds its own reference to the old image and will draw from it
            // even though the cache below is cleared. Discarding instead would blank that run.
            // Growth is a once-ever event, so the extra submit costs nothing in practice.
            self.flush_glyph_uploads();

            // Failing here is the same trap as a failed upload, one step earlier: `pending` is
            // dropped on the way out, but its glyphs were recorded resident when they were
            // rasterized, so a retry finds them "already there", emits nothing to upload, and
            // draws blank once an image finally exists. Throw the residency away on the way out.
            let made = NiriTexture::new_coverage_atlas(gpu, pool, side)
                .map_err(|e| VulkanError::Other(format!("glyph atlas: {e:#}")))
                .and_then(|texture| {
                    let set = self.make_texture_set(&texture)?;
                    Ok((texture, set))
                });
            let (texture, (desc_pool, set)) = match made {
                Ok(made) => made,
                Err(err) => {
                    self.invalidate_glyphs();
                    return Err(err);
                }
            };
            // R8 coverage, only ever sampled (never scanned out or read back), so the fourcc is
            // informational; R8 names the byte layout honestly.
            let texture = VkTexture::new(
                gpu.clone(),
                texture,
                desc_pool,
                set,
                side,
                side,
                Fourcc::R8,
                false,
            );
            self.glyph_atlas = Some(GlyphAtlasImage {
                texture,
                generation,
                side,
            });
            // Cached runs stay *correct* — each holds the image it was built against — but
            // keeping them would pin the old atlas for as long as the cache lives. Dropping
            // them lets it go; they rebuild against the new one on demand.
            self.glyph_runs.clear();
        }

        let atlas = self.glyph_atlas.as_ref().expect("just ensured");
        if !pending.is_empty() {
            debug_assert!(
                self.pending_glyphs.is_empty() || self.pending_glyph_generation == generation,
                "queued glyphs from another atlas generation — their coordinates are meaningless \
                 in this image"
            );
            self.pending_glyph_generation = generation;
            self.pending_glyphs.extend(pending);
        }
        Ok((atlas.texture.clone(), atlas.side))
    }

    /// Copy every queued glyph into the atlas image, in **one** submit, and clear the queue.
    ///
    /// Called from [`VulkanFrame::begin`](super::VulkanFrame::begin), which is both correct and
    /// the latest possible moment: the only thing that samples the atlas is a glyph draw, every
    /// glyph draw goes through a `VulkanFrame`, and a `VulkanFrame` borrows the renderer mutably
    /// — so no shaping, hence no queueing, can happen while one is open. A queued glyph therefore
    /// cannot be drawn before the next `begin` flushes it.
    ///
    /// Never fails the caller. An upload error here would otherwise be unrecoverable rather than
    /// merely ugly: a glyph is recorded resident when it is *rasterized*, before its bytes reach
    /// the GPU, and a resident glyph emits nothing to upload ever again — so a lost copy means
    /// that character stays blank for the life of the atlas. Throwing the residency away
    /// (`invalidate`) costs a re-rasterization and puts everything back.
    /// Whether any glyph is queued but not yet in the atlas image. For the assertion in
    /// `render_glyphs_with` that nothing shaped text after this frame began.
    pub(super) fn has_pending_glyphs(&self) -> bool {
        !self.pending_glyphs.is_empty()
    }

    /// See [`Self::text_epoch`](#structfield.text_epoch). Changes only when glyph residency was
    /// thrown away, so a cache comparing it clears at most once per failure.
    pub(crate) fn text_epoch(&self) -> u64 {
        self.text_epoch
    }

    /// Throw away glyph residency and everything built from it, after an upload could not be
    /// made. Everything downstream has to go: the index would otherwise keep claiming those
    /// glyphs are resident (so nothing would ever upload them again), and any run or bake made
    /// from them drew blanks.
    pub(super) fn invalidate_glyphs(&mut self) {
        self.text_ctx.atlas_mut().invalidate();
        self.glyph_runs.clear();
        self.pending_glyphs.clear();
        self.text_epoch = self.text_epoch.wrapping_add(1);
    }

    pub(super) fn flush_glyph_uploads(&mut self) {
        if self.pending_glyphs.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.pending_glyphs);
        let Some(atlas) = self.glyph_atlas.as_ref() else {
            // Glyphs are only queued after the image is ensured, so this cannot happen; drop them
            // rather than panic, and rebuild the residency so nothing is silently missing.
            debug_assert!(false, "glyphs queued with no atlas image to put them in");
            self.invalidate_glyphs();
            return;
        };

        let regions: Vec<_> = pending
            .iter()
            .map(niri_vk::text::PendingGlyph::region)
            .collect();
        let result =
            atlas
                .texture
                .inner()
                .upload_coverage_regions(&self.gpu, self.command_pool, &regions);
        if let Err(err) = result {
            warn!("glyph atlas upload failed, rebuilding the atlas: {err:#}");
            // The index still claims these glyphs are resident. Forget the lot: the next shape
            // re-rasterizes them, the generation bump recreates the image, and the text that drew
            // blank is re-baked rather than staying blank for the life of its cache entry.
            self.invalidate_glyphs();
        }
    }

    /// Fold the queued glyph copies into `cbuf` — a frame's own command buffer, before its render
    /// pass — instead of paying a standalone submit + fence wait for them. The returned staging
    /// buffer **must outlive that command buffer's submit**; the caller ([`VulkanFrame`]) hands it
    /// to the in-flight record or drops it after its fence wait, whichever branch it takes.
    ///
    /// This is the same trick as
    /// [`record_pending_dmabuf_acquires`](Self::record_pending_dmabuf_acquires): a copy has to
    /// be recorded outside a render pass, which is exactly where that slot is. It replaces the
    /// submit rather than deferring it — on an idle queue a round trip here measured
    /// ~2-3.5ms, which was most of what an uncached widget bake cost.
    ///
    /// The standalone [`flush_glyph_uploads`](Self::flush_glyph_uploads) stays: `absorb_glyphs`
    /// flushes into the *old* atlas image during shaping, where there is no frame to ride.
    ///
    /// **If the caller then fails to submit `cbuf`, it must call
    /// [`invalidate_glyphs`](Self::invalidate_glyphs)** — the residency index already claims these
    /// glyphs are in the atlas, so a copy that never runs is blank text for as long as that
    /// residency lives.
    pub(super) fn record_pending_glyph_uploads(
        &mut self,
        cbuf: vk::CommandBuffer,
    ) -> Option<niri_vk::texture::GlyphStaging> {
        if self.pending_glyphs.is_empty() {
            return None;
        }
        let pending = std::mem::take(&mut self.pending_glyphs);
        let Some(atlas) = self.glyph_atlas.as_ref() else {
            debug_assert!(false, "glyphs queued with no atlas image to put them in");
            self.invalidate_glyphs();
            return None;
        };

        let regions: Vec<_> = pending
            .iter()
            .map(niri_vk::text::PendingGlyph::region)
            .collect();
        let image = atlas.texture.inner().image;
        match atlas
            .texture
            .inner()
            .stage_coverage_regions(&self.gpu, &regions)
        {
            Ok(None) => None,
            Ok(Some(staged)) => {
                niri_vk::texture::record_coverage_copy(&self.gpu.device, cbuf, image, &staged);
                Some(staged)
            }
            Err(err) => {
                warn!("glyph atlas staging failed, rebuilding the atlas: {err:#}");
                self.invalidate_glyphs();
                None
            }
        }
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
        // NOT counted here: this function's fast path is a *cache hit*, and timing it from the
        // top counted every hit as a creation. `Texture::import_dmabuf_sampled` counts itself, on
        // the miss path only.
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
        // Client buffers are the imports most likely to arrive on a modifier we have never seen, so
        // this is the check's widest exposure. A linear sampler is gated on its own feature bit,
        // not on SAMPLED_IMAGE — and we require it unconditionally rather than only when
        // `filter` is linear today: the import is cached per buffer, not per filter, so a
        // texture imported under NEAREST keeps its cached sampler (and skips this check) if
        // `upscale_filter` later flips to LINEAR. A conditional requirement would quietly
        // stop guarding at exactly that moment.
        self.check_modifier(
            format.code,
            vk_format,
            format.modifier.into(),
            vk::FormatFeatureFlags::SAMPLED_IMAGE
                | vk::FormatFeatureFlags::SAMPLED_IMAGE_FILTER_LINEAR,
            "client buffer",
        )?;
        let tex = NiriTexture::import_dmabuf_sampled(
            &self.gpu,
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
        let tex = VkTexture::new_unacquired_dmabuf(
            self.gpu.clone(),
            tex,
            desc_pool,
            set,
            w,
            h,
            format.code,
        );
        // The import left the image `UNDEFINED`; queue its acquire onto the same list the cache-hit
        // path uses, so the barrier rides the next frame's command buffer instead of a submit and
        // fence-wait of its own. A live overview frame showed two of those standalone barriers
        // costing ~3 ms each (`docs/fork/frame-cost-investigation.md`). No de-dupe check needed
        // that the hit path needs: this image did not exist a moment ago, so it cannot be queued
        // already.
        self.pending_dmabuf_acquires.push(tex.clone());
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
        // Reading an image whose staged copy — or queued blur — has not landed would read a blank
        // or stale one. A readback of a blurred offscreen (a test, a screenshot of an xray
        // surface) is exactly that consumer.
        self.flush_pending_texture_uploads()?;
        self.flush_pending_blurs()?;
        self.flush_pending_sampleable()?;
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

        self.gpu.run_commands(
            self.command_pool,
            niri_vk::stats::SubmitSite::Readback,
            |cbuf| unsafe {
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
            },
        )?;
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
    /// it is already sampleable. Reached generically via
    /// [`crate::render_helpers::renderer::OffscreenRenderer::make_offscreen_sampleable`].
    ///
    /// **Usually a no-op**: a [`VulkanFrame`](super::VulkanFrame) targeting an offscreen already
    /// finishes it sampleable, riding the submit it was making anyway.
    ///
    /// When it is not a no-op it is almost always a **fresh** offscreen — created this frame and
    /// not yet rendered into, because the effect buffer's elements did not change and its texture
    /// had just been recreated (a size change, an overview zoom). Measured: every other path
    /// through an effect-buffer prepare costs zero transitions and that one costs one, which on a
    /// live overview frame was `2 transition in 3.03ms`, the only wait left in the line.
    ///
    /// So an `UNDEFINED` image is **queued** rather than submitted. It is the one layout where
    /// that is unconditionally safe: the barrier's source layout says the contents may be
    /// discarded, and there are none — nothing has been rendered into it yet. The two hazards a
    /// queue has to answer:
    ///
    /// - *Something samples it before the drain.* It cannot: the only consumers are draws in a
    ///   frame, a queued blur (recorded after this queue, in the same `begin`), and the
    ///   out-of-frame readbacks, which drain explicitly ([`Self::flush_pending_sampleable`]).
    /// - *Something renders into it before the drain,* which would leave a stale `UNDEFINED ->
    ///   SHADER_READ` barrier to discard the new contents. [`Self::bind`] drops the queued entry
    ///   for exactly this reason — and the render pass then leaves the image sampleable itself, so
    ///   nothing is lost.
    ///
    /// Any other layout still submits. Those carry real contents (a texture that reached a sampled
    /// state by some other route, e.g. a transfer), so the barrier is not a discard and this stays
    /// what it always was: a whole command buffer, submit and fence wait for one pipeline barrier.
    pub(crate) fn make_sampleable(&mut self, tex: &VkTexture) -> Result<(), VulkanError> {
        let old_layout = tex.layout();
        if old_layout == vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL {
            return Ok(());
        }
        if old_layout == vk::ImageLayout::UNDEFINED {
            // Nothing to preserve, so this is bookkeeping the frame can do for free. De-duped by
            // image: a fresh offscreen prepared twice before a frame needs one barrier.
            let image = tex.image();
            if !self.pending_sampleable.iter().any(|t| t.image() == image) {
                self.pending_sampleable.push(tex.clone());
            }
            tex.set_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
            return Ok(());
        }
        let image = tex.image();
        self.gpu.run_commands(
            self.command_pool,
            niri_vk::stats::SubmitSite::Transition,
            |cbuf| unsafe {
                transition_image(
                    &self.gpu.device,
                    cbuf,
                    image,
                    old_layout,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::AccessFlags::SHADER_READ,
                    vk::PipelineStageFlags::FRAGMENT_SHADER,
                );
            },
        )?;
        tex.set_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        Ok(())
    }

    /// Record every queued make-sampleable barrier into `cbuf`, and hand back the images so they
    /// outlive the submit. Must run **before** anything in the same command buffer that samples
    /// them — the queued blurs, and every draw in the frame.
    #[must_use = "the images the barriers name must outlive the submit"]
    pub(super) fn record_pending_sampleable(&mut self, cbuf: vk::CommandBuffer) -> Vec<VkTexture> {
        let queued = std::mem::take(&mut self.pending_sampleable);
        for tex in &queued {
            unsafe {
                transition_image(
                    &self.gpu.device,
                    cbuf,
                    tex.image(),
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::AccessFlags::SHADER_READ,
                    vk::PipelineStageFlags::FRAGMENT_SHADER,
                );
            }
        }
        queued
    }

    /// Submit the queued barriers on their own — the consumer-side drain for anything that touches
    /// one of those images outside a frame. Same shape and same warning as
    /// [`Self::flush_pending_texture_uploads`].
    fn flush_pending_sampleable(&mut self) -> Result<(), VulkanError> {
        if self.pending_sampleable.is_empty() {
            return Ok(());
        }
        let queued = std::mem::take(&mut self.pending_sampleable);
        let gpu = self.gpu.clone();
        gpu.run_commands(
            self.command_pool,
            niri_vk::stats::SubmitSite::Transition,
            |cbuf| {
                for tex in &queued {
                    unsafe {
                        transition_image(
                            &gpu.device,
                            cbuf,
                            tex.image(),
                            vk::ImageLayout::UNDEFINED,
                            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                            vk::AccessFlags::SHADER_READ,
                            vk::PipelineStageFlags::FRAGMENT_SHADER,
                        );
                    }
                }
            },
        )
        .map_err(|e| VulkanError::Other(format!("flushing queued layout barriers: {e:#}")))
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
        // The chain samples `source` on a submit of its own, ahead of any frame.
        self.flush_pending_texture_uploads()?;
        self.flush_pending_blurs()?;
        self.flush_pending_sampleable()?;
        let (w, h) = source.extent();
        let output = self.create_buffer(Fourcc::Abgr8888, Size::from((w as i32, h as i32)))?;

        let gpu = self.gpu.clone();
        let pool = self.command_pool;
        let passes = (options.passes as usize).clamp(1, 31);
        let chain = BlurChain::new(&gpu, source.niri_texture(), passes)?;
        let recorded = gpu.run_commands(pool, niri_vk::stats::SubmitSite::Blur, |cbuf| {
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
pub(super) unsafe fn transition_image(
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

/// GPU-side timing for one submit: a two-slot timestamp query pool, written at
/// the top and bottom of each command buffer and read back right after the fence
/// wait the renderer already does.
///
/// Two slots is enough precisely *because* the renderer is synchronous — every
/// submit is fence-waited before the next command buffer is recorded, so the
/// results are always collected before the pool is reset. An asynchronous
/// renderer would need a pool per frame in flight.
///
/// Only built when `NIRI_FRAME_LOG` asked for `gpu` timing, so the query writes
/// are absent (not merely unread) in a normal session.
struct GpuTimer {
    pool: vk::QueryPool,
    /// Set when the device turns out not to write timestamps despite advertising
    /// them, after which the whole thing goes quiet. See
    /// [`VulkanRenderer::gpu_timer_collect`].
    unusable: Cell<bool>,
    /// Consecutive collections that came back entirely unwritten. Reset by any
    /// collection that carried a value, so only a device that never writes
    /// trips [`GpuTimer::UNWRITTEN_LIMIT`].
    unwritten_run: Cell<u32>,
    /// Whether this device has *ever* handed back a written pair. Once it has,
    /// [`GpuTimer::UNWRITTEN_LIMIT`] stops applying: the question that limit
    /// answers ("does this device implement timestamps at all?") has been
    /// answered, and no length of dry spell re-opens it.
    ever_written: Cell<bool>,
}

impl GpuTimer {
    /// The reading is bogus above this, so drop it rather than report it: a
    /// paravirtualized device (our Venus VM) can hand back a tick delta from an
    /// unrelated clock domain, and a "4200ms GPU pass" in the log is worse than
    /// no number at all.
    const SANE_LIMIT: Duration = Duration::from_secs(1);

    /// How many all-zero collections in a row it takes to call the device broken
    /// and go quiet — **before it has ever written one**. After that it no
    /// longer applies at all; see [`GpuTimer::ever_written`].
    ///
    /// Not one: a stack can implement timestamps and still *drop* them — the
    /// host-side fixes for our Venus VM are aiming at a partial hit rate, and
    /// with a limit of one, the first dropped pair would silence timing for the
    /// rest of the session. A device that implements nothing hits this within a
    /// few frames anyway.
    ///
    /// Measured on this VM 2026-07-25, and the reason it is not 16: Venus writes
    /// about 7% of pairs, in bursts. At 256×256 the dry spells ran to 15 — one
    /// short of the old limit — and at 1920×1080 the *first* 16 collections were
    /// all unwritten, so timing latched off before it ever produced a sample.
    /// The old value was measuring the dry spell, not the device.
    const UNWRITTEN_LIMIT: u32 = 256;

    /// The pool, if this device can answer timestamp queries at all.
    fn create(gpu: &Gpu) -> Result<Option<Self>, VulkanError> {
        if !gpu.timestamps_supported() {
            warn!("GPU timing requested, but this device cannot timestamp");
            return Ok(None);
        }

        let ci = vk::QueryPoolCreateInfo::default()
            .query_type(vk::QueryType::TIMESTAMP)
            .query_count(2);
        let pool = unsafe { gpu.device.create_query_pool(&ci, None) }?;
        Ok(Some(Self {
            pool,
            unusable: Cell::new(false),
            unwritten_run: Cell::new(0),
            ever_written: Cell::new(false),
        }))
    }

    /// The pool only if the session asked for GPU timing.
    fn if_requested(gpu: &Gpu) -> Result<Option<Self>, VulkanError> {
        if !crate::frame_log::gpu_timing() {
            return Ok(None);
        }
        Self::create(gpu)
    }
}

/// A submit the CPU walked away from, and everything it must outlive.
///
/// Freeing any of this while the GPU still reads it is a fault or silent corruption, not a panic
/// — which is why the synchronous renderer never needed the type. See
/// `docs/fork/renderer-synchronous-submits.md`.
struct InFlightSubmit {
    /// The queue-timeline value this submit signals. The timeline is what proves completion here,
    /// rather than the fence: a `SYNC_FD` export resets the fence, so once KMS has taken it the
    /// fence can no longer answer "are you done" — the timeline always can, with one device call
    /// covering every outstanding submit at once.
    timeline: u64,
    cbuf: vk::CommandBuffer,
    /// Shared with the sync point handed to KMS; the fence outlives whichever lets go last.
    _fence: VkSubmitFence,
    /// Every texture the command buffer samples, held for exactly the reason `VulkanFrame::held`
    /// holds them — the draw records reference the image and descriptor set, and the elements
    /// that own them are dropped long before the GPU is finished.
    _held: Vec<VkTexture>,
    /// What the command buffer *renders into*: the bound target, and the present-blit destination
    /// if there is one. Neither is in `_held` — that list is built from what draws sample — and
    /// neither belongs to the caller: the target may be a present-blit shadow owned by
    /// [`VulkanRenderer::present_blit_shadows`], which evicts least-recently-used, or an imported
    /// dmabuf owned by `dmabuf_target_cache`, which drops entries whose weak handle is gone. Both
    /// destroy the image on drop with no wait ([`VkTexture`]'s inner `Drop`), so without this the
    /// caches are free to delete an image a submit in flight is still writing.
    _targets: Vec<VkTexture>,
    /// The glyph-atlas staging buffer whose copy this command buffer carries, if any
    /// ([`VulkanRenderer::record_pending_glyph_uploads`]). Held for the same reason as everything
    /// else here — the copy reads it on the GPU long after the CPU has moved on. It frees itself
    /// when this record is dropped, so neither retirement path has to know it exists.
    _glyph_staging: Option<niri_vk::texture::GlyphStaging>,
    /// The texture-upload staging buffers whose copies this command buffer carries
    /// ([`VulkanRenderer::record_pending_texture_uploads`]). Held for the same reason and freed
    /// the same way as `_glyph_staging`.
    _texture_staging: Vec<niri_vk::texture::StagedTexture>,
    /// The blur chains this command buffer recorded. The widest of these: a chain owns the render
    /// pass, pipelines and descriptor sets the recording *binds*, so letting it go early does not
    /// corrupt the blur — it invalidates the command buffer, and every draw in it. See
    /// [`SharedBlurChain`].
    _blur_chains: Vec<Arc<SharedBlurChain>>,
}

impl VulkanRenderer {
    /// Free everything belonging to submits the GPU has finished. Polls — it must never block, or
    /// the wait we removed from the end of one frame simply reappears at the start of the next.
    pub(super) fn retire_completed(&mut self) {
        if self.in_flight.is_empty() {
            return;
        }
        let Some(completed) = self.gpu.submit_order_value() else {
            return;
        };
        // Submitted in order and signalled in order, so this is a prefix.
        let done = self
            .in_flight
            .iter()
            .take_while(|f| f.timeline <= completed)
            .count();
        for frame in self.in_flight.drain(..done) {
            unsafe {
                self.gpu
                    .device
                    .free_command_buffers(self.command_pool, std::slice::from_ref(&frame.cbuf));
            }
        }
    }

    /// Wait out every in-flight submit and free it. For teardown, and for the paths that need the
    /// GPU quiet before they touch shared state.
    pub(super) fn drain_in_flight(&mut self) {
        if self.in_flight.is_empty() {
            return;
        }
        let _timed = niri_vk::stats::retire(niri_vk::stats::SubmitSite::KmsFrame);
        unsafe { self.gpu.device.device_wait_idle() }.ok();
        self.retire_completed();
        // A device that cannot report its timeline leaves the records unretired above; the wait
        // above already proved them complete, so free them here rather than leak.
        for frame in std::mem::take(&mut self.in_flight) {
            unsafe {
                self.gpu
                    .device
                    .free_command_buffers(self.command_pool, std::slice::from_ref(&frame.cbuf));
            }
        }
    }

    /// Record a submit the CPU is not going to wait for. `targets` is what the command buffer
    /// renders into — see [`InFlightSubmit::_targets`], which is why it is separate from `held`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn add_in_flight(
        &mut self,
        timeline: u64,
        cbuf: vk::CommandBuffer,
        fence: VkSubmitFence,
        held: Vec<VkTexture>,
        targets: Vec<VkTexture>,
        glyph_staging: Option<niri_vk::texture::GlyphStaging>,
        texture_staging: Vec<niri_vk::texture::StagedTexture>,
        blur_chains: Vec<Arc<SharedBlurChain>>,
    ) {
        self.in_flight.push(InFlightSubmit {
            timeline,
            cbuf,
            _fence: fence,
            _held: held,
            _targets: targets,
            _glyph_staging: glyph_staging,
            _texture_staging: texture_staging,
            _blur_chains: blur_chains,
        });
    }

    /// Whether the frame being finished is the one going to KMS — the tty backend's bracket, read
    /// for what it says rather than for the permission it grants. Used to label the submit
    /// ([`VulkanFrame::submit_site`]); unlike `should_defer_finish` it does not care whether the
    /// session opted into deferring, because a frame is the KMS frame either way.
    pub(super) fn finish_is_for_kms(&self) -> bool {
        self.finish_may_defer
    }

    /// Tell the renderer whether the frame it is about to finish is the one going to KMS. The tty
    /// backend brackets `DrmCompositor::render_frame` with this; everything else renders with it
    /// false and keeps the synchronous finish.
    pub fn set_finish_may_defer(&mut self, may: bool) {
        self.finish_may_defer = may;
    }

    /// Whether this frame's finish should hand its fence onward instead of waiting for it.
    ///
    /// Every condition here is load-bearing:
    /// - the caller must have somewhere to put the fence (`finish_may_defer`);
    /// - the device must order submits, or work issued next could execute alongside this one and
    ///   race it on the images the renderer reuses across frames (`Gpu::submit`);
    /// - GPU timing reuses a single query pool per frame, whose reset would race an in-flight
    ///   frame's queries;
    /// - and the session must have asked, until this has run on a real seat.
    pub(super) fn should_defer_finish(&self) -> bool {
        self.finish_may_defer
            && self.defer_scanout
            && self.gpu.orders_submits()
            && self.gpu_timer.is_none()
    }

    /// Whether a finish that targets an **offscreen** should hand its completion to the in-flight
    /// list instead of parking on it.
    ///
    /// Deliberately not `should_defer_finish`: that one requires `finish_may_defer`, the tty
    /// backend's bracket around `DrmCompositor::render_frame`, and an offscreen finish never runs
    /// inside it — widget bakes, window snapshots and effect buffers are all built while elements
    /// are being collected, before the backend asks for a frame. So the offscreen path would never
    /// defer at all, which is how it came to be the largest block of blocked time in the frame log.
    ///
    /// It also needs no *exportable* fence: nothing outside this process ever takes an offscreen's
    /// completion, so a plain fence in the record is enough. What is left is the two device
    /// requirements that make walking away sound at all — a total order on submits, so nothing
    /// issued afterwards can execute alongside this one, and no per-frame query pool whose reset
    /// would race an in-flight frame.
    pub(super) fn should_defer_offscreen_finish(&self) -> bool {
        self.defer_scanout && self.gpu.orders_submits() && self.gpu_timer.is_none()
    }

    /// Override the session's opt-in for this renderer alone. Tests only: headless there is no KMS
    /// plane to take the fence, so the deferred path has to be asked for explicitly to be covered
    /// at all.
    #[cfg(test)]
    pub(crate) fn set_defer_scanout(&mut self, on: bool) {
        self.defer_scanout = on;
    }

    /// How many submits the CPU has walked away from and not yet retired.
    #[cfg(test)]
    pub(crate) fn in_flight_len(&self) -> usize {
        self.in_flight.len()
    }

    /// How many render targets the in-flight records are keeping alive. The count *is* the
    /// assertion: a missing keep-alive is invisible to every pixel — the image survives whenever
    /// its cache happens not to evict, and corrupts silently when it does.
    #[cfg(test)]
    pub(super) fn in_flight_targets_len(&self) -> usize {
        self.in_flight.iter().map(|f| f._targets.len()).sum()
    }

    /// How many sampled textures the in-flight records are keeping alive — the frame's `held`,
    /// which carries the images its command buffer recorded barriers and draws against. Same
    /// reasoning as [`Self::in_flight_targets_len`]: a missing keep-alive shows up as a destroyed
    /// image inside a recorded command buffer, which no pixel comparison can see.
    #[cfg(test)]
    pub(super) fn in_flight_held_len(&self) -> usize {
        self.in_flight.iter().map(|f| f._held.len()).sum()
    }

    /// Turn GPU timing on for this renderer alone, without touching the
    /// process-wide flag the environment sets — tests run in one process and
    /// share that flag, so flipping it would instrument every other renderer too.
    /// Returns whether the device could provide it.
    #[cfg(test)]
    pub(crate) fn enable_gpu_timing(&mut self) -> bool {
        if self.gpu_timer.is_none() {
            self.gpu_timer = GpuTimer::create(&self.gpu).expect("query pool");
        }
        self.gpu_timer.is_some()
    }

    /// Reset the query pool and stamp the start of `cbuf`. Must be called with
    /// `cbuf` recording and **outside** a render pass (`vkCmdResetQueryPool` is
    /// not allowed inside one).
    pub(super) fn gpu_timer_begin(&self, cbuf: vk::CommandBuffer) {
        let Some(timer) = self.gpu_timer.as_ref().filter(|t| !t.unusable.get()) else {
            return;
        };
        unsafe {
            let dev = &self.gpu.device;
            dev.cmd_reset_query_pool(cbuf, timer.pool, 0, 2);
            dev.cmd_write_timestamp(cbuf, vk::PipelineStageFlags::TOP_OF_PIPE, timer.pool, 0);
        }
    }

    /// Stamp the end of `cbuf`, just before it is ended and submitted.
    pub(super) fn gpu_timer_end(&self, cbuf: vk::CommandBuffer) {
        let Some(timer) = self.gpu_timer.as_ref().filter(|t| !t.unusable.get()) else {
            return;
        };
        unsafe {
            self.gpu.device.cmd_write_timestamp(
                cbuf,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                timer.pool,
                1,
            );
        }
    }

    /// Read the pair back and report it to the frame log. Call after the submit's
    /// fence wait, so the results are available without blocking further.
    pub(super) fn gpu_timer_collect(&self) {
        let Some(timer) = self.gpu_timer.as_ref().filter(|t| !t.unusable.get()) else {
            return;
        };

        let mut ticks = [0u64; 2];
        let res = unsafe {
            self.gpu.device.get_query_pool_results(
                timer.pool,
                0,
                &mut ticks,
                vk::QueryResultFlags::TYPE_64 | vk::QueryResultFlags::WAIT,
            )
        };
        if let Err(err) = res {
            warn!("error reading GPU timestamps: {err}");
            return;
        }

        match timestamp_ticks(ticks, self.gpu.timestamp_valid_bits) {
            TimestampSample::Delta(delta) => {
                timer.unwritten_run.set(0);
                timer.ever_written.set(true);
                let duration = self.gpu.timestamp_delta(delta);
                // Above the sane limit the pair is from some other clock domain,
                // so it is a lost sample too, not a very slow pass.
                if duration <= GpuTimer::SANE_LIMIT {
                    crate::frame_log::add_gpu_time(duration);
                } else {
                    crate::frame_log::add_gpu_lost();
                }
            }
            TimestampSample::Lost => {
                // Something was written, so the device does implement the query;
                // this particular pair just isn't a pass we can report.
                timer.unwritten_run.set(0);
                timer.ever_written.set(true);
                crate::frame_log::add_gpu_lost();
            }
            TimestampSample::NotWritten => {
                crate::frame_log::add_gpu_lost();
                let run = timer.unwritten_run.get() + 1;
                timer.unwritten_run.set(run);
                // A device that has written before is not broken, however long
                // this dry spell gets — Venus writes ~7% of pairs, in bursts.
                if run >= GpuTimer::UNWRITTEN_LIMIT && !timer.ever_written.get() {
                    warn!(
                        "this device advertises timestamp queries but wrote none in \
                         {run} collections; GPU timing is unavailable (CPU-side frame \
                         timing is unaffected)"
                    );
                    timer.unusable.set(true);
                }
            }
        }
    }
}

/// What one collection of a start/end timestamp pair yielded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TimestampSample {
    /// A usable tick delta.
    Delta(u64),
    /// The device wrote something, but not a pass we can report: one end
    /// missing, or a zero-tick delta. Discard the sample; the device itself is
    /// fine.
    Lost,
    /// Neither end was written. Once in a while this is a dropped pair; forever
    /// means the device does not implement the query at all.
    NotWritten,
}

/// Turn a start/end timestamp pair into a tick delta.
///
/// Only the low `valid_bits` of a timestamp are defined, so the pair is masked
/// before subtracting; a counter that wrapped inside the pass then still yields
/// the right delta, since the subtraction is modulo the same width.
///
/// A zero at *either* end is a value the device did not write, not a clock that
/// read zero: the GPU clock is free-running, so both ends of a real pass have a
/// large absolute value, and a genuine zero has probability 2^-`valid_bits`.
/// Taking a half-written pair at face value is how a lost sample turns into a
/// bogus duration — either the raw end value, or a near-wrap delta. Likewise a
/// delta of exactly zero: no real pass takes no ticks.
///
/// Devices that advertise the feature and implement nothing (virtio-gpu/Venus,
/// at least here) are the reason any of this is checked rather than assumed.
pub(super) fn timestamp_ticks(ticks: [u64; 2], valid_bits: u32) -> TimestampSample {
    let mask = if valid_bits >= 64 {
        u64::MAX
    } else {
        (1u64 << valid_bits) - 1
    };
    let [start, end] = [ticks[0] & mask, ticks[1] & mask];

    if start == 0 && end == 0 {
        return TimestampSample::NotWritten;
    }
    if start == 0 || end == 0 {
        return TimestampSample::Lost;
    }

    match end.wrapping_sub(start) & mask {
        0 => TimestampSample::Lost,
        delta => TimestampSample::Delta(delta),
    }
}

/// Whether the session asked for the scanout submit to be left in flight, via
/// `NIRI_VK_ASYNC_SCANOUT=1`. Read once — it decides how frames are built, so it must answer the
/// same way for the whole process.
///
/// Opt-in because the win it targets can only be confirmed on a real seat: headless there is no
/// KMS plane to take the fence, so nothing here exercises the part that pays off. See
/// `docs/fork/renderer-synchronous-submits.md`.
fn deferred_scanout_requested() -> bool {
    static REQUESTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *REQUESTED.get_or_init(|| {
        std::env::var("NIRI_VK_ASYNC_SCANOUT")
            .is_ok_and(|v| matches!(v.trim(), "1" | "on" | "true"))
    })
}

impl Drop for VulkanRenderer {
    fn drop(&mut self) {
        // Before anything is destroyed: an in-flight submit still references its command buffer,
        // and the pool it came from is about to go.
        self.drain_in_flight();
        unsafe {
            let dev = &self.gpu.device;
            let _ = dev.device_wait_idle();
            if let Some(timer) = self.gpu_timer.take() {
                dev.destroy_query_pool(timer.pool, None);
            }
            self.readback_staging_buffer.destroy(dev);
            self.solid_pipeline.destroy(dev);
            self.sdf_rect_pipeline.destroy(dev);
            self.texture_pipeline.destroy(dev);
            self.rounded_texture_pipeline.destroy(dev);
            self.clipped_texture_pipeline.destroy(dev);
            self.gradient_fade_pipeline.destroy(dev);
            self.border_pipeline.destroy(dev);
            self.shadow_pipeline.destroy(dev);
            self.postprocess_pipeline.destroy(dev);
            self.resize_pipeline.destroy(dev);
            self.text_pipeline.destroy(dev);
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
        // Rendering into it makes any queued make-sampleable barrier both unnecessary and wrong:
        // the render pass discards from UNDEFINED and leaves the image sampleable itself, while a
        // barrier still queued would be recorded *after* that, discarding what was just drawn.
        // See `make_sampleable`.
        let image = target.image();
        self.pending_sampleable.retain(|tex| tex.image() != image);
        Ok(VkFramebuffer::new_offscreen(target.clone()))
    }
}

impl Bind<Dmabuf> for VulkanRenderer {
    /// Bind a (GBM-allocated) dmabuf as a render target — the KMS-scanout path (Stage 3). Imports
    /// the dmabuf's memory as a `VkImage`; a frame then renders into it (directly for RGBA-order
    /// buffers, or via a shadow + present-blit for `Argb8888`/`Xrgb8888` planes) so a display
    /// controller can scan it out. The import is cached per buffer (`dmabuf_target_cache`) — a
    /// fresh import per bind exhausts the host blob pool on Venus.
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
                    // We draw into it with the blending pipelines, and read it back either by copy
                    // (same byte order) or by blit (`copy_framebuffer` converting to the other
                    // order).
                    const USAGE: vk::ImageUsageFlags = vk::ImageUsageFlags::from_raw(
                        vk::ImageUsageFlags::COLOR_ATTACHMENT.as_raw()
                            | vk::ImageUsageFlags::TRANSFER_SRC.as_raw(),
                    );
                    self.check_modifier(
                        fourcc,
                        IMAGE_VK_FORMAT,
                        modifier,
                        vk::FormatFeatureFlags::COLOR_ATTACHMENT
                            | vk::FormatFeatureFlags::COLOR_ATTACHMENT_BLEND
                            | vk::FormatFeatureFlags::TRANSFER_SRC
                            | vk::FormatFeatureFlags::BLIT_SRC,
                        "scanout target",
                    )?;
                    let tex = NiriTexture::import_dmabuf_render_target(
                        &self.gpu,
                        w,
                        h,
                        fd,
                        offset,
                        stride,
                        modifier,
                        IMAGE_VK_FORMAT,
                        USAGE,
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
                // TRANSFER_DST for the blit; TRANSFER_SRC so the scanout buffer can be read back
                // (ExportMem / the scanout test). It is never a render-pass attachment — the shadow
                // is — so it needs no COLOR_ATTACHMENT.
                const USAGE: vk::ImageUsageFlags = vk::ImageUsageFlags::from_raw(
                    vk::ImageUsageFlags::TRANSFER_DST.as_raw()
                        | vk::ImageUsageFlags::TRANSFER_SRC.as_raw(),
                );
                // BLIT_DST is the bit this whole check exists for: the present blit is a
                // `vkCmdBlitImage` into a modifier-tiled image, and only the modifier's features
                // say whether that is defined. Readback of the scanout buffer needs the other two.
                self.check_modifier(
                    fourcc,
                    present_format,
                    modifier,
                    vk::FormatFeatureFlags::BLIT_DST
                        | vk::FormatFeatureFlags::TRANSFER_SRC
                        | vk::FormatFeatureFlags::BLIT_SRC,
                    "present-blit target",
                )?;
                let present_tex = NiriTexture::import_dmabuf_render_target(
                    &self.gpu,
                    w,
                    h,
                    fd,
                    offset,
                    stride,
                    modifier,
                    present_format,
                    USAGE,
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

    /// Refuse to import a dmabuf whose DRM modifier cannot support the commands we will record
    /// against it.
    ///
    /// An image with `DRM_FORMAT_MODIFIER_EXT` tiling takes its format features from the modifier,
    /// and *none* of them are mandated by the spec — so the `BLIT_DST` that `R8G8B8A8_UNORM` is
    /// guaranteed at `OPTIMAL` tiling is only ever a promise the driver chose to make about a
    /// modifier. `required` therefore has to come from the commands (a blit needs `BLIT_DST`, a
    /// copy needs the unrelated `TRANSFER_DST`), never from the image's usage flags, which gate
    /// creation and nothing else.
    ///
    /// Failing here means no picture, which is the point: the alternative to a legible error at
    /// import is undefined pixels, or a device loss several frames later with no way back to the
    /// cause. A modifier the driver never enumerated is the one soft case — see
    /// [`ModifierSupport::Unlisted`].
    fn check_modifier(
        &mut self,
        fourcc: Fourcc,
        format: vk::Format,
        modifier: u64,
        required: vk::FormatFeatureFlags,
        role: &str,
    ) -> Result<(), VulkanError> {
        let support = self
            .gpu
            .check_modifier_features(format, modifier, required)
            .map_err(|e| VulkanError::Other(format!("{role} ({fourcc:?}): {e:#}")))?;

        if support == ModifierSupport::Unlisted && self.warned_modifiers.insert((format, modifier))
        {
            warn!(
                "{role}: this driver enumerates no DRM modifiers for {fourcc:?}, so we cannot \
                 confirm that modifier {modifier:#018x} supports {required:?}; importing anyway, \
                 but if the image comes out wrong this is the first thing to suspect"
            );
        }
        Ok(())
    }

    /// Record every queued client-dmabuf re-acquire barrier into `cbuf` (a frame's command buffer),
    /// clearing the queue. Called by [`super::VulkanFrame::begin`] before the render pass begins
    /// (barriers must be recorded outside a render pass). Each barrier rides the frame's single
    /// submit, so the acquire is no longer a standalone submit + fence-wait. See
    /// [`Self::pending_dmabuf_acquires`].
    /// **Hands the drained textures back rather than dropping them**, for the caller to keep alive
    /// until this command buffer's submit has retired — the same contract, for the same reason, as
    /// [`Self::record_pending_texture_uploads`].
    ///
    /// Dropping them here destroys the `VkImage` a barrier was just recorded against, while the
    /// command buffer is still recording and unsubmitted. Vulkan invalidates a command buffer whose
    /// bound objects are destroyed, so every later command on it — the whole frame — is invalid
    /// usage, and the submit that follows is a submit of a poisoned buffer.
    ///
    /// It took a live session and the validation layer to see it, because the drop is only the
    /// *last* reference when the client has already released the buffer: `cached_dmabuf_import`
    /// evicts dead entries on every lookup, so a client cycling buffers (any GPU client that
    /// reallocates, e.g. on resize) leaves this queue holding the only reference. While only cache
    /// *hits* were queued the bug was unreachable — a hit means the cache still holds one.
    #[must_use = "the recorded-against images must outlive the submit"]
    pub(super) fn record_pending_dmabuf_acquires(
        &mut self,
        cbuf: vk::CommandBuffer,
    ) -> Vec<VkTexture> {
        let queued = std::mem::take(&mut self.pending_dmabuf_acquires);
        for tex in &queued {
            tex.record_reacquire_dmabuf(cbuf);
        }
        queued
    }

    /// Queue a blur of `source` into `output` for the next frame to record.
    ///
    /// A blur already queued for the same `output` is **replaced**: the second one exists because
    /// the source was re-rendered, which makes the first one's result stale before it was ever
    /// recorded. Same rule, and same reason, as [`Self::queue_texture_upload`].
    pub(super) fn queue_blur(
        &mut self,
        chain: Arc<SharedBlurChain>,
        source: VkTexture,
        output: VkTexture,
        offset: f32,
    ) {
        let image = output.image();
        let entry = PendingBlur {
            chain,
            source,
            output,
            offset,
        };
        match self
            .pending_blurs
            .iter_mut()
            .find(|queued| queued.output.image() == image)
        {
            Some(superseded) => *superseded = entry,
            None => self.pending_blurs.push(entry),
        }
    }

    /// Record every queued blur into `cbuf` and hand back what must outlive its submit: the chains
    /// (whose render pass and pipelines the recording binds) and the images they read and write.
    ///
    /// Must be called outside a render pass — the chain begins its own.
    #[must_use = "the chains and images the passes name must outlive the submit"]
    pub(super) fn record_pending_blurs(
        &mut self,
        cbuf: vk::CommandBuffer,
    ) -> (Vec<Arc<SharedBlurChain>>, Vec<VkTexture>) {
        let queued = std::mem::take(&mut self.pending_blurs);
        let mut chains = Vec::with_capacity(queued.len());
        let mut textures = Vec::with_capacity(queued.len() * 2);
        for blur in queued {
            let (w, h) = blur.output.extent();
            blur.chain
                .record_into(cbuf, blur.offset, blur.output.image(), w, h);
            chains.push(blur.chain);
            textures.push(blur.source);
            textures.push(blur.output);
        }
        (chains, textures)
    }

    /// Record and submit the queued blurs on their own, blocking until they land — the
    /// consumer-side drain for anything that samples a blurred output **outside** a frame.
    ///
    /// Same shape and same warning as [`Self::flush_pending_texture_uploads`]: on the render path
    /// this is empty, because `begin` drained it; every caller of this is a stall being added
    /// somewhere else.
    fn flush_pending_blurs(&mut self) -> Result<(), VulkanError> {
        if self.pending_blurs.is_empty() {
            return Ok(());
        }
        // The chains sample images whose barrier may still be queued.
        self.flush_pending_sampleable()?;
        let queued = std::mem::take(&mut self.pending_blurs);
        let gpu = self.gpu.clone();
        // `run_commands` waits, so `queued` — the chains and the images it holds — outlives the
        // passes it carries.
        gpu.run_commands(
            self.command_pool,
            niri_vk::stats::SubmitSite::Blur,
            |cbuf| {
                for blur in &queued {
                    let (w, h) = blur.output.extent();
                    blur.chain
                        .record_into(cbuf, blur.offset, blur.output.image(), w, h);
                }
            },
        )
        .map_err(|e| VulkanError::Other(format!("flushing queued blurs: {e:#}")))
    }

    /// Queue `staged`'s copy into `tex`'s image for the next frame to record, holding `tex` alive
    /// until then and past the submit ([`PendingTextureUpload`]).
    ///
    /// An upload already queued for the *same image* is **replaced**, not appended to. Every entry
    /// in this queue covers its image's full extent (both producers, `stage_32bpp` and
    /// `reupload_32bpp`, write `w*h*4` bytes), so an earlier copy to the same image is dead the
    /// moment a later one is queued: recording it would only write pixels the next command
    /// overwrites, and its staging is bytes we already paid to fill.
    ///
    /// That also bounds the queue. A frame that fails before it can drain leaves everything
    /// queued for the next one — correct for a transient failure, but the live wedge was a
    /// *permanent* one, where clients kept committing into a queue that would never drain again.
    /// Superseding caps it at one entry per live image instead of one per commit, so the failure
    /// stops feeding itself.
    fn queue_texture_upload(&mut self, tex: &VkTexture, staged: niri_vk::texture::StagedTexture) {
        let entry = PendingTextureUpload {
            tex: tex.clone(),
            staged,
        };
        let image = tex.image();
        match self
            .pending_texture_uploads
            .iter_mut()
            .find(|queued| queued.tex.image() == image)
        {
            Some(superseded) => *superseded = entry,
            None => self.pending_texture_uploads.push(entry),
        }
    }

    /// Record every staged texture upload into `cbuf` and hand back what must outlive that command
    /// buffer's submit: the staging buffers the copies read from, and the destination textures the
    /// copies name. Same slot, same contract and same reason as
    /// [`Self::record_pending_glyph_uploads`].
    ///
    /// Returns them rather than dropping them here precisely because dropping is what breaks it:
    /// the staging would be freed while the GPU still reads it, and the image destroyed while the
    /// command buffer that names it is still recording.
    #[must_use = "the staging and the recorded-into images must outlive the submit"]
    pub(super) fn record_pending_texture_uploads(
        &mut self,
        cbuf: vk::CommandBuffer,
    ) -> (Vec<niri_vk::texture::StagedTexture>, Vec<VkTexture>) {
        let queued = std::mem::take(&mut self.pending_texture_uploads);
        let mut staging = Vec::with_capacity(queued.len());
        let mut textures = Vec::with_capacity(queued.len());
        for upload in queued {
            upload.staged.record(cbuf);
            staging.push(upload.staged);
            textures.push(upload.tex);
        }
        (staging, textures)
    }

    /// Record and submit the queued texture copies on their own, blocking until they land.
    ///
    /// **Every path that submits work outside a [`super::VulkanFrame`] must call this first.** A
    /// frame is not the only thing that touches an imported image: a readback reads it, a blur
    /// samples it, `make_sampleable` transitions it, and an shm re-upload overwrites it. Each of
    /// those submits on its own, ahead of the next `begin()`, so without this they see the image
    /// as it was before its copy — blank, or (worse, and how this was found) they get overwritten
    /// *afterwards* when the frame finally records the stale copy. The shm test caught exactly
    /// that: a re-upload to green came out red.
    ///
    /// Cheap where it matters: on the render path the queue is empty by the time any of these run,
    /// because `begin` drained it. When it is not empty this is precisely the old per-upload
    /// behaviour, except still batched into one submit.
    fn flush_pending_texture_uploads(&mut self) -> Result<(), VulkanError> {
        if self.pending_texture_uploads.is_empty() {
            return Ok(());
        }
        let queued = std::mem::take(&mut self.pending_texture_uploads);
        let gpu = self.gpu.clone();
        // `run_commands` waits, so `queued` — the staging buffers it frees on drop, and the
        // destination images it holds references to — outlives the copies it carries.
        gpu.run_commands(
            self.command_pool,
            niri_vk::stats::SubmitSite::Upload,
            |cbuf| {
                for upload in &queued {
                    upload.staged.record(cbuf);
                }
            },
        )
        .map_err(|e| VulkanError::Other(format!("flushing staged texture uploads: {e:#}")))
    }

    /// Count of texture uploads awaiting a deferred copy (test-only: asserts a frame drained
    /// them). See [`Self::pending_texture_uploads`].
    #[cfg(test)]
    pub(super) fn pending_texture_uploads_len(&self) -> usize {
        self.pending_texture_uploads.len()
    }

    /// Count of blurs awaiting a frame's command buffer (test-only: asserts that preparing one
    /// submitted nothing, and that a frame drained it). See [`Self::pending_blurs`].
    #[cfg(test)]
    pub(crate) fn pending_blurs_len(&self) -> usize {
        self.pending_blurs.len()
    }

    /// Staging buffers the pool owns (test-only): the count that must not grow with the number of
    /// client commits. See [`niri_vk::staging::StagingPool`].
    #[cfg(test)]
    pub(super) fn staging_chunk_count(&self) -> usize {
        self.staging_pool.chunk_count()
    }

    /// Force the staging pool's first chunk into existence (test-only).
    ///
    /// That chunk is a created resource like any other and `niri_vk::stats::take_creates` counts
    /// it, but it is allocated once per renderer and reused for the rest of the session — so any
    /// test measuring "how many resources did this upload cost" wants it out of the window rather
    /// than folded into the first measurement. Touches no cache: it stages four bytes and drops
    /// them, leaving only the chunk behind.
    #[cfg(test)]
    pub(crate) fn warm_staging_pool(&mut self) {
        let _ = self.staging_pool.stage(&self.gpu, &[0u8; 4]);
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
        // `Texture::new_color_target` below counts the image it creates; counting here as well
        // would report two resources for one offscreen.
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
    /// existing image — no image allocation, and no submit of its own.
    pub(super) fn reupload_shm(&mut self, tex: &VkTexture, data: &[u8]) -> Result<(), VulkanError> {
        // The copy covers the image's full w*h extent, so short data would be an out-of-bounds
        // staging read. The sole caller only reaches here on a size-matched cache hit with a 32bpp
        // shm buffer; this pins that contract rather than guarding at runtime. (`reupload_32bpp`
        // checks it again and errors, but a debug build should name the caller.)
        debug_assert_eq!(
            data.len(),
            (tex.size().w as usize) * (tex.size().h as usize) * 4,
            "reupload_shm data must be the texture's w*h*4 (32bpp) extent",
        );
        // Queued, not submitted: the copy rides the next frame's command buffer alongside the
        // imports and the dmabuf acquires. A live seat frame showed `11 shm in 19.38ms` moving
        // 4.5 MiB — 0.33 ms of bytes behind eleven round trips — and forced the queued imports to
        // flush on top of it (`1 upload in 1.89ms` beside it in the same line).
        //
        // Ordering against a staged copy already queued for this same texture is what the flush
        // used to buy, and the queue gives it for free: a full-extent copy queued later replaces
        // the earlier one outright (`queue_texture_upload`), which is what "re-upload" means. A
        // client committing several times between two frames therefore uploads once.
        let staged = tex.stage_reupload_shm(&mut self.staging_pool, data)?;
        self.queue_texture_upload(tex, staged);
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
        // Staged, not uploaded: the copy is recorded into the next frame's own command buffer
        // (`pending_texture_uploads`) instead of costing a submit and a blocking fence wait here.
        let (tex, staged) = NiriTexture::stage_32bpp(
            &self.gpu,
            &mut self.staging_pool,
            w,
            h,
            &data[..expected],
            vk_format,
            alpha_one,
            filter,
        )?;
        // `NiriTexture` has no `Drop`, so free it if the descriptor set fails (matches the batch
        // path's cleanup on the same failure). `staged` drops with it, unqueued: its pixels never
        // reach an image, which is exactly what a failed import means.
        let (desc_pool, set) = match self.make_texture_set(&tex) {
            Ok(v) => v,
            Err(err) => {
                tex.destroy(&self.gpu);
                return Err(err);
            }
        };
        let tex = VkTexture::new(self.gpu.clone(), tex, desc_pool, set, w, h, format, flipped);
        // Queued only now that there is a refcounted texture to hold: the queue keeps the image
        // alive until the copy has been submitted, and the caller dropping its handle before the
        // next frame must not destroy the image out from under the recording.
        self.queue_texture_upload(&tex, staged);
        Ok(tex)
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
/// `push_size` is the pipeline's push-constant range size.
///
/// # Alpha convention
///
/// The renderer is **premultiplied end to end**: every framebuffer (output and offscreen bake)
/// holds premultiplied alpha, every sampled texture is premultiplied (Wayland client buffers, the
/// icon decoder, every `widget::bake`), every push-constant color is premultiplied, every fragment
/// stage outputs premultiplied color, and so every pipeline blends premultiplied-over. There is
/// deliberately no per-material knob: a material that blended `SRC_ALPHA` against an already-
/// premultiplied source would multiply by alpha twice, which is invisible for opaque or black
/// content and darkens everything else (it is exactly the bug this convention replaced).
///
/// Straight-alpha colors still exist *above* this layer — the toolkit's `style::Rgba` mirrors the
/// GNOME SCSS and is straight — but they are premultiplied at the frame-method boundary
/// (`render_rounded_rect_impl`, `render_glyphs_with`, `Painter::clear`), never here.
fn build_pipeline(
    gpu: &Gpu,
    render_pass: vk::RenderPass,
    vert_spv: &[u8],
    frag_spv: &[u8],
    set_layouts: &[vk::DescriptorSetLayout],
    push_size: u32,
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

    // Premultiplied-over compositing, for every material without exception — see the alpha
    // convention on `build_pipeline`. This matches the GLES oracle, which set
    // `BlendFunc(ONE, ONE_MINUS_SRC_ALPHA)` for every draw.
    let blend_attachment = vk::PipelineColorBlendAttachmentState::default()
        .blend_enable(true)
        .src_color_blend_factor(vk::BlendFactor::ONE)
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
