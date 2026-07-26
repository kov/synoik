//! Cached backdrop-blur state for the owned Vulkan renderer's `FramebufferEffectElement` path
//! (niri's GNOME-style backdrop blur). Owns the mid-frame capture plus, when blur is enabled, the
//! dual-Kawase [`BlurChain`] and its blurred output. Held across frames in the effect element's
//! `UserDataMap` so nothing is allocated per frame — per-frame `VkTexture` creation is the
//! virtio-gpu blob churn that aborts Venus live, so this MUST be cached; it is recreated only when
//! the intermediate size or the blur pass-count changes.

use std::sync::Arc;

use ash::vk;
use niri_vk::blur::BlurChain;
use niri_vk::gpu::Gpu;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::Offscreen;
use smithay::utils::{Buffer as BufferCoord, Size};

use super::error::VulkanError;
use super::renderer::VulkanRenderer;
use super::types::VkTexture;

/// The blur resources, present only when blur is enabled (`BlurOptions` was `Some`).
struct BlurState {
    passes: usize,
    chain: BlurChain,
    /// The blurred result, copied out of the chain's level 0 each frame.
    output: VkTexture,
}

/// Cached per-effect blur resources — see the module docs.
pub(crate) struct BackdropBlur {
    /// Kept so [`Drop`] can free the (Drop-less) [`BlurChain`] and to record the blur; an `Arc`
    /// clone keeps the device alive regardless of renderer/cache drop order.
    gpu: Arc<Gpu>,
    size: (u32, u32),
    /// The captured backdrop, filled each frame by [`VulkanFrame::capture_region`] and left
    /// `SHADER_READ_ONLY_OPTIMAL` (sampleable by the chain or, unblurred, by the postprocess
    /// draw).
    ///
    /// [`VulkanFrame::capture_region`]: super::frame::VulkanFrame::capture_region
    capture: VkTexture,
    blur: Option<BlurState>,
}

impl BackdropBlur {
    /// (Re)create the capture (and, when `passes` is `Some`, the blur chain + output) sized to
    /// `size`. `passes` comes from the element's `BlurOptions` (`None` = blur off: the raw capture
    /// is the composite source). The per-frame `offset` is passed to [`Self::run_blur`], not
    /// stored.
    pub(crate) fn new(
        renderer: &mut VulkanRenderer,
        size: Size<i32, BufferCoord>,
        passes: Option<usize>,
    ) -> Result<Self, VulkanError> {
        let capture = renderer.create_buffer(Fourcc::Abgr8888, size)?;
        let dims = capture.extent();
        let blur = match passes {
            Some(passes) => {
                // The chain samples `capture` (its descriptor set is built here, bound to the
                // capture's stable view) — capture_region refills the capture each frame, so a
                // cached chain stays valid across frames.
                let chain = BlurChain::new(&renderer.gpu, capture.niri_texture(), passes)?;
                let output = renderer.create_buffer(Fourcc::Abgr8888, size)?;
                Some(BlurState {
                    passes,
                    chain,
                    output,
                })
            }
            None => None,
        };
        Ok(Self {
            gpu: renderer.gpu.clone(),
            size: dims,
            capture,
            blur,
        })
    }

    /// Whether this cache already matches the requested intermediate size + blur pass-count (and so
    /// can be reused instead of rebuilt).
    pub(crate) fn matches(&self, size: (u32, u32), passes: Option<usize>) -> bool {
        self.size == size && self.blur.as_ref().map(|b| b.passes) == passes
    }

    /// The capture destination for [`VulkanFrame::capture_region`].
    ///
    /// [`VulkanFrame::capture_region`]: super::frame::VulkanFrame::capture_region
    pub(crate) fn capture(&self) -> &VkTexture {
        &self.capture
    }

    /// Record the blur (down/up chain + copy into `output`) on its own submission, sampling the
    /// just-captured `self.capture`. A no-op when blur is disabled. Runs on a separate one-shot
    /// command buffer (like `render_blur`) that the CPU walks away from where the device allows it
    /// ([`VulkanRenderer::run_commands_deferred`]).
    ///
    /// Safe mid-frame because `capture_region` already flushed the capture and submits are ordered,
    /// so the caller's postprocess draw — recorded after this — samples `output` only once the blur
    /// has run. That used to rest on this submit fence-waiting; ordering is what it rests on now,
    /// and ordering is the condition the deferral is gated on.
    pub(crate) fn run_blur(
        &self,
        renderer: &mut VulkanRenderer,
        offset: f32,
    ) -> Result<(), VulkanError> {
        let Some(bs) = &self.blur else {
            return Ok(());
        };
        let (w, h) = self.size;
        let gpu = self.gpu.clone();
        renderer.run_commands_deferred(
            niri_vk::stats::SubmitSite::Blur,
            vec![self.capture.clone()],
            vec![bs.output.clone()],
            |cbuf| {
                bs.chain.record(&gpu, cbuf, offset);
                bs.chain.copy_output_to(&gpu, cbuf, bs.output.image(), w, h);
            },
        )?;
        // copy_output_to leaves `output` in SHADER_READ_ONLY (with a barrier); record the tracked
        // layout so a later readback/sample knows it. True as soon as it is recorded — every later
        // command is ordered after this submit.
        bs.output
            .set_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        Ok(())
    }

    /// The texture to composite: the blurred output when blur is on, else the raw capture.
    pub(crate) fn intermediate(&self) -> &VkTexture {
        self.blur.as_ref().map_or(&self.capture, |b| &b.output)
    }
}

impl Drop for BackdropBlur {
    fn drop(&mut self) {
        // BlurChain has no Drop (manual teardown, matching the rest of niri-vk); free its GPU
        // resources. The VkTextures free themselves via their own Drop.
        if let Some(bs) = &self.blur {
            // The chain is not refcounted and its teardown does not wait, while `run_blur` leaves a
            // submit reading it in flight. The renderer's in-flight record holds that submit's
            // *textures*, but nothing can hold the chain — this is where it dies, and there is no
            // renderer here. Drain first. Only runs when the cache misses on size/pass count, or at
            // teardown; never per frame. Same reasoning as `EffectBlur`'s `Drop`.
            unsafe {
                let _ = self.gpu.device.device_wait_idle();
            }
            bs.chain.destroy(&self.gpu);
        }
    }
}
