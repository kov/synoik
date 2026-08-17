// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! A [`BlurChain`] with a lifetime someone else can hold on to.
//!
//! `BlurChain` owns raw Vulkan objects — render passes, pipelines, descriptor sets, level images —
//! and has no `Drop` (its teardown needs a `&Gpu`), so both blur users freed it by hand and drained
//! the device first, because a submit the CPU had walked away from might still be reading it.
//!
//! That stops working once the blur is *recorded* into a command buffer instead of submitted on one
//! of its own. Recording binds those objects to a command buffer that has not been submitted yet,
//! and destroying any of them invalidates the whole buffer — the frame's, in this case, taking
//! every draw recorded after it down with it. `device_wait_idle` cannot help: there is nothing in
//! flight to wait for. It shows up as `VkRenderPass … was destroyed` from `vkCmdEndRenderPass`, and
//! the frame dies with `ERROR_DEVICE_LOST`.
//!
//! So the chain is refcounted, and whoever records it holds a reference until its submit retires —
//! the same rule as every other queued resource (`docs/fork/frame-submit-discipline.md`), just for
//! the one kind of object that had no way to express it.

use std::sync::Arc;

use ash::vk;
use synoik_vk::blur::BlurChain;
use synoik_vk::gpu::Gpu;
use synoik_vk::texture::Texture as SynoikTexture;

use super::error::VulkanError;

/// A gaussian blur chain that lives as long as its last holder — its owner (a `BackdropBlur` or
/// an `EffectBlur`) and every frame that has recorded it but not yet retired.
pub(crate) struct SharedBlurChain {
    /// Keeps the device alive regardless of renderer/cache drop order, and is what `Drop` needs to
    /// free the chain.
    gpu: Arc<Gpu>,
    chain: BlurChain,
}

impl SharedBlurChain {
    /// Build a chain sampling `source`, with `passes` halving rungs.
    pub(crate) fn new_gaussian(
        gpu: &Arc<Gpu>,
        source: &SynoikTexture,
        passes: usize,
    ) -> Result<Arc<Self>, VulkanError> {
        Self::build(gpu, source, passes, None)
    }

    /// As [`Self::new_gaussian`], but the magnifying upsample renders straight into `dst` — an
    /// image the caller owns, level-0 sized and `COLOR_ATTACHMENT`-usable — so
    /// [`Self::record_gaussian_into`] has nothing to copy afterwards. Worth it
    /// only for a blur that re-runs every frame — a cached one (the lock screen's wallpaper, the
    /// xray buffer) re-blurs so rarely that the copy is noise.
    pub(crate) fn new_gaussian_into(
        gpu: &Arc<Gpu>,
        source: &SynoikTexture,
        passes: usize,
        dst: &SynoikTexture,
    ) -> Result<Arc<Self>, VulkanError> {
        Self::build(gpu, source, passes, Some(dst))
    }

    fn build(
        gpu: &Arc<Gpu>,
        source: &SynoikTexture,
        passes: usize,
        dst: Option<&SynoikTexture>,
    ) -> Result<Arc<Self>, VulkanError> {
        let mut chain = BlurChain::new(gpu, source, passes)?;
        if let Some(dst) = dst {
            chain.set_external_dst(gpu, dst.view, dst.width, dst.height)?;
        }
        Ok(Arc::new(Self {
            gpu: gpu.clone(),
            chain,
        }))
    }

    /// Record the whole blur — GNOME's separable gaussian at a `2σ` radius in the **source
    /// texture's** pixels, plus the brightness multiply that goes with it — into a command buffer
    /// the caller is already submitting. Must be outside a render pass (the chain begins its own).
    /// The copy into `output` is skipped when the chain has an external destination.
    ///
    /// The caller must keep this `Arc` alive until that submit retires; see the module docs for
    /// what happens otherwise.
    pub(crate) fn record_gaussian_into(
        &self,
        cbuf: vk::CommandBuffer,
        radius: f64,
        brightness: f32,
        output: vk::Image,
        width: u32,
        height: u32,
    ) {
        self.chain
            .record_gaussian(&self.gpu, cbuf, radius, brightness);
        if !self.chain.has_external_dst() {
            self.chain
                .copy_output_to(&self.gpu, cbuf, output, width, height);
        }
    }
}

impl Drop for SharedBlurChain {
    fn drop(&mut self) {
        // No wait here, and none needed: **refcount zero already means every submit that recorded
        // this chain has completed.** The three ways a reference is released each prove it —
        //
        // - A *deferred* finish moves the frame's chains into an [`InFlightSubmit`]
        //   (`VulkanFrame::finish_internal_impl`), and `VulkanRenderer::retire_completed` drains
        //   only submits whose timeline value the queue has already **passed**.
        // - A *synchronous* finish parks on `wait_for_fences` and only then lets the frame — still
        //   holding its chains — drop.
        // - A frame that errors before its submit, or is abandoned, has nothing executing at all.
        // - The one recording that happens outside a frame, `flush_pending_blurs`, goes through
        //   `run_commands`, which waits — and holds the chains across it.
        //
        // It used to `device_wait_idle` here, defended by a comment claiming this "only runs when
        // a chain is rebuilt … never per frame". `BackdropBlur::matches` keys on the exact
        // intermediate size, so an animating geometry rebuilds — and stalled — every single frame
        // (`vulkan_backdrop_blur_reuses_across_a_size_sweep` counts the rebuilds). That is the
        // whole reason the wait had to go: it was a full-device stall on the compositor thread,
        // once per animation frame, guarding an invariant the refcount already gives.
        //
        // The renderer's own teardown is covered too: `VulkanRenderer::drop` runs
        // `drain_in_flight()` (which waits) before destroying anything, so a chain outliving the
        // renderer — it holds its own `Arc<Gpu>` — has nothing in flight either.
        self.chain.destroy(&self.gpu);
    }
}
