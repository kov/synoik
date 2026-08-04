//! Cached blur resources for the owned Vulkan renderer's xray effect-buffer path — the blur half of
//! [`EffectBuffer`](crate::render_helpers::effect_buffer::EffectBuffer)'s Vulkan arm.
//!
//! A focused sibling of [`BackdropBlur`](super::backdrop_blur::BackdropBlur): it runs the same
//! dual-Kawase [`SharedBlurChain`] but over an **externally-owned** offscreen [`VkTexture`] (the
//! effect buffer's offscreen) rather than owning a capture of the scene. It is held *inside* the
//! effect buffer's Vulkan offscreen so it is rebuilt **atomically** with that texture — the chain
//! binds the source's image view at construction, so a chain left over a recreated offscreen would
//! sample a dangling descriptor. Per-frame `VkTexture` allocation is the virtio-gpu blob churn that
//! aborts Venus live, so `output` is created once and the chain is reused across frames (the source
//! view is stable; only its contents change, which [`Self::queue`] re-blurs).

use std::sync::Arc;

use ash::vk;
use smithay::backend::renderer::{Offscreen, Texture as _};

use super::blur_chain::SharedBlurChain;
use super::error::VulkanError;
use super::renderer::VulkanRenderer;
use super::types::{VkTexture, NATIVE_FOURCC};

pub(crate) struct EffectBlur {
    passes: usize,
    /// Samples the source offscreen (its descriptor set was bound to the source's stable view at
    /// construction); the owner rebuilds this whole `EffectBlur` when the source is recreated.
    /// Refcounted because a frame that has *recorded* the blur holds it until its submit retires
    /// — see [`SharedBlurChain`].
    chain: Arc<SharedBlurChain>,
    /// The blurred result, copied out of the chain's level 0 by the blur [`Self::queue`] queued.
    output: VkTexture,
    /// Whether `output` currently holds the blur of the source's latest contents. Reset (via
    /// [`Self::invalidate`]) when the source is re-rendered in place (same view, new pixels) so
    /// the next prepare re-runs the blur.
    valid: bool,
}

impl EffectBlur {
    /// Build the chain (bound to `source`'s view) plus a source-sized blurred-output buffer.
    /// `output` starts invalid — [`Self::queue`] must fill it before it is sampled.
    pub(crate) fn new(
        renderer: &mut VulkanRenderer,
        source: &VkTexture,
        passes: usize,
    ) -> Result<Self, VulkanError> {
        let chain = SharedBlurChain::new(&renderer.gpu, source.synoik_texture(), passes)?;
        let output = renderer.create_buffer(NATIVE_FOURCC, source.size())?;
        Ok(Self {
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

    /// Queue the dual-Kawase chain over `source`'s current contents into `output`, to be recorded
    /// into the **next frame's** command buffer ([`VulkanRenderer::queue_blur`]) rather than
    /// submitted here.
    ///
    /// This runs during element building, outside any frame, so there is no command buffer open to
    /// record into — the queue is how the work reaches one. It used to take a submit of its own,
    /// which the CPU at least walked away from; on this stack a submit is a host round trip whose
    /// cost has nothing to do with its payload, and the frame is making one anyway
    /// (`docs/fork/frame-submit-discipline.md`).
    ///
    /// Recorded-is-as-good-as-submitted applies as usual: nothing samples `output` before the
    /// frame that records the blur, because the only consumer is a draw *in* that frame, and
    /// `begin` drains the queue before its render pass opens. So the layout and `valid` are
    /// advanced here, as everywhere else in this renderer.
    ///
    /// `source` is passed back in rather than stored because the queue has to hold it alive: the
    /// frame's command buffer samples it long after `prepare_blur_vulkan` returns.
    pub(crate) fn queue(&mut self, renderer: &mut VulkanRenderer, source: &VkTexture, offset: f32) {
        renderer.queue_blur(
            self.chain.clone(),
            source.clone(),
            self.output.clone(),
            offset,
        );
        self.output
            .set_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        self.valid = true;
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
