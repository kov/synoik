// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! A cached one-directional smear of somebody else's texture — the workspace switch's motion blur.
//!
//! The sibling of [`GaussianBackdrop`](super::gaussian_backdrop::GaussianBackdrop), and it works
//! the same way for the same reasons: the chain binds its source's image view at construction, so
//! it is rebuilt whenever that texture is replaced, and the blur is *queued* for the frame's own
//! command buffer rather than submitted here (`docs/fork/frame-submit-discipline.md`).
//!
//! Two things differ, and both come from what this is for.
//!
//! **The radius is enormous and changes every frame.** A workspace switch is a critically damped
//! spring at stiffness 1000, so a seven-to-one jump peaks near 70 workspaces per second — about
//! 1200 px of travel between two 60 Hz frames, more than the screen is tall. That is why the
//! switch looked "busy": consecutive frames share no content at all, which is a strobe, not a
//! motion. A smear has to span that travel to read as movement, so the radius here is the travel
//! itself rather than some tasteful constant.
//!
//! **So the pyramid is built once, at full depth, and the radius picks how far down it goes.** A
//! chain sized to the current radius would be rebuilt on nearly every frame of the animation —
//! exactly the churn `BackdropBlur`'s size key once caused. [`BlurChain::record_directional`]
//! already descends only `k` rungs of the `passes` it has, so one allocation covers the whole
//! switch and the per-frame cost still tracks the per-frame radius.

use std::sync::Arc;

use ash::vk;
use smithay::backend::renderer::element::RenderElement;
use smithay::backend::renderer::{Offscreen, Texture as _};
use smithay::utils::Scale;
use synoik_vk::blur::{downscale_levels_axis, Axis};

use super::blur_chain::SharedBlurChain;
use super::error::VulkanError;
use super::renderer::VulkanRenderer;
use super::types::{VkTexture, NATIVE_FOURCC};
use crate::render_helpers::offscreen::{OffscreenBuffer, OffscreenRenderElement};

pub(crate) struct MotionBlur {
    chain: Arc<SharedBlurChain>,
    output: VkTexture,
    axis: Axis,
    /// The image the chain's descriptor set is bound to. A source texture that has been
    /// reallocated (the offscreen grew) is a different image, and the chain would sample a dead
    /// view — so this is the cache key, not the size.
    source_image: vk::Image,
    size: (u32, u32),
}

impl MotionBlur {
    /// Build a chain smearing `source` along `axis`, deep enough for any radius this will be asked
    /// for.
    ///
    /// "Any radius" is bounded by the caller: the compositor caps the travel it reports at
    /// [`MAX_TRAVEL_FACTOR`] times the axis extent, so the pyramid is sized for that and the
    /// clamp inside `record_directional` never bites.
    pub(crate) fn new(
        renderer: &mut VulkanRenderer,
        source: &VkTexture,
        axis: Axis,
    ) -> Result<Self, VulkanError> {
        let (w, h) = source.extent();
        let extent = match axis {
            Axis::Horizontal => w,
            Axis::Vertical => h,
        };
        let passes = downscale_levels_axis(extent, f64::from(extent) * MAX_TRAVEL_FACTOR).max(1);
        let output = renderer.create_buffer(NATIVE_FOURCC, source.size())?;
        let chain = SharedBlurChain::new_directional_into(
            &renderer.gpu,
            source.synoik_texture(),
            passes,
            axis,
            output.synoik_texture(),
        )?;
        Ok(Self {
            chain,
            output,
            axis,
            source_image: source.image(),
            size: (w, h),
        })
    }

    /// Whether this cache still describes `source` smeared along `axis` — see
    /// [`Self::source_image`](Self#structfield.source_image).
    pub(crate) fn matches(&self, source: &VkTexture, axis: Axis) -> bool {
        self.axis == axis && self.source_image == source.image() && self.size == source.extent()
    }

    /// The smeared result, to sample instead of the source.
    pub(crate) fn output(&self) -> &VkTexture {
        &self.output
    }

