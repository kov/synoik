// SPDX-License-Identifier: GPL-3.0-only
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

use std::mem;

use anyhow::Context as _;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::{Id, RenderElementStates};
use smithay::backend::renderer::utils::CommitCounter;
use smithay::backend::renderer::{
    Bind as _, Color32F, ContextId, Offscreen as _, Renderer as _, Texture,
};
use smithay::utils::{Buffer, Logical, Physical, Scale, Size, Transform};

use crate::render_helpers::blur::BlurOptions;
use crate::render_helpers::renderer::OffscreenRenderer as _;
use crate::render_helpers::vulkan::{EffectBlur, VkTexture, VulkanRenderer};
use crate::render_helpers::NATIVE_FOURCC;
use crate::synoik::OutputRenderElements;

#[derive(Debug)]
pub struct EffectBuffer {
    /// Id to be used for this effect buffer's elements.
    id: Id,

    /// Size of the effect buffer.
    size: Size<i32, Buffer>,
    /// Scale of the effect buffer.
    scale: Scale<f64>,
    /// Options for blurring.
    blur_options: BlurOptions,

    /// Elements to be rendered on demand.
    elements_vk: ElementsVk,
    /// Offscreen buffer where elements get rendered, plus its blur.
    offscreen_vk: Option<OffscreenVk>,

    /// Commit counter that takes into account both original and blurred texture changes.
    commit_counter: CommitCounter,
}

#[derive(Debug)]
enum ElementsVk {
    /// Contents remain unchanged.
    Unchanged(
        // Storage to avoid reallocating it every time.
        Vec<OutputRenderElements>,
    ),
    /// New contents, need to check damage and render.
    New(Vec<OutputRenderElements>),
}

impl Default for ElementsVk {
    fn default() -> Self {
        Self::Unchanged(Vec::new())
    }
}

/// The offscreen and its blur. The [`EffectBlur`] lives here (rather than beside it) so recreating
/// the offscreen texture — which sets `offscreen_vk = None` — drops and rebuilds the blur chain
/// against the new texture view atomically (the chain binds a fixed source view and has no `Drop`).
#[derive(Debug)]
struct OffscreenVk {
    /// The texture with the offscreen contents.
    texture: VkTexture,
    /// Id of the renderer context that the texture comes from.
    renderer_context_id: ContextId<VkTexture>,
    /// Scale of the texture.
    scale: Scale<f64>,
    /// Damage tracker for drawing to the texture.
    damage: OutputDamageTracker,
    /// Render element states from the last render into the offscreen.
    states: RenderElementStates,
    /// Blurred version of the texture. Present once blur has been requested; when the offscreen
    /// contents change, its validity is reset (see [`EffectBlur::invalidate`]) rather than
    /// dropped.
    blur: Option<EffectBlur>,
}

impl EffectBuffer {
    pub fn new() -> Self {
        Self {
            id: Id::new(),
            size: Size::default(),
            scale: Scale::from(1.),
            blur_options: BlurOptions::default(),
            elements_vk: ElementsVk::default(),
            offscreen_vk: None,
            commit_counter: CommitCounter::default(),
        }
    }

    pub fn id(&self) -> &Id {
        &self.id
    }

    pub fn commit(&self) -> CommitCounter {
        self.commit_counter
    }

    pub fn logical_size(&self) -> Size<f64, Logical> {
        self.size.to_f64().to_logical(self.scale, Transform::Normal)
    }

    pub fn scale(&self) -> Scale<f64> {
        self.scale
    }

    /// States from the last render into the offscreen, or `None` before the first one.
    ///
    /// Reads the Vulkan arm: it is the one the live session fills. Reading the GLES arm here meant
    /// this returned `None` on every real frame, which silently disabled the background-layer id
    /// remap in `Synoik::update_primary_scanout_output` (synoik.rs) — a background layer surface
    /// visible only through an xray blur was then treated as un-presented and its frame callbacks
    /// throttled.
    ///
    /// Callers must have filled and prepared this buffer during the frame they read it in: the
    /// states are only rewritten on the `ElementsVk::New` path, so a caller that skipped its
    /// `elements_vulkan()` + `prepare_vulkan()` would silently get the previous frame's states and
    /// gate frame callbacks on them. Today `Synoik::fill_xray_elements` does both on every frame.
    pub fn render_element_states(&self) -> Option<&RenderElementStates> {
        self.offscreen_vk.as_ref().map(|o| &o.states)
    }

