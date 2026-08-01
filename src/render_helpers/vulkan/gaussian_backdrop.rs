//! A cached gaussian blur of somebody else's texture — GNOME's `Shell.BlurEffect` in
//! `BACKGROUND` mode, as the lock screen puts it on its wallpaper (`unlockDialog.js:706-713`).
//!
//! The sibling of [`EffectBlur`](super::effect_blur::EffectBlur), and it works the same way for the
//! same reasons: the chain binds its source's image view at construction, so it must be rebuilt
//! whenever that texture is replaced; the blur is *queued* for the next frame's command buffer
//! rather than submitted here, because this runs during element building where no command buffer is
//! open and a submit of its own would be a host round trip the frame was going to make anyway
//! (`docs/fork/frame-submit-discipline.md`).
//!
//! What differs is the parameter. The Kawase chain takes a tap offset; this takes a **radius in the
//! source texture's pixels**, because that is the only thing GNOME's blur is specified in. Callers
//! whose texture is not at output resolution owe the conversion — see
//! [`crate::wallpaper::Wallpaper::render_blurred`], where a 4K picture on a 1080p screen needs
//! twice the radius to look the same.

use std::sync::Arc;

use ash::vk;
use smithay::backend::renderer::{Offscreen, Texture as _};

use super::blur_chain::SharedBlurChain;
use super::error::VulkanError;
use super::renderer::VulkanRenderer;
use super::types::{VkTexture, NATIVE_FOURCC};

pub(crate) struct GaussianBackdrop {
    chain: Arc<SharedBlurChain>,
    output: VkTexture,
    /// What `output` currently holds the blur for. `None` means nothing yet; a mismatch means the
    /// radius or brightness moved and the blur has to run again.
    ///
    /// Not a bool, because this cache's whole job is to *not* re-blur a wallpaper that has not
    /// changed: a lock screen redraws on every clock tick and on every keystroke, and re-running a
    /// full-screen blur for each of those is the cost this exists to avoid.
    blurred_for: Option<(f64, f32)>,
}

impl GaussianBackdrop {
    /// Build a chain over `source` with just enough rungs for `radius`.
    ///
    /// Sizing the pyramid to the radius rather than to some generous maximum matters here: the
    /// source is a wallpaper, which can be 8192px on a side, and every unused rung is an
    /// allocation twice over (a level and its ping-pong twin).
    pub(crate) fn new(
        renderer: &mut VulkanRenderer,
        source: &VkTexture,
        radius: f64,
    ) -> Result<Self, VulkanError> {
        let (w, h) = source.extent();
        let passes = niri_vk::blur::downscale_levels(w, h, radius).max(1);
        let chain = SharedBlurChain::new_gaussian(&renderer.gpu, source.niri_texture(), passes)?;
        let output = renderer.create_buffer(NATIVE_FOURCC, source.size())?;
        Ok(Self {
            chain,
            output,
            blurred_for: None,
        })
    }

    pub(crate) fn output(&self) -> &VkTexture {
        &self.output
    }

    /// Whether `output` already holds this exact blur.
    pub(crate) fn is_current(&self, radius: f64, brightness: f32) -> bool {
        self.blurred_for == Some((radius, brightness))
    }

    /// Queue the blur into the next frame's command buffer.
    ///
    /// `source` comes back in rather than being stored so the queue can hold it alive: the frame's
    /// command buffer samples it long after this returns.
    pub(crate) fn queue(
        &mut self,
        renderer: &mut VulkanRenderer,
        source: &VkTexture,
        radius: f64,
        brightness: f32,
    ) {
        renderer.queue_gaussian_blur(
            self.chain.clone(),
            source.clone(),
            self.output.clone(),
            radius,
            brightness,
        );
        self.output
            .set_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        self.blurred_for = Some((radius, brightness));
    }
}

impl std::fmt::Debug for GaussianBackdrop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GaussianBackdrop")
            .field("blurred_for", &self.blurred_for)
            .finish_non_exhaustive()
    }
}
