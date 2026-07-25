//! Cached blur resources for the owned Vulkan renderer's xray effect-buffer path — the blur half of
//! [`EffectBuffer`](crate::render_helpers::effect_buffer::EffectBuffer)'s Vulkan arm.
//!
//! A focused sibling of [`BackdropBlur`](super::backdrop_blur::BackdropBlur): it runs the same
//! dual-Kawase [`BlurChain`] but over an **externally-owned** offscreen [`VkTexture`] (the effect
//! buffer's offscreen) rather than owning a capture of the scene. It is held *inside* the effect
//! buffer's Vulkan offscreen so it is rebuilt **atomically** with that texture — [`BlurChain::new`]
//! binds the source's image view at construction and the chain has no `Drop`, so a chain left over
//! a recreated offscreen would sample a dangling descriptor. Per-frame `VkTexture` allocation is
//! the virtio-gpu blob churn that aborts Venus live, so `output` is created once and the chain is
//! reused across frames (the source view is stable; only its contents change, which [`Self::run`]
//! re-blurs).

use std::sync::Arc;

use ash::vk;
use niri_vk::blur::BlurChain;
use niri_vk::gpu::Gpu;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::{Offscreen, Texture as _};

use super::error::VulkanError;
use super::renderer::VulkanRenderer;
use super::types::VkTexture;

pub(crate) struct EffectBlur {
    /// Kept so [`Drop`] can free the (Drop-less) [`BlurChain`]; an `Arc` clone keeps the device
    /// alive regardless of renderer/cache drop order.
    gpu: Arc<Gpu>,
    /// The renderer's command pool. Dereferenced only by [`Self::run`], which the effect buffer
    /// calls exclusively from `prepare_blur_vulkan` — immediately after the offscreen's context-id
    /// recreation check, with the renderer borrowed — so the pool outlives every use even though
    /// an `EffectBuffer` in `OutputState` can outlive a `VulkanRenderer` (device reset).
    /// Assumes a single renderer owns this cache (true today), as
    /// [`BackdropBlur`](super::backdrop_blur) does.
    pool: vk::CommandPool,
    passes: usize,
    /// Samples the source offscreen (its descriptor set was bound to the source's stable view at
    /// [`BlurChain::new`]); the owner rebuilds this whole `EffectBlur` when the source is
    /// recreated.
    chain: BlurChain,
    /// The blurred result, copied out of the chain's level 0 by [`Self::run`].
    output: VkTexture,
    /// Whether `output` currently holds the blur of the source's latest contents. Reset (via
    /// [`Self::invalidate`]) when the source is re-rendered in place (same view, new pixels) so
    /// the next prepare re-runs the blur.
    valid: bool,
}

impl EffectBlur {
    /// Build the chain (bound to `source`'s view) plus a source-sized blurred-output buffer.
    /// `output` starts invalid — [`Self::run`] must fill it before it is sampled.
    pub(crate) fn new(
        renderer: &mut VulkanRenderer,
        source: &VkTexture,
        passes: usize,
    ) -> Result<Self, VulkanError> {
        let chain = BlurChain::new(&renderer.gpu, source.niri_texture(), passes)?;
        let output = renderer.create_buffer(Fourcc::Abgr8888, source.size())?;
        Ok(Self {
            gpu: renderer.gpu.clone(),
            pool: renderer.command_pool,
            passes,
            chain,
            output,
            valid: false,
        })
    }

    pub(crate) fn passes(&self) -> usize {
        self.passes
    }

    pub(crate) fn is_valid(&self) -> bool {
        self.valid
    }

    pub(crate) fn invalidate(&mut self) {
        self.valid = false;
    }

    pub(crate) fn output(&self) -> &VkTexture {
        &self.output
    }

    /// Run the dual-Kawase chain over the source's current contents into `output`, on its own
    /// fence-waited submission (like [`BackdropBlur::run_blur`](super::backdrop_blur)). Safe mid
    /// element-build: the offscreen render already flushed and [`make_offscreen_sampleable`] left
    /// the source `SHADER_READ_ONLY`, and this renderer is synchronous (each submit fence-waits),
    /// so the source is fully written before the chain samples it.
    ///
    /// [`make_offscreen_sampleable`]: crate::render_helpers::renderer::OffscreenRenderer::make_offscreen_sampleable
    pub(crate) fn run(&mut self, offset: f32) -> Result<(), VulkanError> {
        let (w, h) = self.output.extent();
        let gpu = &self.gpu;
        gpu.run_commands(self.pool, niri_vk::stats::SubmitSite::Blur, |cbuf| {
            self.chain.record(gpu, cbuf, offset);
            self.chain
                .copy_output_to(gpu, cbuf, self.output.image(), w, h);
        })?;
        // copy_output_to barriers `output` to SHADER_READ_ONLY; record the tracked layout so a
        // later sample/readback knows it.
        self.output
            .set_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        self.valid = true;
        Ok(())
    }
}

impl Drop for EffectBlur {
    fn drop(&mut self) {
        // BlurChain has no Drop (teardown needs a &Gpu); the VkTexture output frees itself.
        self.chain.destroy(&self.gpu);
    }
}

impl std::fmt::Debug for EffectBlur {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // BlurChain/Gpu are opaque; surface only the observable state.
        f.debug_struct("EffectBlur")
            .field("passes", &self.passes)
            .field("valid", &self.valid)
            .finish_non_exhaustive()
    }
}
