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

    /// Run the dual-Kawase chain over `source`'s current contents into `output`, on its own
    /// submission (like [`BackdropBlur::run_blur`](super::backdrop_blur)) — which the CPU walks
    /// away from where the device allows it ([`VulkanRenderer::run_commands_deferred`]).
    ///
    /// Safe mid element-build: the offscreen render already flushed and
    /// [`make_offscreen_sampleable`] left the source `SHADER_READ_ONLY`, so the source is fully
    /// written before the chain samples it. That used to rest on every submit fence-waiting; it now
    /// rests on submits being *ordered*, which is the condition the deferral is gated on and which
    /// holds whether or not anyone waits.
    ///
    /// `source` is passed back in rather than stored because the record has to hold it alive: this
    /// command buffer samples it long after `prepare_blur_vulkan` returns.
    ///
    /// [`make_offscreen_sampleable`]: crate::render_helpers::renderer::OffscreenRenderer::make_offscreen_sampleable
    pub(crate) fn run(
        &mut self,
        renderer: &mut VulkanRenderer,
        source: &VkTexture,
        offset: f32,
    ) -> Result<(), VulkanError> {
        let (w, h) = self.output.extent();
        let gpu = self.gpu.clone();
        let chain = &self.chain;
        let output = &self.output;
        renderer.run_commands_deferred(
            niri_vk::stats::SubmitSite::Blur,
            vec![source.clone()],
            vec![output.clone()],
            |cbuf| {
                chain.record(&gpu, cbuf, offset);
                chain.copy_output_to(&gpu, cbuf, output.image(), w, h);
            },
        )?;
        // copy_output_to barriers `output` to SHADER_READ_ONLY; record the tracked layout so a
        // later sample/readback knows it. True as soon as it is recorded — every later command is
        // ordered after this submit.
        self.output
            .set_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        self.valid = true;
        Ok(())
    }
}

impl Drop for EffectBlur {
    fn drop(&mut self) {
        // The chain's images and framebuffers are not refcounted and its teardown does not wait, so
        // a blur submit the CPU walked away from ([`Self::run`]) could still be reading them. Every
        // *texture* in that submit is held by the renderer's in-flight record; the chain is not,
        // and cannot be — it is destroyed from here, which has no renderer. So drain the
        // device first.
        //
        // Cheap in practice because this only runs when the chain is rebuilt (pass count changed,
        // or the source offscreen was recreated — i.e. a resize or a config change) or at teardown,
        // never per frame.
        unsafe {
            let _ = self.gpu.device.device_wait_idle();
        }
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
