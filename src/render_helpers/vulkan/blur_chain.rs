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

/// A dual-Kawase blur chain that lives as long as its last holder — its owner (a `BackdropBlur` or
/// an `EffectBlur`) and every frame that has recorded it but not yet retired.
pub(crate) struct SharedBlurChain {
    /// Keeps the device alive regardless of renderer/cache drop order, and is what `Drop` needs to
    /// free the chain.
    gpu: Arc<Gpu>,
    chain: BlurChain,
}

impl SharedBlurChain {
    /// Build a chain sampling `source`, with `passes` down/up levels.
    pub(crate) fn new(
        gpu: &Arc<Gpu>,
        source: &SynoikTexture,
        passes: usize,
    ) -> Result<Arc<Self>, VulkanError> {
        Self::build(gpu, source, passes, false)
    }

    /// As [`Self::new`], plus the pipelines [`Self::record_gaussian_into`] needs.
    pub(crate) fn new_gaussian(
        gpu: &Arc<Gpu>,
        source: &SynoikTexture,
        passes: usize,
    ) -> Result<Arc<Self>, VulkanError> {
        Self::build(gpu, source, passes, true)
    }

    fn build(
        gpu: &Arc<Gpu>,
        source: &SynoikTexture,
        passes: usize,
        gaussian: bool,
    ) -> Result<Arc<Self>, VulkanError> {
        let chain = if gaussian {
            BlurChain::new_with_gaussian(gpu, source, passes)?
        } else {
            BlurChain::new(gpu, source, passes)?
        };
        Ok(Arc::new(Self {
            gpu: gpu.clone(),
            chain,
        }))
    }

    /// Record the down/up passes and the copy into `output` — the whole blur, into a command
    /// buffer the caller is already submitting. Must be outside a render pass (the chain begins
    /// its own).
    ///
    /// The caller must keep this `Arc` alive until that submit retires; see the module docs for
    /// what happens otherwise.
    /// As [`Self::record_into`], but GNOME's separable gaussian: a radius in the **source
    /// texture's** pixels, and the brightness multiply that goes with it.
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
        self.chain
            .copy_output_to(&self.gpu, cbuf, output, width, height);
    }

    pub(crate) fn record_into(
        &self,
        cbuf: vk::CommandBuffer,
        offset: f32,
        output: vk::Image,
        width: u32,
        height: u32,
    ) {
        self.chain.record(&self.gpu, cbuf, offset);
        self.chain
            .copy_output_to(&self.gpu, cbuf, output, width, height);
    }
}

impl Drop for SharedBlurChain {
    fn drop(&mut self) {
        // Every recorded reference is gone by construction — that is what the refcount means — but
        // a submit carrying one may still be *executing*, and the chain's teardown does not wait.
        // Only runs when a chain is rebuilt (pass count changed, source recreated) or at teardown,
        // never per frame.
        unsafe {
            let _ = self.gpu.device.device_wait_idle();
        }
        self.chain.destroy(&self.gpu);
    }
}