    pub fn update_size(&mut self, size: Size<i32, Physical>, scale: Scale<f64>) {
        self.size = size.to_logical(1).to_buffer(1, Transform::Normal);
        self.scale = scale;
    }

    pub fn update_blur_options(&mut self, options: BlurOptions) {
        if self.blur_options == options {
            return;
        }

        self.blur_options = options;

        let mut had_blur = false;
        // A pass-count change is picked up by `prepare_blur_vulkan` (it rebuilds the chain when
        // `passes` differs), so invalidating is enough here.
        if let Some(offscreen) = &mut self.offscreen_vk {
            if let Some(blur) = &mut offscreen.blur {
                if blur.is_valid() {
                    blur.invalidate();
                    had_blur = true;
                }
            }
        }
        if had_blur {
            self.commit_counter.increment();
        }
    }

    /// The storage the caller pushes render elements into before [`Self::prepare_vulkan`].
    pub fn elements_vulkan(&mut self) -> &mut Vec<OutputRenderElements> {
        // Assume we're going to insert new elements, switch to New.
        match mem::take(&mut self.elements_vk) {
            ElementsVk::Unchanged(elements) | ElementsVk::New(elements) => {
                self.elements_vk = ElementsVk::New(elements);
            }
        }
        let ElementsVk::New(elements) = &mut self.elements_vk else {
            unreachable!();
        };
        elements
    }

    /// (Re)render the offscreen and, when `blur`, its blurred version. Returns `false` on error
    /// (the caller skips drawing the effect).
    ///
    /// The blur is prepared **eagerly** here rather than lazily at draw time, which keeps it in the
    /// same place as the offscreen render it follows. It no longer costs a submission: it is queued
    /// for the next frame's command buffer ([`EffectBlur::queue`]). Both are damage-gated
    /// ([`Self::prepare_blur_vulkan`] only queues the blur when the offscreen actually
    /// re-rendered), so a steady frame does no work here.
    pub fn prepare_vulkan(&mut self, renderer: &mut VulkanRenderer, blur: bool) -> bool {
        if let Err(err) = self.prepare_offscreen_vulkan(renderer) {
            warn!("error preparing Vulkan offscreen: {err:?}");
            return false;
        }

        if blur {
            if let Err(err) = self.prepare_blur_vulkan(renderer) {
                warn!("error preparing Vulkan blur: {err:?}");
                return false;
            }
        }

        true
    }