    /// Queue the smear into the frame's command buffer. `radius` is the travel, in the source
    /// texture's own pixels.
    ///
    /// Unconditional — unlike the cached gaussians, there is nothing to skip: the radius is
    /// different on every frame of an animation by construction, and the source has been
    /// re-rendered underneath it anyway.
    pub(crate) fn queue(&mut self, renderer: &mut VulkanRenderer, source: &VkTexture, radius: f64) {
        renderer.queue_directional_blur(
            self.chain.clone(),
            source.clone(),
            self.output.clone(),
            radius,
            1.0,
            self.axis,
        );
        self.output
            .set_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
    }
}

/// How much travel, as a multiple of the strip-axis extent, the pyramid is built to cover.
///
/// The cap exists so a pathological velocity cannot ask for rungs the chain does not have; one and
/// a half screens of travel in a single frame is already far past where the smear stops being
/// distinguishable from a wash.
pub(crate) const MAX_TRAVEL_FACTOR: f64 = 1.5;

/// One output's motion-blur state: the offscreen the sliding strip is composited into, and the
/// chain that smears it.
///
/// Per output, and held across frames, because both are sized to that output — and because two
/// monitors can be mid-switch at once. A single shared pair would fail
/// [`OffscreenBuffer`]'s uniqueness check and reallocate on every frame that both asked for it,
/// which is the shape of cache bug this codebase has already paid for three times.
#[derive(Debug, Default)]
pub(crate) struct MotionBlurSlot {
    offscreen: OffscreenBuffer,
    blur: Option<MotionBlur>,
    /// Whether the previous frame drew a *smeared* element from this slot. See [`Self::render`].
    was_smeared: bool,
}

impl MotionBlurSlot {
    /// Whether the previous frame left a smear on screen, and so still owes it a repaint.
    pub(crate) fn was_smeared(&self) -> bool {
        self.was_smeared
    }

    /// Composite `elements` as one group and hand back an element drawing them smeared by
    /// `motion`'s travel, in this texture's own pixels, along its axis.
    ///
    /// `motion` of `None` means the motion has stopped. The group still goes through the offscreen
    /// that one last time, unsmeared and fully damaged — because the frame before it put a blurred
    /// full-output element on the screen, and the sharp elements' own damage covers only where
    /// *they* changed. Everything the smear had reached but they do not would keep the blurred
    /// pixels, leaving the switch to settle into a screen that is still visibly streaked at the
    /// edges. A capture cannot show a missing repaint, so this is structural rather than something
    /// to look for later.
    ///
    /// `None` if the group could not be composited or the chain could not be built; the caller
    /// pushes the elements straight through, which is correct, merely unblurred.
    pub(crate) fn render(
        &mut self,
        renderer: &mut VulkanRenderer,
        scale: Scale<f64>,
        motion: Option<(Axis, f64)>,
        elements: &[impl RenderElement<VulkanRenderer>],
    ) -> Option<OffscreenRenderElement> {
        let (elem, _sync, _data) = match self.offscreen.render(renderer, scale, elements) {
            Ok(res) => res,
            Err(err) => {
                warn!("error compositing the workspace strip for its motion blur: {err:?}");
                self.was_smeared = false;
                return None;
            }
        };

        let Some((axis, travel)) = motion else {
            self.was_smeared = false;
            return Some(elem.with_full_damage());
        };

        let source = elem.texture().clone();
        if !self.blur.as_ref().is_some_and(|b| b.matches(&source, axis)) {
            match MotionBlur::new(renderer, &source, axis) {
                Ok(blur) => self.blur = Some(blur),
                Err(err) => {
                    warn!("error building the workspace switch motion blur: {err:?}");
                    self.blur = None;
                    self.was_smeared = false;
                    // The group composited fine; draw it sharp rather than dropping the frame.
                    return Some(elem);
                }
            }
        }

        let blur = self.blur.as_mut()?;
        blur.queue(renderer, &source, travel);
        self.was_smeared = true;
        Some(
            elem.with_source_texture(blur.output().clone())
                .with_full_damage(),
        )
    }
}

impl std::fmt::Debug for MotionBlur {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MotionBlur")
            .field("axis", &self.axis)
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}
