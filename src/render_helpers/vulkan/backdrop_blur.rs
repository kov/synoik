//! Cached backdrop-blur state for the owned Vulkan renderer's `FramebufferEffectElement` path
//! (niri's GNOME-style backdrop blur). Owns the mid-frame capture plus, when blur is enabled, the
//! dual-Kawase [`SharedBlurChain`] and its blurred output. Held across frames in the effect
//! element's `UserDataMap` so nothing is allocated per frame — per-frame `VkTexture` creation is
//! the virtio-gpu blob churn that aborts Venus live, so this MUST be cached; it is recreated only
//! when the intermediate size or the blur pass-count changes.

use std::sync::Arc;

use ash::vk;
use smithay::backend::renderer::Offscreen;
use smithay::utils::{Buffer as BufferCoord, Size};

use super::blur_chain::SharedBlurChain;
use super::error::VulkanError;
use super::renderer::VulkanRenderer;
use super::types::{VkTexture, NATIVE_FOURCC};

/// The blur resources, present only when blur is enabled (`BlurOptions` was `Some`).
struct BlurState {
    passes: usize,
    /// Refcounted because a frame that has *recorded* the blur but not yet submitted it holds a
    /// reference too — see [`SharedBlurChain`].
    chain: Arc<SharedBlurChain>,
    /// The blurred result, copied out of the chain's level 0 each frame.
    output: VkTexture,
}

/// Cached per-effect blur resources — see the module docs.
pub(crate) struct BackdropBlur {
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
    /// is the composite source). The per-frame `offset` is passed to [`Self::record_blur`], not
    /// stored.
    pub(crate) fn new(
        renderer: &mut VulkanRenderer,
        size: Size<i32, BufferCoord>,
        passes: Option<usize>,
    ) -> Result<Self, VulkanError> {
        let capture = renderer.create_buffer(NATIVE_FOURCC, size)?;
        let dims = capture.extent();
        let blur = match passes {
            Some(passes) => {
                // The chain samples `capture` (its descriptor set is built here, bound to the
                // capture's stable view) — capture_region refills the capture each frame, so a
                // cached chain stays valid across frames.
                let chain = SharedBlurChain::new(&renderer.gpu, capture.synoik_texture(), passes)?;
                let output = renderer.create_buffer(NATIVE_FOURCC, size)?;
                Some(BlurState {
                    passes,
                    chain,
                    output,
                })
            }
            None => None,
        };
        Ok(Self {
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

    /// Record the blur (down/up chain + copy into `output`) into `cbuf`, sampling the
    /// just-captured `self.capture`. A no-op when blur is disabled.
    ///
    /// `cbuf` is the **frame's own** command buffer, in the gap
    /// [`VulkanFrame::capture_region`](super::frame::VulkanFrame::capture_region) opens between
    /// its two render passes — so the blur costs no submit at all. It used to take one of its own
    /// (deferred, but still a round trip), which in turn is why the capture had to flush: a
    /// separate submission cannot see an un-submitted blit. Both are gone; the capture's `to_read`
    /// barrier, recorded just above this in the same command buffer, is what orders the chain
    /// after the blit.
    pub(crate) fn record_blur(&self, cbuf: vk::CommandBuffer, offset: f32) {
        let Some(bs) = &self.blur else {
            return;
        };
        let (w, h) = self.size;
        bs.chain.record_into(cbuf, offset, bs.output.image(), w, h);
        // copy_output_to leaves `output` in SHADER_READ_ONLY (with a barrier); record the tracked
        // layout so a later readback/sample knows it. True as soon as it is recorded — the
        // postprocess draw that samples it is recorded after this, in this same command buffer.
        bs.output
            .set_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
    }

    /// The texture to composite: the blurred output when blur is on, else the raw capture.
    pub(crate) fn intermediate(&self) -> &VkTexture {
        self.blur.as_ref().map_or(&self.capture, |b| &b.output)
    }

    /// The chain a frame has recorded, to hold until that frame's submit retires. `None` when blur
    /// is off (nothing was recorded). See [`SharedBlurChain`].
    pub(crate) fn chain(&self) -> Option<Arc<SharedBlurChain>> {
        self.blur.as_ref().map(|b| b.chain.clone())
    }
}