    fn prepare_offscreen_vulkan(&mut self, renderer: &mut VulkanRenderer) -> anyhow::Result<()> {
        let _span = tracy_client::span!("EffectBuffer::prepare_offscreen_vulkan");

        // A zero-size buffer (a fresh EffectBuffer before `update_size`) can't back a VkImage
        // (`vkCreateImage` with a 0 extent is invalid usage, and the blur chain would build 0-size
        // levels). GLES tolerates 0-size GL textures; the Vulkan arm must skip. `texture_vulkan`
        // then returns `None` and the caller skips the effect.
        if self.size.w <= 0 || self.size.h <= 0 {
            return Ok(());
        }

        // Same recreation policy as `prepare_offscreen`, expressed through the renderer-neutral
        // offscreen traits (`Offscreen`/`Bind`/`OffscreenRenderer`) so the policy lives in one
        // place.
        let size_string;
        let mut reason = "";
        if let Some(OffscreenVk {
            texture,
            renderer_context_id,
            ..
        }) = &mut self.offscreen_vk
        {
            let old_size = texture.size();
            if old_size != self.size {
                size_string = format!(
                    "size changed from {} × {} to {} × {}",
                    old_size.w, old_size.h, self.size.w, self.size.h
                );
                reason = &size_string;

                self.offscreen_vk = None;
            } else if !renderer.offscreen_is_reusable(texture) {
                reason = "not unique";

                self.offscreen_vk = None;
            } else if *renderer_context_id != renderer.context_id() {
                reason = "renderer id changed";

                self.offscreen_vk = None;
            }
        } else {
            reason = "first render";
        }

        let offscreen = if let Some(offscreen) = &mut self.offscreen_vk {
            offscreen
        } else {
            trace!("creating new Vulkan offscreen texture: {reason}");
            let span = tracy_client::span!("creating effect offscreen texture (vulkan)");
            span.emit_text(reason);

            let texture: VkTexture = renderer
                .create_buffer(NATIVE_FOURCC, self.size)
                .context("error creating texture")?;

            let buffer_size = self.size.to_logical(1, Transform::Normal).to_physical(1);
            let damage = OutputDamageTracker::new(buffer_size, self.scale, Transform::Normal);

            self.offscreen_vk.insert(OffscreenVk {
                texture,
                renderer_context_id: renderer.context_id(),
                scale: self.scale,
                damage,
                states: RenderElementStates::default(),
                blur: None,
            })
        };

        // Recreate the damage tracker if the scale changes (mirrors `prepare_offscreen`).
        if offscreen.scale != self.scale {
            offscreen.scale = self.scale;

            trace!("recreating damage tracker due to scale change");
            let buffer_size = self.size.to_logical(1, Transform::Normal).to_physical(1);
            offscreen.damage = OutputDamageTracker::new(buffer_size, self.scale, Transform::Normal);

            self.commit_counter.increment();
            if let Some(blur) = &mut offscreen.blur {
                blur.invalidate();
            }
        }

        // Render the elements if any.
        let mut elements = match mem::take(&mut self.elements_vk) {
            ElementsVk::New(elements) => elements,
            x @ ElementsVk::Unchanged(_) => {
                // No redrawing necessary — but still ensure the (possibly freshly recreated)
                // texture is in a sampleable layout. `render_output` leaves a just-rendered
                // offscreen in `TRANSFER_SRC_OPTIMAL`, while the blur chain and
                // `render_postprocess` sample `SHADER_READ_ONLY`;
                // `make_offscreen_sampleable` no-ops when already sampleable and
                // handles a fresh `UNDEFINED` texture, so this is safe on every path.
                self.elements_vk = x;
                renderer
                    .make_offscreen_sampleable(&offscreen.texture)
                    .context("error preparing offscreen for sampling")?;
                return Ok(());
            }
        };

        let res = {
            let mut target = renderer
                .bind(&mut offscreen.texture)
                .context("error binding texture")?;
            offscreen
                .damage
                .render_output(renderer, &mut target, 1, &elements, Color32F::TRANSPARENT)
                .context("error rendering")?
        };

        // Make the just-rendered offscreen sampleable by a later postprocess draw / the blur chain
        // (a no-op on GLES; the owned Vulkan renderer inserts the layout barrier its images need).
        renderer
            .make_offscreen_sampleable(&offscreen.texture)
            .context("error preparing offscreen for sampling")?;

        offscreen.states = res.states;

        if res.damage.is_some() {
            self.commit_counter.increment();

            // Original texture changed; the blurred version is stale.
            if let Some(blur) = &mut offscreen.blur {
                blur.invalidate();
            }
        }

        // Clear and put the storage back.
        elements.clear();
        self.elements_vk = ElementsVk::Unchanged(elements);

        Ok(())
    }

    fn prepare_blur_vulkan(&mut self, renderer: &mut VulkanRenderer) -> anyhow::Result<()> {
        let passes = (self.blur_options.passes as usize).clamp(1, 31);
        let offset = self.blur_options.offset as f32;

        let offscreen = self.offscreen_vk.as_mut().context("missing offscreen")?;

        // Rebuild the chain when it is missing or the pass count changed. A texture recreation
        // already dropped it (offscreen_vk was set to None), so this only handles the same-texture
        // pass-count change; the fresh chain binds the current texture view.
        let need_new = offscreen.blur.as_ref().is_none_or(|b| b.passes() != passes);
        if need_new {
            offscreen.blur = Some(EffectBlur::new(renderer, &offscreen.texture, passes)?);
        }

        // Split the borrow: queueing hands the source texture to the renderer alongside the blur's
        // own output, both held until the frame that records it retires.
        let OffscreenVk { texture, blur, .. } = offscreen;
        let blur = blur.as_mut().expect("just populated");
        if !blur.is_valid() {
            blur.queue(renderer, texture, offset);
        }

        Ok(())
    }

    /// The texture the effect element should sample: the blurred output when `blur` is requested,
    /// else the raw offscreen. `None` before the first successful [`Self::prepare_vulkan`] — and,
    /// when `blur` is requested, also `None` if the blurred output is not valid. We deliberately do
    /// NOT fall back to the unblurred offscreen there: `prepare_vulkan` runs the blur eagerly, so a
    /// missing/invalid blur means prepare failed and the caller should draw nothing rather than
    /// silently sample an unblurred texture — which, behind an xray, would show through content the
    /// blur was meant to obscure.
    pub fn texture_vulkan(&self, blur: bool) -> Option<VkTexture> {
        let offscreen = self.offscreen_vk.as_ref()?;
        if blur {
            return offscreen
                .blur
                .as_ref()
                .filter(|b| b.is_valid())
                .map(|b| b.output().clone());
        }
        Some(offscreen.texture.clone())
    }
}
