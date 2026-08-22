// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

use std::fmt;
use std::sync::Arc;

use ash::vk;
use glam::{Mat3, Vec2, Vec3};
use smithay::backend::renderer::sync::SyncPoint;
use smithay::backend::renderer::{Color32F, ContextId, Frame, Texture};
use smithay::utils::{Buffer as BufferCoord, Physical, Point, Rectangle, Size, Transform};
use synoik_vk::render::{
    as_bytes, BorderPush, ClippedTexturePush, PostprocessPush, QuadPush, ResizePush, ShadowPush,
    TextPush, IDENTITY_PROJ,
};
use tracing::{debug, warn};

use super::backdrop_blur::{self, BackdropBlur};
use super::blur_chain::SharedBlurChain;
use super::custom::{CustomAnimPush, CustomResizePush, CustomShaderType};
use super::error::VulkanError;
use super::fence::VkSubmitFence;
use super::renderer::{transition_image, GpuTimerSlot, VulkanRenderer};
use super::types::{GlyphRun, VkFramebuffer, VkTexture};

/// How many consecutive frames must ask for the same intermediate size before an effect counts as
/// settled and gets its full-resolution blur back.
///
/// Not one: frame timing duplicates a frame often enough here (a contended host, a missed vblank)
/// that a single repeat is no evidence of rest, and acting on it flips the intermediate to full
/// resolution and back — two of the most expensive rebuilds there are, in the middle of the
/// animation `MOVING_INTERMEDIATE_CAP` exists to protect. Measured on the five-effect sweep: with a
/// repeat every fourth frame, treating one repeat as rest cost 31 rebuilds against 0, and pushed
/// the pool 273 MB past its budget. It also explained why live rebuild counts moved with host
/// contention when nothing about contention should change how many bundles get built.
///
/// Three frames is ~50 ms at 60 Hz — long enough that a duplicate cannot reach it, short enough
/// that the upgrade lands while the screen is still obviously at rest.
const REST_AFTER_STILL_FRAMES: u8 = 3;

/// Longest axis a blur intermediate may take **while its effect's geometry is moving**, in pixels.
/// A resting effect is never capped. See [`VulkanFrame::capture_backdrop`].
///
/// The intermediate is a resample, so its size is purely a resolution dial, and the on-screen blur
/// radius is held constant across the cap by the radius compensation next to it — what a cap
/// costs is detail in the blurred image, which is what an animation hides best and a still frame
/// shows worst.
///
/// What it buys is the end of the rebuild churn rather than a bigger pool to store it in: every
/// ladder rung above the cap collapses into one, so the expensive top of a sweep stops crossing
/// rungs. Measured on the three-big-windows overview shape: 19 rebuilds per cycle to 0, with the
/// pool holding 41 MB where fitting the uncapped rungs would have taken ~480 MB.
///
/// 921 because it *is* a rung of `backdrop_blur::quantize`'s ladder: the cap is applied before
/// quantization, so a value between rungs would be rounded straight back up and the constant would
/// not mean what it says (960 silently behaved as 1151). It sits a little under half this seat's
/// 2371 px output, which keeps the softening modest; the whole trade is this one number.
const MOVING_INTERMEDIATE_CAP: i32 = 921;

/// The clip a [`ClippedSurfaceRenderElement`](crate::render_helpers::clipped_surface) wants applied
/// to the surface it is about to draw. Set on the frame (via [`VulkanFrame::set_clip_override`])
/// before the inner `WaylandSurfaceRenderElement` draws, so [`VulkanFrame::render_texture_from_to`]
/// — the single sampling entry point Smithay routes that surface through — swaps to the clipped
/// pipeline and folds in the clip. The owned-renderer analogue of GLES's
/// `GlesFrame::override_default_tex_program`. Cleared right after.
///
/// `input_to_geo` maps `v_uv` (0..1 across the quad) to `[0, 1]` geometry space, packed as 3 `vec4`
/// columns (`.xyz` used). It is built by the element from **creation-space** quantities (not the
/// draw-time `dst`), so it is invariant under a `RescaleRenderElement`/`RelocateRenderElement`
/// wrapping the clipped element. `geo_size` is the **logical** geometry size (the rounding
/// coordinate space, matching GLES `ClippedSurfaceRenderElement::compute_uniforms`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct ClipParams {
    pub input_to_geo: [[f32; 4]; 3],
    pub geo_size: [f32; 2],
    pub corner_radius: [f32; 4],
    pub synoik_scale: f32,
}

/// An in-progress render into a bound [`VkFramebuffer`]. Records draws into one command buffer
/// begun in [`VulkanFrame::begin`] and submitted (synchronously, fence-waited) in
/// [`Frame::finish`] / on drop. A mid-frame [`capture_region`](Self::capture_region) flushes that
/// command buffer and swaps in a fresh one (continuing on the LOAD-variant render pass), so `cbuf`
/// is read afresh by every recording method rather than cached.
pub struct VulkanFrame<'frame, 'buffer> {
    renderer: &'frame mut VulkanRenderer,
    fb: &'frame mut VkFramebuffer<'buffer>,
    cbuf: vk::CommandBuffer,
    /// Physical framebuffer size (the raw arg to `render`, == `fb.buffer.extent()`).
    output_size: Size<i32, Physical>,
    /// The frame's output transform (already inverted by `render_elements`, per Smithay).
    transform: Transform,
    /// Logical output size: `output_size` with `transform` applied (w/h swapped for 90/270).
    /// Elements draw in this space; it's what `output_size()` returns (matching GLES) and the
    /// `target` the vertex ortho divides by.
    logical_size: Size<i32, Physical>,
    /// Output-transform 2×2 for the vertex projection (see [`ndc_transform`]), applied to every
    /// draw that targets this frame's output framebuffer. Offscreen passes (blur) stay identity.
    proj: [f32; 4],
    /// Every texture sampled by a recorded draw, cloned (ref-count bump) so its GPU image and
    /// descriptor set outlive command-buffer submission. Draw records reference these resources;
    /// callers (e.g. the render-element loop) routinely drop the source element right after
    /// `draw`, before `finish` submits — without this, the freed image/descriptor would be
    /// sampled by the in-flight GPU work (a use-after-free that segfaults on lavapipe).
    /// `finish` fence-waits, so releasing these when the frame drops (after `finish`) is safe.
    held: Vec<VkTexture>,
    /// A pending clip for the next surface draw (see [`ClipParams`]). While `Some`,
    /// `render_texture_from_to` binds the clipped-surface pipeline instead of the plain texture
    /// one.
    clip_override: Option<ClipParams>,
    /// Physical-framebuffer rects this frame actually touched (every `clear` rect and every draw's
    /// scissor rects). On the present-blit path their union is exactly the age-N damage
    /// `DrmCompositor` rendered into the shadow, so `record_present_blit` copies only these
    /// regions shadow→dmabuf instead of the whole frame — the cycled dmabuf already holds the
    /// rest from its own age-N-ago presentation. Empty (e.g. a clear-only frame that cleared
    /// nothing) falls back to a whole-frame blit. See [`Self::record_present_blit`].
    present_damage: Vec<vk::Rect2D>,
    /// The staging buffer behind the glyph-atlas copies recorded into `cbuf` at `begin`, if this
    /// frame carried any. Owned here so the branch `finish_internal` actually takes decides its
    /// fate: into the in-flight record on the deferred path, dropped after the fence wait on the
    /// synchronous one. Keying that on `fb.offscreen` instead would miss the KMS frame that falls
    /// back to synchronous because no exportable fence could be made.
    glyph_staging: Option<synoik_vk::texture::GlyphStaging>,
    /// The staged texture uploads whose copies this frame's command buffer carries. Held for the
    /// same reason as `glyph_staging`: the GPU reads the staging buffers long after `begin`
    /// returned, so they must survive to the submit's retirement.
    texture_staging: Vec<synoik_vk::texture::StagedTexture>,
    /// Blur chains this frame's command buffer has recorded. Held for the same reason as the two
    /// above, and it is the one that bites hardest: a chain owns the render pass and pipelines the
    /// recording binds, so destroying it before the submit invalidates the *whole* frame, not just
    /// the blur. See [`SharedBlurChain`].
    blur_chains: Vec<Arc<SharedBlurChain>>,
    /// The timestamp pair this frame's command buffer stamped, if GPU timing is on and the ring
    /// had a slot. Carried from `begin` to `finish` because the pair is per-*submit*, and it is
    /// `finish` that decides whether the read happens here (after the fence wait) or at
    /// retirement (when nobody waited).
    gpu_slot: Option<GpuTimerSlot>,
    finished: bool,
    /// Whether this frame's render pass PRESERVED the target's prior contents (LOAD) instead of
    /// discarding them (DONT_CARE) — see the decision in [`Self::begin`].
    ///
    /// Exposed through [`Self::preserves_target`] because the choice cannot be checked from the
    /// pixels: DONT_CARE leaves them *undefined*, and a driver that happens to keep them (this
    /// stack does, for a LINEAR image) makes a broken partial-damage frame read as correct right
    /// up until the one that doesn't. The decision is the contract; assert on it.
    // Only the test reads it, and it stays out of `cfg(test)` deliberately: a seam that exists
    // only under test is a seam that can drift from what ships.
    #[allow(dead_code)]
    preserve: bool,
}

impl fmt::Debug for VulkanFrame<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VulkanFrame")
            .field("output_size", &self.output_size)
            .field("transform", &self.transform)
            .field("finished", &self.finished)
            .finish()
    }
}

/// Warn when a **scanout** frame is about to discard the target instead of loading it.
///
/// This is the silent half of the partial-damage contract. When it fires, the tty backend still
/// redraws only `DrmCompositor`'s buffer-age damage, so everything outside that damage becomes
/// undefined — on screen, the parts the scene has stopped repainting decay into stale content,
/// while every screenshot (a full redraw) comes out clean and nothing is logged. That is precisely
/// how `f3f2f076` hid for a week, and how a 2026-08-15 wedge presented: a compositor submitting a
/// clean 60 Hz onto a frozen screen.
///
/// One discard per swapchain slot is expected and correct — a fresh image is `UNDEFINED` and the
/// age-0 frame redraws it whole — so the first few are quiet. After that a discard means something
/// left a scanout image in a layout the preserve test does not recognise, and the layout is the
/// clue to which pass did it.
fn note_scanout_discard(layout: vk::ImageLayout) {
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Enough for every slot of a swapchain to legitimately discard once on its first use.
    const EXPECTED_COLD_DISCARDS: u64 = 8;
    /// Warn at most once per this many afterwards: a decaying screen discards every frame, and a
    /// per-frame warning would itself cost frames.
    const WARN_EVERY: u64 = 300;

    static SEEN: AtomicU64 = AtomicU64::new(0);
    let n = SEEN.fetch_add(1, Ordering::Relaxed) + 1;
    if n <= EXPECTED_COLD_DISCARDS {
        // Say so anyway, quietly. The budget exists because a slot's first discard is correct, but
        // it is a *lifetime* budget: a swapchain of three slots spends three and leaves five for a
        // pathological discard to hide in later. Then a journal with no discard line reads as "the
        // preserve path held", which is a conclusion the counter had not earned. These lines are a
        // handful at startup and make the silence afterwards mean something.
        debug!(
            seen = n,
            ?layout,
            "scanout frame discarded its target (cold)"
        );
        return;
    }
    if (n - EXPECTED_COLD_DISCARDS - 1).is_multiple_of(WARN_EVERY) {
        warn!(
            total = n,
            ?layout,
            "scanout frame is DISCARDING its target rather than preserving it;              everything outside this frame's damage is now undefined. Expected layout is              TRANSFER_SRC_OPTIMAL. Set SYNOIK_VK_FULL_DAMAGE=1 to take the partial-damage              chain out of the picture"
        );
    }
}

impl<'frame, 'buffer> VulkanFrame<'frame, 'buffer> {
    /// Allocate + begin a command buffer, begin the render pass on `fb`'s GPU framebuffer, and set
    /// the (dynamic) viewport/scissor to the full target — leaving the frame ready to record draws.
    pub(super) fn begin(
        renderer: &'frame mut VulkanRenderer,
        fb: &'frame mut VkFramebuffer<'buffer>,
        output_size: Size<i32, Physical>,
        transform: Transform,
    ) -> Result<Self, VulkanError> {
        let (fb_w, fb_h) = fb.buffer.extent();
        let extent = vk::Extent2D {
            width: fb_w,
            height: fb_h,
        };
        // `Bind<VkTexture>` only produces a `VkFramebuffer` for offscreen textures, so this is
        // Some.
        let framebuffer = fb
            .buffer
            .framebuffer()
            .expect("bound VkFramebuffer wraps an offscreen texture");

        // Preserve the previous frame's contents (render-pass LOAD) instead of discarding them
        // (DONT_CARE) whenever this is a SCANOUT target that already holds a valid prior frame —
        // the basis for damage-preserving (partial-damage) rendering. A caller that redraws only
        // the damage needs everything else to survive the pass, and that is true of both scanout
        // arms:
        //
        // - **Present-blit** (`present.is_some()`): `buffer` is the shadow, a single image reused
        //   across frames, so from its perspective its buffer-age is always 1; DrmCompositor's
        //   per-dmabuf damage (age ≥ 1) is a superset of what it needs preloaded.
        // - **Direct** (`!offscreen`, `present.is_none()`): `buffer` IS the cycled scanout dmabuf,
        //   and it holds exactly its own age-N-ago presentation — which is the frame DrmCompositor
        //   computed this damage against. Preserving is not an approximation here.
        //
        // A fresh image is `UNDEFINED` (nothing to preserve) so it discards, which lines up with
        // the age-0 full-damage frame that redraws it whole; `finish_internal` only records
        // `TRANSFER_SRC_OPTIMAL` after a successful submit, so an errored frame never leaves a
        // "valid" layout over undefined content. The LOAD pass is render-pass-compatible with the
        // base pass (identical attachment/subpass layout), so `framebuffer` (built against the base
        // pass) and every pipeline bind unchanged.
        //
        // An offscreen only preserves when its binder *asked* to
        // (`VulkanRenderer::bind_preserving`, i.e. `OffscreenBuffer`'s persistent, damage-tracked
        // target). A bake target is handed to its caller as a blank canvas, and one that happened
        // to be left in a defined layout would otherwise come back carrying the previous bake.
        // Offscreens end their frame in `SHADER_READ_ONLY_OPTIMAL`, not the `TRANSFER_SRC_OPTIMAL`
        // the continuation pass loads from, so a preserving one is transitioned back below —
        // contents survive any transition out of a defined layout.
        //
        // Getting this wrong is invisible to every test that renders one frame and to every
        // screenshot (both full-damage): the direct arm silently discarded, so the parts of the
        // screen the scene had stopped repainting decayed into whatever the driver left behind.
        // The offscreen arm discarded the same way, and a tiling driver writes back only the tiles
        // a draw touched — so a 32×32 damage rect came back as a 64×64 tile-aligned hole with the
        // redraw in one corner of it, which is what the overview's fade-out group was trailing.
        let preserve = if fb.offscreen {
            fb.preserve
        } else {
            fb.buffer.layout() == vk::ImageLayout::TRANSFER_SRC_OPTIMAL
        };
        if !fb.offscreen && !preserve {
            note_scanout_discard(fb.buffer.layout());
        }
        let render_pass = if preserve {
            renderer.continuation_render_pass
        } else {
            renderer.render_pass
        };

        let render_area = vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent,
        };
        // Free anything the GPU has finished with since the last frame. Polls, never blocks —
        // a blocking drain here would just move the wait we are removing from the end of one
        // frame to the start of the next.
        renderer.retire_completed();

        let cbuf = {
            let dev = &renderer.gpu.device;
            let alloc = vk::CommandBufferAllocateInfo::default()
                .command_pool(renderer.command_pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1);
            let cbuf = unsafe { dev.allocate_command_buffers(&alloc) }?[0];
            let begin_info = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            unsafe { dev.begin_command_buffer(cbuf, &begin_info)? };
            cbuf
        };

        // Fold every deferred client-dmabuf re-acquire barrier into this frame's command buffer,
        // BEFORE the render pass (queue-family/layout barriers must be recorded outside a render
        // pass). They ride this frame's single submit; the wait is the existing `finish()` park —
        // no per-commit standalone submit/fence-wait per animating surface. Done after
        // `begin_command_buffer` succeeds so an earlier failure leaves the queue intact for the
        // next frame; once the frame is constructed its `Drop` always submits
        // (`finish_internal`), so the recorded acquires are never orphaned. See
        // `VulkanRenderer::pending_dmabuf_acquires`.
        // Outside the render pass, before any work: `vkCmdResetQueryPool` is not
        // allowed inside one, and this must precede the acquires so they count.
        // The GPU pair is tagged with the same site the submit will report, so the
        // frame log's `gpu` split and its wait breakdown speak one vocabulary. Both
        // inputs are fixed for the frame's lifetime, so deciding here and at submit
        // time cannot disagree — `submit_site_of` is the single classifier.
        let gpu_slot = renderer.gpu_timer_begin(
            cbuf,
            submit_site_of(fb.offscreen, renderer.finish_is_for_kms()),
        );

        // Kept alive by the frame (`held`) until its submit retires: the barriers above are
        // recorded against these images, and destroying one mid-recording invalidates this whole
        // command buffer. See `record_pending_dmabuf_acquires`.
        let mut acquired = renderer.record_pending_dmabuf_acquires(cbuf);

        // Glyphs shaped since the last frame are still only in host memory; they must reach the
        // atlas before anything samples it, and this is the one place that has to happen (nothing
        // samples the atlas outside a frame's glyph draws). Recorded into *this* frame's command
        // buffer rather than submitted on its own: the copy rides the submit below instead of
        // costing a round trip of its own, which on an idle queue measured ~2-3.5ms and was most
        // of what an uncached widget bake cost. Same slot and same reason as the acquires above.
        let glyph_staging = renderer.record_pending_glyph_uploads(cbuf);

        // Textures imported since the last frame are staged on the host but not yet copied into
        // their images. Same slot and same reason as the two above: the copies ride this frame's
        // submit instead of costing one round trip *and one blocking fence wait* apiece. The
        // staging buffers must outlive this submit, so they travel with the frame exactly as
        // `glyph_staging` does — and so must the images the copies were just recorded against,
        // which join the acquires in `held` for the same reason they are there.
        let (texture_staging, upload_targets) = renderer.record_pending_texture_uploads(cbuf);
        // Every upload this frame has now claimed its staging, so this is the point where "unused
        // for a frame" is decided. Ages the pool and hands back oversized chunks whose client has
        // stopped committing; a client still streaming keeps its mapping, and its warmth.
        renderer.age_staging_pool();
        acquired.extend(upload_targets);

        // Barriers for offscreens created but never rendered into, before anything that samples
        // them — the blurs below, and every draw in this frame. See
        // `VulkanRenderer::make_sampleable`.
        acquired.extend(renderer.record_pending_sampleable(cbuf));

        // A preserving offscreen was left sampleable by its last frame, but the continuation pass
        // loads from `TRANSFER_SRC_OPTIMAL` — hand it over here, in the same command buffer and
        // the same submit as every other pre-pass barrier. (Layout transitions are illegal inside
        // a render pass, so this cannot ride along with the draws.)
        if preserve && fb.offscreen {
            unsafe {
                transition_image(
                    &renderer.gpu.device,
                    cbuf,
                    fb.buffer.image(),
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    vk::AccessFlags::COLOR_ATTACHMENT_READ,
                    vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                );
            }
            fb.buffer.set_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL);
        }

        // Blurs queued while collecting elements (the wallpaper's). Recorded here for the
        // same reason and into the same slot — outside the render pass, riding this frame's
        // submit. Their chains and images must outlive it, exactly as the staging must.
        let (blur_chains, blur_targets) = renderer.record_pending_blurs(cbuf);
        acquired.extend(blur_targets);

        // Everything folded in ahead of the pass is now recorded; close the prepass phase. This
        // mark is unconditional — a frame with nothing queued reports a near-zero prepass rather
        // than merging it into the render pass.
        renderer.gpu_timer_mark(cbuf, gpu_slot, synoik_vk::stats::GpuPhase::Prepass);

        {
            let dev = &renderer.gpu.device;
            // `render_pass` is the DONT_CARE base pass (callers clear explicitly) or, when
            // preserving a valid present-blit shadow, the LOAD continuation pass (see above).
            let pass_begin = vk::RenderPassBeginInfo::default()
                .render_pass(render_pass)
                .framebuffer(framebuffer)
                .render_area(render_area);
            let viewport = vk::Viewport {
                x: 0.0,
                y: 0.0,
                width: extent.width as f32,
                height: extent.height as f32,
                min_depth: 0.0,
                max_depth: 1.0,
            };
            unsafe {
                synoik_vk::stats::render_pass();
                dev.cmd_begin_render_pass(cbuf, &pass_begin, vk::SubpassContents::INLINE);
                dev.cmd_set_viewport(cbuf, 0, std::slice::from_ref(&viewport));
                dev.cmd_set_scissor(cbuf, 0, std::slice::from_ref(&render_area));
            }
        }

        Ok(VulkanFrame {
            renderer,
            fb,
            cbuf,
            output_size,
            transform,
            logical_size: transform.transform_size(output_size),
            proj: ndc_transform(transform),
            held: acquired,
            clip_override: None,
            present_damage: Vec::new(),
            glyph_staging,
            texture_staging,
            blur_chains,
            gpu_slot,
            finished: false,
            preserve,
        })
    }

    /// Whether this frame preserves the target's prior contents rather than discarding them, i.e.
    /// whether a caller that redraws only its damage will land a correct frame. See
    /// [`Self::preserve`].
    #[allow(dead_code)]
    pub(crate) fn preserves_target(&self) -> bool {
        self.preserve
    }

    /// The `target` for the vertex ortho: the **logical** output size in pixels, as `[w, h]`
    /// floats. Elements place geometry in logical space; the ortho divides by this and `proj` then
    /// rotates into the physical framebuffer (whose extent may be w/h-swapped). == the physical
    /// extent for `Transform::Normal`.
    fn target_dims(&self) -> [f32; 2] {
        [self.logical_size.w as f32, self.logical_size.h as f32]
    }

    /// Keep `texture` alive until this frame is dropped (i.e. past `finish`'s fence wait), so a
    /// draw that samples it can't outlive the source element. Cheap: a ref-count bump.
    fn retain(&mut self, texture: &VkTexture) {
        self.held.push(texture.clone());
    }

    /// Physical-framebuffer scissor rects for a draw of element geometry `dst` restricted to its
    /// per-element `damage` (element-local rects, exactly as the damage tracker passes them). Each
    /// rect is clipped to the element, shifted into output space, mapped through the output
    /// transform (like [`Frame::clear`]), and clamped to the framebuffer (Vulkan scissors must be
    /// non-negative and in-bounds). An empty result means the element has no on-target damage this
    /// frame and must not be drawn — the analogue of `GlesFrame`'s per-damage-rect instancing (and
    /// its `damage.is_empty()` early-out). Drawing the full `dst` unscissored instead would repaint
    /// undamaged pixels and, because the damage tracker SKIPS undamaged elements above this one,
    /// erase their LOAD-preserved shadow content — the partial-damage blanking bug.
    fn damage_scissors(
        &mut self,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
    ) -> Vec<vk::Rect2D> {
        let (fb_w, fb_h) = self.fb.buffer.extent();
        let bounds = Rectangle::from_size(dst.size);
        let scissors: Vec<vk::Rect2D> = damage
            .iter()
            .filter_map(|rect| {
                // Clip the element-local damage to the element, then place it in output space.
                let mut r = bounds.intersection(*rect)?;
                r.loc += dst.loc;
                // Output space -> physical framebuffer (identity for `Transform::Normal`).
                let phys = self.transform.transform_rect_in(r, &self.logical_size);
                let x0 = phys.loc.x.max(0);
                let y0 = phys.loc.y.max(0);
                let x1 = (phys.loc.x + phys.size.w).min(fb_w as i32);
                let y1 = (phys.loc.y + phys.size.h).min(fb_h as i32);
                (x1 > x0 && y1 > y0).then(|| vk::Rect2D {
                    offset: vk::Offset2D { x: x0, y: y0 },
                    extent: vk::Extent2D {
                        width: (x1 - x0) as u32,
                        height: (y1 - y0) as u32,
                    },
                })
            })
            .collect();
        // Record what this draw touches so the present-blit can copy only the damaged regions.
        self.present_damage.extend_from_slice(&scissors);
        scissors
    }

    /// Record the 6-vertex quad once per scissor rect, so a draw only touches the damaged pixels
    /// (the rest stay as the LOAD-preserved shadow). Pipeline, descriptor sets and push constants
    /// must already be bound. Leaves the scissor at the last rect: every draw sets its own scissor
    /// first, and `clear` / the present-blit are scissor-independent.
    ///
    /// # Safety
    /// `cbuf` must be in the recording state with a compatible pipeline bound.
    unsafe fn draw_quad(dev: &ash::Device, cbuf: vk::CommandBuffer, scissors: &[vk::Rect2D]) {
        for s in scissors {
            dev.cmd_set_scissor(cbuf, 0, std::slice::from_ref(s));
            dev.cmd_draw(cbuf, 6, 1, 0, 0);
            // The quad covers the element, so the scissor IS the shaded area.
            synoik_vk::stats::draw(
                synoik_vk::stats::DrawSite::Scene,
                u64::from(s.extent.width) * u64::from(s.extent.height),
            );
        }
    }

    /// Arm (or clear) a clip for the next surface draw. `ClippedSurfaceRenderElement`'s Vulkan draw
    /// sets this, draws its inner `WaylandSurfaceRenderElement` (which routes through
    /// `render_texture_from_to`), then clears it. See [`ClipParams`].
    pub(crate) fn set_clip_override(&mut self, clip: Option<ClipParams>) {
        self.clip_override = clip;
    }

    /// Draw `texture` (from `src`) into `dst`, clipped to `clip`'s rounded geometry — the owned-
    /// renderer port of niri's `ClippedSurfaceRenderElement` GLES draw (window rounded corners /
    /// clip-to-geometry). Reached from [`render_texture_from_to`](Frame::render_texture_from_to)
    /// when a clip is armed. Samples through the same `tex_transform` as the plain texture path (so
    /// a partial `src`, buffer rotation/flip, and y-invert are handled), then masks the result.
    ///
    /// `dst` is used only for placement (via the shared quad vertex + `proj`); the clip mask uses
    /// the element-supplied [`ClipParams::input_to_geo`] (built from creation-space geometry), so
    /// it is correct even when an outer `RescaleRenderElement`/`RelocateRenderElement` has
    /// transformed `dst`, and — acting on the quad coordinate, not the sampled UV or the
    /// post-`proj` position — independent of both the buffer transform and the output
    /// transform.
    #[allow(clippy::too_many_arguments)]
    fn render_clipped_texture(
        &mut self,
        texture: &VkTexture,
        src: Rectangle<f64, BufferCoord>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        src_transform: Transform,
        alpha: f32,
        clip: ClipParams,
    ) -> Result<(), VulkanError> {
        let scissors = self.damage_scissors(dst, damage);
        if scissors.is_empty() {
            return Ok(());
        }
        let push = ClippedTexturePush {
            origin: [dst.loc.x as f32, dst.loc.y as f32],
            size: [dst.size.w as f32, dst.size.h as f32],
            proj: self.proj,
            target: self.target_dims(),
            corner_radius: 0.0,
            _pad0: 0.0,
            // clipped_texture.frag multiplies the premultiplied sample by this premultiplied tint,
            // so a uniform `alpha` scales color and coverage together.
            color: [alpha, alpha, alpha, alpha],
            tex_transform: build_tex_transform(src, texture, src_transform),
            geo_size: clip.geo_size,
            _pad1: [0.0, 0.0],
            clip_corner_radius: clip.corner_radius,
            // Built by the element from creation-space geometry, so `dst` (possibly rescaled/
            // relocated by an outer wrapper) is used only for placement, never for the clip.
            input_to_geo: clip.input_to_geo,
            synoik_scale: clip.synoik_scale,
            _pad2: [0.0, 0.0, 0.0],
        };
        self.retain(texture);
        let dev = &self.renderer.gpu.device;
        let pipe = &self.renderer.clipped_texture_pipeline;
        let set = texture.descriptor_set();
        unsafe {
            dev.cmd_bind_pipeline(self.cbuf, vk::PipelineBindPoint::GRAPHICS, pipe.pipeline);
            dev.cmd_bind_descriptor_sets(
                self.cbuf,
                vk::PipelineBindPoint::GRAPHICS,
                pipe.layout,
                0,
                std::slice::from_ref(&set),
                &[],
            );
            dev.cmd_push_constants(
                self.cbuf,
                pipe.layout,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                as_bytes(&push),
            );
            Self::draw_quad(dev, self.cbuf, &scissors);
        }
        Ok(())
    }

    /// The solid-colour half of [`render_clipped_texture`](Self::render_clipped_texture): a flat
    /// fill masked to the same rounded geometry, for a surface whose content is a colour rather
    /// than a texture. Same push block, same clip, no sampler.
    fn render_clipped_solid(
        &mut self,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        color: Color32F,
        clip: ClipParams,
    ) -> Result<(), VulkanError> {
        let scissors = self.damage_scissors(dst, damage);
        if scissors.is_empty() {
            return Ok(());
        }
        let push = ClippedTexturePush {
            origin: [dst.loc.x as f32, dst.loc.y as f32],
            size: [dst.size.w as f32, dst.size.h as f32],
            proj: self.proj,
            target: self.target_dims(),
            corner_radius: 0.0,
            _pad0: 0.0,
            color: color.components(),
            // Unused by `clipped_solid.frag`; the block is shared with the texture path.
            tex_transform: Default::default(),
            geo_size: clip.geo_size,
            _pad1: [0.0, 0.0],
            clip_corner_radius: clip.corner_radius,
            input_to_geo: clip.input_to_geo,
            synoik_scale: clip.synoik_scale,
            _pad2: [0.0, 0.0, 0.0],
        };
        let dev = &self.renderer.gpu.device;
        let pipe = &self.renderer.clipped_solid_pipeline;
        unsafe {
            dev.cmd_bind_pipeline(self.cbuf, vk::PipelineBindPoint::GRAPHICS, pipe.pipeline);
            dev.cmd_push_constants(
                self.cbuf,
                pipe.layout,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                as_bytes(&push),
            );
            Self::draw_quad(dev, self.cbuf, &scissors);
        }
        Ok(())
    }

    /// Draw `texture` into `dst` with its corners rounded by `corner_radius` (physical pixels) —
    /// the owned-renderer equivalent of niri's `RoundedTextureRenderElement` GLES draw. The buffer
    /// `src_transform` (rotation/flip/y-invert) and a partial `src` are baked into the sampling
    /// `tex_transform`.
    ///
    /// Assumes the element's rounding geometry equals `dst` (true for the overview wallpaper, whose
    /// `geometry` is the whole view at the origin); the general `geometry != dst` clip is a later
    /// clipped-surface concern. Called from `RoundedTextureRenderElement`'s Vulkan draw, hence
    /// `pub(crate)`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_rounded_texture(
        &mut self,
        texture: &VkTexture,
        src: Rectangle<f64, BufferCoord>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        src_transform: Transform,
        corner_radius: f32,
        alpha: f32,
    ) -> Result<(), VulkanError> {
        let scissors = self.damage_scissors(dst, damage);
        if scissors.is_empty() {
            return Ok(());
        }
        let push = QuadPush {
            origin: [dst.loc.x as f32, dst.loc.y as f32],
            size: [dst.size.w as f32, dst.size.h as f32],
            proj: self.proj,
            target: self.target_dims(),
            corner_radius,
            // rounded_texture.frag multiplies the premultiplied sample by this premultiplied tint;
            // the SDF coverage then cuts the corners.
            color: [alpha, alpha, alpha, alpha],
            tex_transform: build_tex_transform(src, texture, src_transform),
            ..Default::default()
        };
        self.retain(texture);
        let dev = &self.renderer.gpu.device;
        let pipe = &self.renderer.rounded_texture_pipeline;
        let set = texture.descriptor_set();
        unsafe {
            dev.cmd_bind_pipeline(self.cbuf, vk::PipelineBindPoint::GRAPHICS, pipe.pipeline);
            dev.cmd_bind_descriptor_sets(
                self.cbuf,
                vk::PipelineBindPoint::GRAPHICS,
                pipe.layout,
                0,
                std::slice::from_ref(&set),
                &[],
            );
            dev.cmd_push_constants(
                self.cbuf,
                pipe.layout,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                as_bytes(&push),
            );
            Self::draw_quad(dev, self.cbuf, &scissors);
        }
        Ok(())
    }

    /// Draw a rounded solid-color rectangle filling `dst`, corners cut by `corner_radius` (physical
    /// pixels) with analytic 1px antialiasing — the toolkit's rounded-rect fill primitive (quick-
    /// settings tile/pill/menu backgrounds, calendar highlights). `color` is straight-alpha RGBA;
    /// the box SDF modulates its alpha at the edge/corners, so the corner region blends to whatever
    /// is already in the target (e.g. the menu background this tile sits on).
    ///
    /// The owned-renderer analogue of GLES's `rounding_alpha` chrome fill. Unlike
    /// [`Self::render_rounded_texture`] it samples no texture (no descriptor set): the box-SDF
    /// `sdf_rect.frag` reads only `origin`/`size`/`corner_radius`/`color` from the shared
    /// [`QuadPush`]. `dst` places the quad; `damage` scopes the draw like every other material.
    pub(crate) fn render_rounded_rect(
        &mut self,
        color: [f32; 4],
        corner_radius: f32,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
    ) -> Result<(), VulkanError> {
        self.render_rounded_rect_impl(color, corner_radius, 0.0, dst, damage, (0., 0.))
    }

    /// [`render_rounded_rect`](Self::render_rounded_rect) with a horizontal alpha ramp: full
    /// `color` at `ramp.0`, transparent at `ramp.1`, both in 0..1 across `dst`'s width and in
    /// either order. This is GNOME's `background-gradient-direction: horizontal` for the case
    /// the two stops share an RGB and differ only in alpha — the app grid's page-preview hints
    /// (`.page-navigation-hint`, `_app-grid.scss:150-170`).
    pub(crate) fn render_rounded_rect_faded(
        &mut self,
        color: [f32; 4],
        corner_radius: f32,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        ramp: (f32, f32),
    ) -> Result<(), VulkanError> {
        self.render_rounded_rect_impl(color, corner_radius, 0.0, dst, damage, ramp)
    }

    /// Stroke a rounded rectangle: an inset ring of `stroke_width` physical px hugging the inside
    /// of `dst`'s edge, corners cut by `corner_radius` (inner corners concentric). The stroke path
    /// of the same SDF material as [`render_rounded_rect`] — a focus ring, a 1px outline.
    pub(crate) fn stroke_rounded_rect(
        &mut self,
        color: [f32; 4],
        corner_radius: f32,
        stroke_width: f32,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
    ) -> Result<(), VulkanError> {
        self.render_rounded_rect_impl(
            color,
            corner_radius,
            stroke_width.max(0.),
            dst,
            damage,
            (0., 0.),
        )
    }

    /// Fill an isoceles triangle inscribed in `dst`: its base spans one edge and its apex is the
    /// midpoint of the opposite edge, `side` naming the edge the apex points at (`St.Side` order:
    /// 0 TOP, 1 RIGHT, 2 BOTTOM, 3 LEFT). Analytic 1px antialiasing, like the rounded-rect SDF.
    ///
    /// GNOME's `SwitcherPopup.drawArrow` (`js/ui/switcherPopup.js:661-704`) — the app switcher's
    /// multi-window chevron and the switcher list's scroll arrows. `color` is straight-alpha.
    pub(crate) fn render_triangle(
        &mut self,
        color: [f32; 4],
        side: u8,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
    ) -> Result<(), VulkanError> {
        let scissors = self.damage_scissors(dst, damage);
        if scissors.is_empty() {
            return Ok(());
        }
        let push = QuadPush {
            origin: [dst.loc.x as f32, dst.loc.y as f32],
            size: [dst.size.w as f32, dst.size.h as f32],
            proj: self.proj,
            target: self.target_dims(),
            color: premultiply(color),
            // `sdf_triangle.frag` reads the apex side at the shared block's `cutoff` offset.
            cutoff: [f32::from(side), 0.],
            ..Default::default()
        };
        let dev = &self.renderer.gpu.device;
        let pipe = &self.renderer.sdf_triangle_pipeline;
        unsafe {
            dev.cmd_bind_pipeline(self.cbuf, vk::PipelineBindPoint::GRAPHICS, pipe.pipeline);
            dev.cmd_push_constants(
                self.cbuf,
                pipe.layout,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                as_bytes(&push),
            );
            Self::draw_quad(dev, self.cbuf, &scissors);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn render_rounded_rect_impl(
        &mut self,
        color: [f32; 4],
        corner_radius: f32,
        stroke_width: f32,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        ramp: (f32, f32),
    ) -> Result<(), VulkanError> {
        let scissors = self.damage_scissors(dst, damage);
        if scissors.is_empty() {
            return Ok(());
        }
        let push = QuadPush {
            origin: [dst.loc.x as f32, dst.loc.y as f32],
            size: [dst.size.w as f32, dst.size.h as f32],
            proj: self.proj,
            target: self.target_dims(),
            corner_radius,
            stroke_width,
            // The toolkit states colors straight-alpha (as the SCSS does); the renderer is
            // premultiplied. This is that boundary.
            color: premultiply(color),
            // `sdf_rect.frag` reads this at the shared block's `cutoff` offset.
            cutoff: [ramp.0, ramp.1],
            ..Default::default()
        };
        let dev = &self.renderer.gpu.device;
        let pipe = &self.renderer.sdf_rect_pipeline;
        unsafe {
            dev.cmd_bind_pipeline(self.cbuf, vk::PipelineBindPoint::GRAPHICS, pipe.pipeline);
            dev.cmd_push_constants(
                self.cbuf,
                pipe.layout,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                as_bytes(&push),
            );
            Self::draw_quad(dev, self.cbuf, &scissors);
        }
        Ok(())
    }

    /// Draw `texture` into `dst` with a horizontal alpha fade over `cutoff` (`[left, right]` in the
    /// sampled texture's u coordinate; `left >= right` disables it) — the owned-renderer equivalent
    /// of niri's `GradientFadeTextureRenderElement` (the MRU switcher fades clipped thumbnails).
    /// The buffer `src_transform` and a partial `src` are baked into the sampling
    /// `tex_transform`; the fade band follows the sampled u axis, matching the GLES shader.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render_gradient_fade(
        &mut self,
        texture: &VkTexture,
        src: Rectangle<f64, BufferCoord>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        src_transform: Transform,
        cutoff: (f32, f32),
        alpha: f32,
    ) -> Result<(), VulkanError> {
        let scissors = self.damage_scissors(dst, damage);
        if scissors.is_empty() {
            return Ok(());
        }
        let push = QuadPush {
            origin: [dst.loc.x as f32, dst.loc.y as f32],
            size: [dst.size.w as f32, dst.size.h as f32],
            proj: self.proj,
            target: self.target_dims(),
            // Premultiplied tint (see `render_texture_from_to`).
            color: [alpha, alpha, alpha, alpha],
            tex_transform: build_tex_transform(src, texture, src_transform),
            cutoff: [cutoff.0, cutoff.1],
            ..Default::default()
        };
        self.retain(texture);
        let dev = &self.renderer.gpu.device;
        let pipe = &self.renderer.gradient_fade_pipeline;
        let set = texture.descriptor_set();
        unsafe {
            dev.cmd_bind_pipeline(self.cbuf, vk::PipelineBindPoint::GRAPHICS, pipe.pipeline);
            dev.cmd_bind_descriptor_sets(
                self.cbuf,
                vk::PipelineBindPoint::GRAPHICS,
                pipe.layout,
                0,
                std::slice::from_ref(&set),
                &[],
            );
            dev.cmd_push_constants(
                self.cbuf,
                pipe.layout,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                as_bytes(&push),
            );
            Self::draw_quad(dev, self.cbuf, &scissors);
        }
        Ok(())
    }

    /// Draw a shaped glyph `run` with its top-left run origin at `origin` (physical pixels), tinted
    /// `color` (straight-alpha; the atlas coverage modulates the alpha). Each glyph is one quad
    /// sampling its slot in the run's R8 coverage atlas.
    ///
    /// **Offscreen-only**: `text.vert` has no output-transform `proj`, so this is correct only on
    /// an identity-transform target (a UI-chrome offscreen built by the draw layer); that
    /// offscreen is then composited through the transform-aware texture material. `dst` is the
    /// run's geometry, used purely to derive the damage scissors (the glyphs place themselves
    /// via `origin`).
    // Non-test dead until the panel draw-layer (increment 3) calls it; exercised now by the
    // `vulkan_render_glyphs_rasterizes_coverage` test.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn render_glyphs(
        &mut self,
        run: &GlyphRun,
        origin: Point<i32, Physical>,
        color: [f32; 4],
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
    ) -> Result<(), VulkanError> {
        self.render_glyphs_with(run, origin, dst, damage, |_| color)
    }

    /// Like [`Self::render_glyphs`], but each glyph is tinted by the colour of its source span:
    /// `colors[span_index]`, falling back to white for an out-of-range index. Lets one shaped run
    /// carry multiple colours (e.g. the MRU scope panel's selected-vs-unselected words) in a single
    /// draw. See [`GlyphRun::spans`](super::types::GlyphRun).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn render_glyphs_spans(
        &mut self,
        run: &GlyphRun,
        origin: Point<i32, Physical>,
        colors: &[[f32; 4]],
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
    ) -> Result<(), VulkanError> {
        let per_glyph: Vec<[f32; 4]> = run
            .spans()
            .iter()
            .map(|&s| colors.get(s as usize).copied().unwrap_or([1.; 4]))
            .collect();
        self.render_glyphs_with(run, origin, dst, damage, |i| {
            per_glyph.get(i).copied().unwrap_or([1.; 4])
        })
    }

    /// Shared glyph-draw loop: `color_for(glyph_index)` gives each glyph's tint.
    fn render_glyphs_with(
        &mut self,
        run: &GlyphRun,
        origin: Point<i32, Physical>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        color_for: impl Fn(usize) -> [f32; 4],
    ) -> Result<(), VulkanError> {
        // Every glyph this could sample was uploaded by the flush at `begin`, and nothing can have
        // queued more since — shaping needs `&mut VulkanRenderer`, which this frame holds. That is
        // a property of the borrow rather than of any type, so assert it: a future frame method
        // that shaped text mid-frame would draw blanks, silently, in release.
        debug_assert!(
            !self.renderer.has_pending_glyphs(),
            "glyphs were queued after this frame began, so they are not in the atlas it samples"
        );
        debug_assert!(
            matches!(self.transform, Transform::Normal),
            "render_glyphs is offscreen-only (identity transform), got {:?}",
            self.transform,
        );
        let scissors = self.damage_scissors(dst, damage);
        if scissors.is_empty() || run.glyphs().is_empty() {
            return Ok(());
        }
        let target = self.target_dims();
        let side = run.side() as f32;
        self.retain(run.atlas());
        let dev = &self.renderer.gpu.device;
        let pipe = &self.renderer.text_pipeline;
        let set = run.atlas().descriptor_set();
        unsafe {
            dev.cmd_bind_pipeline(self.cbuf, vk::PipelineBindPoint::GRAPHICS, pipe.pipeline);
            dev.cmd_bind_descriptor_sets(
                self.cbuf,
                vk::PipelineBindPoint::GRAPHICS,
                pipe.layout,
                0,
                std::slice::from_ref(&set),
                &[],
            );
            for (i, g) in run.glyphs().iter().enumerate() {
                let push = TextPush {
                    origin: [(origin.x + g.x) as f32, (origin.y + g.y) as f32],
                    size: [g.w as f32, g.h as f32],
                    target,
                    uv_origin: [g.atlas_x as f32 / side, g.atlas_y as f32 / side],
                    uv_size: [g.w as f32 / side, g.h as f32 / side],
                    _pad: [0.0, 0.0],
                    // Toolkit colors are straight-alpha; the glyph material is premultiplied.
                    color: premultiply(color_for(i)),
                };
                dev.cmd_push_constants(
                    self.cbuf,
                    pipe.layout,
                    vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                    0,
                    as_bytes(&push),
                );
                // One draw per damaged scissor rect: a glyph straddling two damage regions draws in
                // each, matching the per-rect instancing the other materials do via `draw_quad`.
                for s in &scissors {
                    dev.cmd_set_scissor(self.cbuf, 0, std::slice::from_ref(s));
                    dev.cmd_draw(self.cbuf, 6, 1, 0, 0);
                    // A glyph quad is far smaller than its scissor (which spans the whole run), so
                    // the glyph's own area is what gets shaded.
                    synoik_vk::stats::draw(
                        synoik_vk::stats::DrawSite::Text,
                        u64::from(g.w) * u64::from(g.h),
                    );
                }
            }
        }
        Ok(())
    }

    /// Draw a border ring — the owned-renderer equivalent of niri's `BorderRenderElement` (an
    /// angled gradient clipped to a rounded-rect ring). The caller (the element's Vulkan draw)
    /// fills every material field of `push` plus `origin`/`size` from `dst`; this sets `target`,
    /// binds the premultiplied-blend border pipeline (no texture), and draws the quad.
    pub(crate) fn render_border(
        &mut self,
        mut push: BorderPush,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
    ) -> Result<(), VulkanError> {
        let scissors = self.damage_scissors(dst, damage);
        if scissors.is_empty() {
            return Ok(());
        }
        push.proj = self.proj;
        push.target = self.target_dims();
        let dev = &self.renderer.gpu.device;
        let pipe = &self.renderer.border_pipeline;
        unsafe {
            dev.cmd_bind_pipeline(self.cbuf, vk::PipelineBindPoint::GRAPHICS, pipe.pipeline);
            dev.cmd_push_constants(
                self.cbuf,
                pipe.layout,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                as_bytes(&push),
            );
            Self::draw_quad(dev, self.cbuf, &scissors);
        }
        Ok(())
    }

    /// Draw a rounded-rectangle drop shadow — the owned-renderer equivalent of niri's
    /// `ShadowRenderElement` (a gaussian-blurred rounded rect with an optional window cutout).
    /// Like [`Self::render_border`], the caller fills the material fields plus `origin`/`size`;
    /// this sets `target`, binds the premultiplied-blend shadow pipeline (no texture), and draws.
    pub(crate) fn render_shadow(
        &mut self,
        mut push: ShadowPush,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
    ) -> Result<(), VulkanError> {
        let scissors = self.damage_scissors(dst, damage);
        if scissors.is_empty() {
            return Ok(());
        }
        push.proj = self.proj;
        push.target = self.target_dims();
        let dev = &self.renderer.gpu.device;
        let pipe = &self.renderer.shadow_pipeline;
        unsafe {
            dev.cmd_bind_pipeline(self.cbuf, vk::PipelineBindPoint::GRAPHICS, pipe.pipeline);
            dev.cmd_push_constants(
                self.cbuf,
                pipe.layout,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                as_bytes(&push),
            );
            Self::draw_quad(dev, self.cbuf, &scissors);
        }
        Ok(())
    }

    /// Toolkit convenience over [`Self::render_shadow`]: a simple rounded-rect drop shadow (no
    /// window cutout). `box_dst` is the casting rect (with any shadow offset already applied); the
    /// gaussian field is evaluated over `box_dst` outset by the blur bleed (~3·`sigma`), computed
    /// here so the fringe always has room. `color` is straight-alpha (premultiplied here for the
    /// premultiplied-over blend); `sigma` and radii are physical px. Used by
    /// [`Painter::drop_shadow`](crate::ui::widget::Painter::drop_shadow) for GNOME `box-shadow`.
    pub(crate) fn render_drop_shadow(
        &mut self,
        color: [f32; 4],
        corner_radius: f32,
        sigma: f32,
        synoik_scale: f32,
        box_dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
    ) -> Result<(), VulkanError> {
        let margin = (sigma * 3.0).ceil() as i32;
        let area = Rectangle::new(
            Point::from((box_dst.loc.x - margin, box_dst.loc.y - margin)),
            Size::from((box_dst.size.w + margin * 2, box_dst.size.h + margin * 2)),
        );
        let premul = [
            color[0] * color[3],
            color[1] * color[3],
            color[2] * color[3],
            color[3],
        ];
        let push = ShadowPush {
            origin: [area.loc.x as f32, area.loc.y as f32],
            size: [area.size.w as f32, area.size.h as f32],
            proj: IDENTITY_PROJ,
            target: [0.0, 0.0],
            sigma,
            synoik_scale,
            shadow_color: premul,
            corner_radius: [corner_radius; 4],
            window_corner_radius: [0.0; 4],
            area_size: [area.size.w as f32, area.size.h as f32],
            geo_loc: [
                (box_dst.loc.x - area.loc.x) as f32,
                (box_dst.loc.y - area.loc.y) as f32,
            ],
            geo_size: [box_dst.size.w as f32, box_dst.size.h as f32],
            window_geo_loc: [0.0, 0.0],
            window_geo_size: [0.0, 0.0],
            synoik_alpha: 1.0,
            _pad0: 0.0,
        };
        // `damage` arrives local to `box_dst` — the box the caller named — but the draw, and so
        // the scissor, is `area`: that box grown by the blur margin. Grow the damage to match, or
        // the fringe (the entire point of a shadow) is scissored away by exactly the margin.
        let damage: Vec<_> = damage
            .iter()
            .map(|r| {
                Rectangle::new(
                    r.loc,
                    Size::from((r.size.w + margin * 2, r.size.h + margin * 2)),
                )
            })
            .collect();
        self.render_shadow(push, area, &damage)
    }

    /// Draw the postprocess-and-clip material: sample `texture` (from `src`) into `dst`, applying
    /// the saturation / noise / premultiplied-bg + general rounded-corner clip carried by `push`.
    /// The caller fills the material fields (`geo_size`, `corner_radius`, `bg_color`,
    /// `input_to_geo`, `synoik_scale`, `synoik_alpha`, `saturation`, `noise`); this fills the
    /// placement (`origin`/`size`/`target`/`src_rect`), binds the premultiplied-blend
    /// postprocess pipeline + the texture's descriptor set, and draws the quad. The
    /// owned-renderer equivalent of niri's clipped-surface / framebuffer-effect postprocess
    /// shader. Same unflipped scope as the other sampling materials.
    // Consumed by the live ClippedSurfaceRenderElement / FramebufferEffectElement wiring (Stage 3);
    // exercised now by the offscreen material test.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn render_postprocess(
        &mut self,
        texture: &VkTexture,
        src: Rectangle<f64, BufferCoord>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        mut push: PostprocessPush,
    ) -> Result<(), VulkanError> {
        if texture.flipped() {
            tracing::warn!(
                "VulkanFrame::render_postprocess: flipped textures unsupported; skipping"
            );
            return Ok(());
        }

        let scissors = self.damage_scissors(dst, damage);
        if scissors.is_empty() {
            return Ok(());
        }

        push.origin = [dst.loc.x as f32, dst.loc.y as f32];
        push.size = [dst.size.w as f32, dst.size.h as f32];
        push.proj = self.proj;
        push.target = self.target_dims();
        push.src_rect = normalized_src(src, texture);

        self.retain(texture);
        let dev = &self.renderer.gpu.device;
        let pipe = &self.renderer.postprocess_pipeline;
        let set = texture.descriptor_set();
        unsafe {
            dev.cmd_bind_pipeline(self.cbuf, vk::PipelineBindPoint::GRAPHICS, pipe.pipeline);
            dev.cmd_bind_descriptor_sets(
                self.cbuf,
                vk::PipelineBindPoint::GRAPHICS,
                pipe.layout,
                0,
                std::slice::from_ref(&set),
                &[],
            );
            dev.cmd_push_constants(
                self.cbuf,
                pipe.layout,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                as_bytes(&push),
            );
            Self::draw_quad(dev, self.cbuf, &scissors);
        }
        Ok(())
    }

    /// Capture the scene rendered so far into `dest` and continue the frame on top of it — the
    /// owned-renderer equivalent of niri's GLES `FramebufferEffectElement::capture_framebuffer`
    /// (a `glBlitFramebuffer` from the draw framebuffer into an intermediate). Used mid-frame by a
    /// framebuffer effect (backdrop blur) to grab the backdrop before compositing over it.
    ///
    /// A render pass can't be a transfer source while it's active, so this **ends** the in-progress
    /// pass (leaving the target in `TRANSFER_SRC_OPTIMAL`), scaled-blits the `src_region` sub-rect
    /// of the target into the whole of `dest` (`LINEAR`, mirroring the GLES blit — the size may
    /// differ, e.g. the overview zoom trick), leaves `dest` in `SHADER_READ_ONLY_OPTIMAL`, and
    /// re-opens the LOAD-variant
    /// [`continuation pass`](VulkanRenderer::continuation_render_pass) so the preserved scene can
    /// be drawn over. `dest` must be a `SAMPLED | TRANSFER_DST` offscreen (i.e. from
    /// [`Offscreen::create_buffer`]); its whole extent is filled.
    ///
    /// **`record_gap` is where work that needs the capture belongs.** Between the two passes the
    /// command buffer is outside any render pass, which is the one slot a blur chain (or any other
    /// copy/barrier/pass sequence) can be recorded into — and it rides the frame's own submit, so
    /// it costs nothing. This used to end the command buffer here and submit + fence-wait it, for
    /// exactly one reason: the caller's blur ran on a submission of its own and had to see a
    /// finished capture. Recording it in the gap instead is what removed both the flush and the
    /// blur's submit; the `to_read` barrier below is what orders the two now, and it orders them
    /// inside one command buffer, which is stronger than what the flush bought.
    ///
    /// The invariant that replaces the flush: **whatever consumes `dest` must be recorded into
    /// this frame** (in `record_gap`, or in a later draw), not submitted separately before it. A
    /// separate submit made before this frame's would read an unwritten capture.
    // Consumed by the live FramebufferEffectElement wiring (Stage 3); exercised now by the
    // render-pass-split test.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn capture_region(
        &mut self,
        src_region: Rectangle<i32, Physical>,
        dest: &VkTexture,
        record_gap: impl FnOnce(vk::CommandBuffer),
    ) -> Result<(), VulkanError> {
        let (fb_w, fb_h) = self.fb.buffer.extent();
        // Clamp the blit source to the target bounds (a framebuffer effect near an edge can have a
        // geometry that spills off-screen).
        let sx0 = src_region.loc.x.clamp(0, fb_w as i32);
        let sy0 = src_region.loc.y.clamp(0, fb_h as i32);
        let sx1 = (src_region.loc.x + src_region.size.w).clamp(0, fb_w as i32);
        let sy1 = (src_region.loc.y + src_region.size.h).clamp(0, fb_h as i32);
        let (d_w, d_h) = dest.extent();

        // A fully off-screen effect clamps to an empty source rect (and a zero-size `dest` is
        // degenerate): there is nothing to capture, so skip the split entirely — leaving the frame
        // on its current pass and `dest` untouched. Mirrors GLES `capture_framebuffer` returning
        // early on an empty clamp (its `draw` then finds no intermediate and composites nothing).
        if sx1 <= sx0 || sy1 <= sy0 || d_w == 0 || d_h == 0 {
            return Ok(());
        }

        let src_image = self.fb.buffer.image();
        let dest_image = dest.image();
        let framebuffer = self
            .fb
            .buffer
            .framebuffer()
            .expect("bound VkFramebuffer wraps an offscreen texture");
        let continuation = self.renderer.continuation_render_pass;
        let extent = vk::Extent2D {
            width: fb_w,
            height: fb_h,
        };
        let render_area = vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent,
        };
        let viewport = vk::Viewport {
            x: 0.0,
            y: 0.0,
            width: fb_w as f32,
            height: fb_h as f32,
            min_depth: 0.0,
            max_depth: 1.0,
        };
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .base_mip_level(0)
            .level_count(1)
            .base_array_layer(0)
            .layer_count(1);
        let layers = vk::ImageSubresourceLayers::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .mip_level(0)
            .base_array_layer(0)
            .layer_count(1);

        let cbuf = self.cbuf;
        let dev = &self.renderer.gpu.device;
        unsafe {
            dev.cmd_end_render_pass(cbuf);

            // Capture destination: contents are fully overwritten by the blit, so discard from
            // UNDEFINED. (Reused across frames — safe because `finish` fence-waits, so the previous
            // frame's sampling of `dest` has completed before this frame records.)
            let to_dst = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(dest_image)
                .subresource_range(range);
            synoik_vk::stats::barriers(1);
            dev.cmd_pipeline_barrier(
                cbuf,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                std::slice::from_ref(&to_dst),
            );

            // Scaled blit of the source sub-region into the whole destination. The ended pass's
            // subpass→EXTERNAL dependency already made its color writes available to this
            // TRANSFER_READ, so no extra barrier is needed on the source.
            let blit = vk::ImageBlit::default()
                .src_subresource(layers)
                .src_offsets([
                    vk::Offset3D {
                        x: sx0,
                        y: sy0,
                        z: 0,
                    },
                    vk::Offset3D {
                        x: sx1,
                        y: sy1,
                        z: 1,
                    },
                ])
                .dst_subresource(layers)
                .dst_offsets([
                    vk::Offset3D { x: 0, y: 0, z: 0 },
                    vk::Offset3D {
                        x: d_w as i32,
                        y: d_h as i32,
                        z: 1,
                    },
                ]);
            synoik_vk::stats::blit(
                synoik_vk::stats::BlitSite::Capture,
                u64::from(d_w) * u64::from(d_h),
            );
            dev.cmd_blit_image(
                cbuf,
                src_image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                dest_image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                std::slice::from_ref(&blit),
                vk::Filter::LINEAR,
            );

            // Make the capture sampleable for the caller's blur/postprocess.
            let to_read = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(dest_image)
                .subresource_range(range);
            synoik_vk::stats::barriers(1);
            dev.cmd_pipeline_barrier(
                cbuf,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                std::slice::from_ref(&to_read),
            );

            // Whatever needs the capture goes here, outside any render pass and inside the
            // frame's own command buffer: a blur chain records its passes and its copy-out, and
            // the barrier above orders them after the blit. No submit, no wait, no second command
            // buffer — this is the gap the doc comment is about.
            record_gap(cbuf);

            // Re-open on the LOAD-variant continuation pass so the preserved scene can be drawn
            // over. Same command buffer: ending a pass and beginning another is ordinary
            // recording, and the split into two submits only ever existed to make a
            // separately-submitted blur see a finished capture.
            let pass_begin = vk::RenderPassBeginInfo::default()
                .render_pass(continuation)
                .framebuffer(framebuffer)
                .render_area(render_area);
            synoik_vk::stats::render_pass();
            dev.cmd_begin_render_pass(cbuf, &pass_begin, vk::SubpassContents::INLINE);
            dev.cmd_set_viewport(cbuf, 0, std::slice::from_ref(&viewport));
            dev.cmd_set_scissor(cbuf, 0, std::slice::from_ref(&render_area));
        };

        // The ended base pass left the target in TRANSFER_SRC; the blit left `dest` sampleable.
        // (The continuation pass restores the target to TRANSFER_SRC again at `finish`.)
        self.fb
            .buffer
            .set_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL);
        dest.set_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        Ok(())
    }

    /// The **logical** output size (w/h swapped for 90/270), the space elements draw in — mirrors
    /// `GlesFrame::output_size`, which returns the transform-swapped size, not the physical extent.
    /// The framebuffer-effect clamp math depends on this being logical.
    // Returns `logical_size`, not the like-named `output_size` field — that's the whole point (see
    // the doc above), so the misnamed-getter lint is a false positive here.
    #[allow(clippy::misnamed_getters)]
    pub(crate) fn output_size(&self) -> Size<i32, Physical> {
        self.logical_size
    }

    /// The frame's (already-inverted, per `render_elements`) output transform — mirrors
    /// `GlesFrame::transformation`, used by the framebuffer-effect geometry mapping.
    pub(crate) fn transform(&self) -> Transform {
        self.transform
    }

    /// Capture the backdrop into `slot`'s cache and blur it (when enabled) — the orchestration
    /// behind `FramebufferEffectElement`'s Vulkan `capture_framebuffer`. (Re)builds the cached
    /// [`BackdropBlur`] when the intermediate `size`/`passes` change (kept across frames otherwise
    /// — per-frame allocation is a host round trip on Venus), captures `src_region` of
    /// the target into it
    /// via [`Self::capture_region`], then records the blur. The element then composites
    /// [`BackdropBlur::intermediate`] with [`Self::render_postprocess`] in its `draw`.
    pub(crate) fn capture_backdrop(
        &mut self,
        slot: &mut Option<BackdropBlur>,
        src_region: Rectangle<i32, Physical>,
        size: Size<i32, BufferCoord>,
        radius: Option<f64>,
    ) -> Result<(), VulkanError> {
        // A near-fully-clipped effect (edge, or deep overview zoom) can round the intermediate size
        // to zero. `vkCreateImage` with a 0 extent is invalid usage, and a zero-region
        // `capture_region` would skip the blit and leave `capture` UNDEFINED (then sampled as
        // SHADER_READ) — so bail before allocating or capturing. There is nothing to composite;
        // `draw` clamps to the same degenerate `dst`, so it contributes ~nothing. (A reused cache
        // keeps last frame's content untouched — we skip capture AND blur, so it stays consistent.)
        if size.w <= 0 || size.h <= 0 {
            return Ok(());
        }

        // Give the intermediate sizing some slack so an animating geometry stops rebuilding the
        // cache every frame — see `backdrop_blur::quantize` for why this is free of seams and why
        // it is a ladder and not a hysteresis band. Only when blur is on: the unblurred path
        // composites the capture directly, where an upsample-then-downsample through the slack is a
        // visible softening of otherwise crisp backdrop (text, most obviously), and there is no
        // chain to rebuild anyway — the capture texture alone is a fraction of the cost.
        //
        // That leaves a residual: a *visible but unblurred* effect (`is_visible()` is also true for
        // noise or saturation alone) under animating geometry still allocates one capture texture
        // per frame. Reachable only through a window rule that asks for noise/saturation with
        // `blur: false` — no default path builds one — so it is a
        // known gap, not an oversight. Quantizing it is the wrong trade: the raw capture is a
        // resample of the framebuffer, so slack would upsample and then downsample it, and that
        // path exists precisely to leave the backdrop crisp.
        // Is this effect's geometry *moving*? A need that differs from the previous frame's is an
        // animation in progress. That is the only state in which the intermediate is capped below,
        // and the reason is perceptual: a blur computed at a lower resolution is a wider, softer
        // one, which is hard to see on something that is moving and easy to see on something that
        // is not. Resting effects keep today's full-resolution look exactly.
        let need = (size.w, size.h);
        let still_for = match slot.as_ref().map(|b| (b.last_need(), b.still_for())) {
            Some((last, n)) if last == need => n.saturating_add(1),
            _ => 0,
        };
        let moving = still_for < REST_AFTER_STILL_FRAMES;

        let (size, radius) = match radius {
            Some(radius) => {
                // While moving, cap the intermediate's long axis, preserving aspect so the blur
                // stays as isotropic as the ladder's own slack leaves it. This is what actually
                // ends the rebuild churn rather than storing it: every rung above the cap collapses
                // into one, so the expensive top of a sweep stops crossing rungs at all — measured
                // on the three-big-windows shape as 19 rebuilds per cycle down to 0, with the pool
                // holding 41 MB instead of 171 MB. Fitting those rungs in the pool instead would
                // have taken ~480 MB.
                let long = size.w.max(size.h);
                let size = if moving && long > MOVING_INTERMEDIATE_CAP {
                    Size::from((
                        (size.w * MOVING_INTERMEDIATE_CAP / long).max(1),
                        (size.h * MOVING_INTERMEDIATE_CAP / long).max(1),
                    ))
                } else {
                    size
                };
                let quantized = Size::from((
                    backdrop_blur::quantize(size.w),
                    backdrop_blur::quantize(size.h),
                ));
                // The radius is in texels of the intermediate, so a higher-resolution
                // intermediate is a proportionally *smaller* radius on screen. Scale the radius
                // back up by how much we overshot, or the blur would visibly thin at every rung
                // crossing. One scalar, not one per axis: the two overshoots differ by at most
                // 1.25:1, and that much anisotropy in a blur is not something you can see.
                //
                // Measured against the *pre-cap* need, so the cap above changes resolution and
                // nothing else. The radius is in texels of the intermediate, and the on-screen
                // radius is that times (screen / intermediate) — so capping the
                // intermediate by `k` and scaling the radius by the same `k` leaves the on-screen
                // radius exactly where it was. Comparing against the capped size instead would
                // leave the blur `1/k` times wider while moving, and it would visibly snap back
                // the moment the animation settled.
                let ratio = (f64::from(quantized.w) / f64::from(need.0))
                    * (f64::from(quantized.h) / f64::from(need.1));
                (quantized, Some(radius * ratio.sqrt()))
            }
            None => (size, None),
        };

        let dims = (size.w as u32, size.h as u32);
        // GNOME's cascade sizes the pyramid to the radius, so how many rungs the chain needs is
        // only knowable once the ladder has picked `dims` — which is also exactly the point at
        // which the radius has been rescaled into that intermediate's texels. `.max(1)`: the
        // horizontal pass needs a same-sized twin to land in, and only the shrinking levels have
        // one.
        let passes = radius.map(|r| synoik_vk::blur::downscale_levels(dims.0, dims.1, r).max(1));
        let reuse = slot.as_ref().is_some_and(|b| b.matches(dims, passes));
        if !reuse {
            // The rung we are leaving goes to the pool rather than to `vkDestroy*`. An animated
            // geometry is almost always a *cyclic* sweep — the overview shrinks each intermediate
            // down through a set of rungs and grows it back through the same ones — so the bundle
            // being evicted here is, more often than not, one this same effect will ask for again
            // within the second. See `VulkanRenderer::backdrop_blur_pool`.
            if let Some(evicted) = slot.take() {
                self.renderer.recycle_backdrop_blur(evicted);
            }
            *slot = match self.renderer.take_backdrop_blur(dims, passes) {
                Some(pooled) => Some(pooled),
                None => {
                    #[cfg(test)]
                    self.renderer.note_backdrop_blur_alloc();
                    Some(BackdropBlur::new(self.renderer, size, passes)?)
                }
            };
        }
        // Every frame, including on a bundle just taken from the pool, whose stored values belong
        // to whichever effect used it last.
        slot.as_mut()
            .expect("just populated")
            .set_stillness(need, still_for);
        let bb = slot.as_ref().expect("just populated");
        // The blur rides the capture's own command buffer, recorded in the gap between the two
        // render passes — no submit of its own, and no flush to make one visible to it.
        self.capture_region(src_region, bb.capture(), |cbuf| {
            if let Some(radius) = radius {
                bb.record_blur(cbuf, radius);
            }
        })?;
        // The chain's images and the output are read by a submit that has not happened yet (this
        // frame's), so they must outlive it exactly as a queued upload's destination does. The
        // textures go in `held`, the chain in `blur_chains` — and holding that `Arc` until this
        // frame's submit retires is the whole of what makes `SharedBlurChain::drop` safe without a
        // wait, so it is not optional bookkeeping.
        let (capture, intermediate) = (bb.capture().clone(), bb.intermediate().clone());
        self.held.push(capture);
        self.held.push(intermediate);
        self.blur_chains.extend(bb.chain());
        Ok(())
    }

    /// Composite a captured/blurred backdrop into `dst` — the `draw` half of the framebuffer
    /// effect, paired with [`Self::capture_backdrop`]. Samples the cached
    /// [`BackdropBlur::intermediate`] (blurred output, or the raw capture when blur is off)
    /// across its whole extent; the caller fills the material fields of `push` (`geo_size`,
    /// `corner_radius`, `input_to_geo`, `synoik_scale`, `saturation`, `noise`), this
    /// fills the placement + `src_rect` via [`Self::render_postprocess`] and clips to the
    /// rounded geometry.
    pub(crate) fn draw_backdrop(
        &mut self,
        blur: &BackdropBlur,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        push: PostprocessPush,
    ) -> Result<(), VulkanError> {
        let tex = blur.intermediate();
        let (w, h) = tex.extent();
        let src = Rectangle::<f64, BufferCoord>::from_size(Size::from((w as f64, h as f64)));
        self.render_postprocess(tex, src, dst, damage, push)
    }

    /// Draw the resize cross-fade material: blend two window snapshots (`tex_prev`, `tex_next`)
    /// into `dst` by `push.clamped_progress`, then optionally clip/round to the current
    /// geometry. The caller fills the material fields (the three transforms, `curr_geo_size`,
    /// `corner_radius`, `clamped_progress`, `clip_to_geometry`, `synoik_scale`, `synoik_alpha`);
    /// this fills the placement (`origin`/`size`/`target`), binds the premultiplied-blend
    /// resize pipeline with each texture's own descriptor set (prev at set 0, next at set 1),
    /// and draws the quad. The owned-renderer equivalent of niri's `ResizeRenderElement`.
    // Consumed by the live ResizeRenderElement wiring (Stage 3); exercised now by the material
    // test.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn render_resize(
        &mut self,
        tex_prev: &VkTexture,
        tex_next: &VkTexture,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        mut push: ResizePush,
    ) -> Result<(), VulkanError> {
        if tex_prev.flipped() || tex_next.flipped() {
            tracing::warn!("VulkanFrame::render_resize: flipped textures unsupported; skipping");
            return Ok(());
        }

        let scissors = self.damage_scissors(dst, damage);
        if scissors.is_empty() {
            return Ok(());
        }
        push.origin = [dst.loc.x as f32, dst.loc.y as f32];
        push.size = [dst.size.w as f32, dst.size.h as f32];
        push.proj = self.proj;
        push.target = self.target_dims();

        self.retain(tex_prev);
        self.retain(tex_next);
        let dev = &self.renderer.gpu.device;
        let pipe = &self.renderer.resize_pipeline;
        let sets = [tex_prev.descriptor_set(), tex_next.descriptor_set()];
        unsafe {
            dev.cmd_bind_pipeline(self.cbuf, vk::PipelineBindPoint::GRAPHICS, pipe.pipeline);
            dev.cmd_bind_descriptor_sets(
                self.cbuf,
                vk::PipelineBindPoint::GRAPHICS,
                pipe.layout,
                0,
                &sets,
                &[],
            );
            dev.cmd_push_constants(
                self.cbuf,
                pipe.layout,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                as_bytes(&push),
            );
            Self::draw_quad(dev, self.cbuf, &scissors);
        }
        Ok(())
    }

    /// Draw a user **resize** animation shader (niri's `custom_resize`) over two window snapshots
    /// (`tex_prev` at set 0, `tex_next` at set 1). No-op (with a warning) if no custom resize
    /// shader is installed — the built-in crossfade is `render_resize`, so this path is purely
    /// the user override. The caller fills the material fields of `push`; this fills placement.
    /// Wired live from the resize animation (`ResizeRenderElement`'s Vulkan arm).
    pub(crate) fn render_custom_resize(
        &mut self,
        tex_prev: &VkTexture,
        tex_next: &VkTexture,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        mut push: CustomResizePush,
    ) -> Result<(), VulkanError> {
        if tex_prev.flipped() || tex_next.flipped() {
            tracing::warn!("render_custom_resize: flipped textures unsupported; skipping");
            return Ok(());
        }
        let scissors = self.damage_scissors(dst, damage);
        if scissors.is_empty() {
            return Ok(());
        }
        push.origin = [dst.loc.x as f32, dst.loc.y as f32];
        push.size = [dst.size.w as f32, dst.size.h as f32];
        push.target = self.target_dims();
        push.proj = self.proj;

        self.retain(tex_prev);
        self.retain(tex_next);
        let Some(pipe) = self.renderer.custom_pipeline(CustomShaderType::Resize) else {
            tracing::warn!("render_custom_resize: no custom resize shader installed; skipping");
            return Ok(());
        };
        let dev = &self.renderer.gpu.device;
        let sets = [tex_prev.descriptor_set(), tex_next.descriptor_set()];
        unsafe {
            dev.cmd_bind_pipeline(self.cbuf, vk::PipelineBindPoint::GRAPHICS, pipe.pipeline);
            dev.cmd_bind_descriptor_sets(
                self.cbuf,
                vk::PipelineBindPoint::GRAPHICS,
                pipe.layout,
                0,
                &sets,
                &[],
            );
            dev.cmd_push_constants(
                self.cbuf,
                pipe.layout,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                as_bytes(&push),
            );
            Self::draw_quad(dev, self.cbuf, &scissors);
        }
        Ok(())
    }

    /// Draw a user **close** or **open** animation shader (niri's `custom_close`/`custom_open`)
    /// over one window snapshot (`texture` at set 0). No-op (with a warning) if that slot has
    /// no shader installed. `ty` must be `Close` or `Open` — resize uses
    /// [`Self::render_custom_resize`].
    /// Wired live from the window open animation (`CustomAnimRenderElement`'s Vulkan arm).
    pub(crate) fn render_custom_anim(
        &mut self,
        ty: CustomShaderType,
        texture: &VkTexture,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        mut push: CustomAnimPush,
    ) -> Result<(), VulkanError> {
        debug_assert!(
            matches!(ty, CustomShaderType::Close | CustomShaderType::Open),
            "render_custom_anim is for close/open; resize uses render_custom_resize",
        );
        if texture.flipped() {
            tracing::warn!("render_custom_anim: flipped textures unsupported; skipping");
            return Ok(());
        }
        let scissors = self.damage_scissors(dst, damage);
        if scissors.is_empty() {
            return Ok(());
        }
        push.origin = [dst.loc.x as f32, dst.loc.y as f32];
        push.size = [dst.size.w as f32, dst.size.h as f32];
        push.target = self.target_dims();
        push.proj = self.proj;

        self.retain(texture);
        let Some(pipe) = self.renderer.custom_pipeline(ty) else {
            tracing::warn!("render_custom_anim: no custom {ty:?} shader installed; skipping");
            return Ok(());
        };
        let dev = &self.renderer.gpu.device;
        let set = texture.descriptor_set();
        unsafe {
            dev.cmd_bind_pipeline(self.cbuf, vk::PipelineBindPoint::GRAPHICS, pipe.pipeline);
            dev.cmd_bind_descriptor_sets(
                self.cbuf,
                vk::PipelineBindPoint::GRAPHICS,
                pipe.layout,
                0,
                std::slice::from_ref(&set),
                &[],
            );
            dev.cmd_push_constants(
                self.cbuf,
                pipe.layout,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                as_bytes(&push),
            );
            Self::draw_quad(dev, self.cbuf, &scissors);
        }
        Ok(())
    }

    /// Record the present-blit into `self.cbuf` (already past `cmd_end_render_pass`): transition
    /// the imported dmabuf to a transfer destination, blit the shadow
    /// (`self.fb.buffer`, left in `TRANSFER_SRC_OPTIMAL` by the render pass) into it —
    /// `vkCmdBlitImage` converts component order, so RGBA lands as the BGRA bytes
    /// `Argb8888`/`Xrgb8888` scanout wants — then leave it in `GENERAL` for the display engine.
    ///
    /// Only the regions the frame actually touched are copied (see [`Self::present_damage`]): their
    /// union is the age-N damage `DrmCompositor` rendered for this cycled dmabuf, so the dmabuf's
    /// other pixels — its own age-N-ago presentation — are preserved (hence the barrier from the
    /// dmabuf's *tracked* layout, not `UNDEFINED`). This drops the per-frame cost from a
    /// full-screen blit to just the damage (e.g. a cursor move copies a couple of small rects,
    /// not 1280×720).
    ///
    /// (No queue-family-foreign ownership release: we CPU-wait for completion and the buffer is
    /// LINEAR, so the memory is coherent for KMS; a formal release would also block reading the
    /// result back on our own queue. Revisit if live scanout needs it.)
    fn record_present_blit(&self, present: &VkTexture) {
        let dev = &self.renderer.gpu.device;
        let (w, h) = self.fb.buffer.extent();
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .base_mip_level(0)
            .level_count(1)
            .base_array_layer(0)
            .layer_count(1);
        let layers = vk::ImageSubresourceLayers::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .mip_level(0)
            .base_array_layer(0)
            .layer_count(1);

        // Copy only the regions this frame touched (`present_damage`); the cycled dmabuf already
        // holds the rest from its own age-N-ago presentation. Clamp to the image and dedup exact
        // duplicates (many elements can share one damage rect).
        //
        // An empty set means this frame drew nothing, so copy nothing: the shadow is shared between
        // every present-blit target of the same size (scanout, a screencast buffer, a screencopy
        // region — see `present_blit_shadows`), and a whole-frame fallback here would copy whatever
        // the *previous* consumer rendered into this one's buffer. Leaving the target untouched is
        // also just what "nothing was drawn" means for a cycled buffer.
        //
        // Returning skips the layout transitions below, which keeps `present.layout()` accurate (it
        // is only advanced when we actually blit). A never-blitted buffer therefore stays
        // `UNDEFINED` — fine, because a buffer we have not rendered into before is age-0, which
        // damages the full frame and so never lands here.
        let mut rects: Vec<vk::Rect2D> = self
            .present_damage
            .iter()
            .filter_map(|r| {
                let x0 = r.offset.x.max(0);
                let y0 = r.offset.y.max(0);
                let x1 = ((r.offset.x as i64) + r.extent.width as i64).min(w as i64) as i32;
                let y1 = ((r.offset.y as i64) + r.extent.height as i64).min(h as i64) as i32;
                (x1 > x0 && y1 > y0).then(|| vk::Rect2D {
                    offset: vk::Offset2D { x: x0, y: y0 },
                    extent: vk::Extent2D {
                        width: (x1 - x0) as u32,
                        height: (y1 - y0) as u32,
                    },
                })
            })
            .collect();
        rects.sort_unstable_by_key(|r| (r.offset.x, r.offset.y, r.extent.width, r.extent.height));
        rects.dedup_by_key(|r| (r.offset.x, r.offset.y, r.extent.width, r.extent.height));
        if rects.is_empty() {
            return;
        }
        let blits: Vec<vk::ImageBlit> = rects
            .iter()
            .map(|r| {
                let x0 = r.offset.x;
                let y0 = r.offset.y;
                let x1 = x0 + r.extent.width as i32;
                let y1 = y0 + r.extent.height as i32;
                vk::ImageBlit::default()
                    .src_subresource(layers)
                    .src_offsets([
                        vk::Offset3D { x: x0, y: y0, z: 0 },
                        vk::Offset3D { x: x1, y: y1, z: 1 },
                    ])
                    .dst_subresource(layers)
                    .dst_offsets([
                        vk::Offset3D { x: x0, y: y0, z: 0 },
                        vk::Offset3D { x: x1, y: y1, z: 1 },
                    ])
            })
            .collect();

        // Preserve the dmabuf's existing content (its own last presentation) OUTSIDE the blitted
        // regions: transition from its TRACKED layout, not `UNDEFINED` (which would discard it and
        // reintroduce blanking under a partial copy). The first time a given scanout buffer is used
        // its tracked layout is `UNDEFINED`, which coincides with the age-0 full-damage frame that
        // copies the whole shadow anyway.
        let old_layout = present.layout();

        // That coincidence is the whole load-bearing assumption, and it is invisible when it
        // breaks: an `UNDEFINED` source layout lets the driver throw the image's contents away, so
        // a *partial* copy over one leaves the rest of the frame as whatever the driver felt like
        // — trails, in exactly the regions the scene is not repainting. It breaks the moment this
        // buffer is imported *again* under a fresh `Dmabuf` identity (the import is cached as
        // `HashMap<WeakDmabuf, VkTexture>`, and layout is tracked on the `VkTexture`), because the
        // re-import starts at `UNDEFINED` while `DrmCompositor` still counts the slot as aged.
        // Say so out loud rather than discarding silently: at most one per swapchain slot per
        // mode-set is expected, and anything more is the bug.
        if old_layout == vk::ImageLayout::UNDEFINED && rects != [full_rect(w, h)] {
            warn!(
                "present blit discarding a scanout buffer it is only partly overwriting: \
                 {}x{} target, tracked layout UNDEFINED but only {} rect(s) copied. \
                 Either this is the buffer's first use (once per swapchain slot) or its import \
                 was re-created and DrmCompositor's buffer age is now a lie.",
                w,
                h,
                rects.len(),
            );
        }

        unsafe {
            let to_dst = vk::ImageMemoryBarrier::default()
                .old_layout(old_layout)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(present.image())
                .subresource_range(range);
            synoik_vk::stats::barriers(1);
            dev.cmd_pipeline_barrier(
                self.cbuf,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                std::slice::from_ref(&to_dst),
            );

            synoik_vk::stats::blit(
                synoik_vk::stats::BlitSite::Present,
                rects
                    .iter()
                    .map(|r| u64::from(r.extent.width) * u64::from(r.extent.height))
                    .sum(),
            );
            dev.cmd_blit_image(
                self.cbuf,
                self.fb.buffer.image(),
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                present.image(),
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &blits,
                // Same size, so nearest is exact (and avoids a LINEAR-filter format check).
                vk::Filter::NEAREST,
            );

            let to_display = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::GENERAL)
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::empty())
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(present.image())
                .subresource_range(range);
            synoik_vk::stats::barriers(1);
            dev.cmd_pipeline_barrier(
                self.cbuf,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                std::slice::from_ref(&to_display),
            );
        }
        present.set_layout(vk::ImageLayout::GENERAL);
    }

    /// Which submit this frame's `finish` is. See [`synoik_vk::stats::SubmitSite`].
    ///
    /// The target alone does not answer it: a screencast or screencopy render is a dmabuf too, and
    /// counting those as scanout is what made "N to scanout" mean "N non-offscreen frames" instead
    /// of the one submit that costs a refresh interval. What separates them is whether the tty
    /// backend is asking for this frame — the same permission the deferred finish rides on.
    fn submit_site(&self) -> synoik_vk::stats::SubmitSite {
        submit_site_of(self.fb.offscreen, self.renderer.finish_is_for_kms())
    }

    /// The command buffer carrying our recorded glyph copies is never going to be submitted, so
    /// the atlas will never receive glyphs the residency index already records as resident. Forget
    /// the residency: the next shape re-rasterizes them and the affected bakes are redone. Without
    /// this, a transient submit failure is *permanently* blank text — the cache key a widget would
    /// have to change to re-bake has no reason to change.
    fn abandon_glyph_copies(&mut self) {
        if self.glyph_staging.take().is_some() {
            self.renderer.invalidate_glyphs();
        }
    }

    fn finish_internal(&mut self) -> Result<SyncPoint, VulkanError> {
        let result = self.finish_internal_impl();
        if result.is_err() {
            self.abandon_glyph_copies();
        }
        result
    }

    fn finish_internal_impl(&mut self) -> Result<SyncPoint, VulkanError> {
        if self.finished {
            return Ok(SyncPoint::signaled());
        }
        self.finished = true;
        let dev = &self.renderer.gpu.device;
        unsafe {
            dev.cmd_end_render_pass(self.cbuf);
            self.renderer.gpu_timer_mark(
                self.cbuf,
                self.gpu_slot,
                synoik_vk::stats::GpuPhase::Render,
            );
            // Present-blit scanout (KMS planes wanting `Argb8888`/`Xrgb8888`): the render pass left
            // the shadow in `TRANSFER_SRC_OPTIMAL` (its subpass→EXTERNAL dependency
            // already makes the writes available to a transfer read), so blit it into
            // the imported dmabuf, reordering RGBA→BGRA, then release the dmabuf to the
            // display engine.
            if let Some(present) = self.fb.present.as_ref() {
                self.record_present_blit(present);
            }
            // The render pass's `final_layout` left the target in
            // TRANSFER_SRC_OPTIMAL; take it the rest of the way here rather than
            // in a command buffer of its own.
            if self.fb.offscreen {
                transition_image(
                    dev,
                    self.cbuf,
                    self.fb.buffer.image(),
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                    vk::AccessFlags::SHADER_READ,
                    vk::PipelineStageFlags::FRAGMENT_SHADER,
                );
            }
            self.renderer.gpu_timer_end(self.cbuf, self.gpu_slot);
            dev.end_command_buffer(self.cbuf)?;

            // A *scanout* frame we intend to walk away from needs an **exportable** fence, because
            // KMS takes it — and a `SYNC_FD` fence's handle types are fixed at creation, so the
            // decision is made here, before the submit, and falls back to a plain fence (and thus
            // a synchronous finish) if the device cannot export. An offscreen finish has no such
            // consumer: its completion never leaves this process, so it defers on a plain fence
            // and can never be forced back onto the synchronous path by an export failure.
            let exportable = self.renderer.should_defer_finish() && !self.fb.offscreen;
            let exported = exportable
                .then(|| self.renderer.gpu.create_exportable_fence())
                .flatten()
                .transpose()
                .unwrap_or_else(|err| {
                    tracing::warn!("no exportable fence, finishing synchronously: {err}");
                    None
                });
            let defer = exported.is_some()
                || (self.fb.offscreen && self.renderer.should_defer_offscreen_finish());
            let fence = match exported {
                Some(fence) => fence,
                None => dev.create_fence(&vk::FenceCreateInfo::default(), None)?,
            };

            let timeline = {
                let _timed = synoik_vk::stats::submit(self.submit_site());
                self.renderer
                    .gpu
                    .submit(std::slice::from_ref(&self.cbuf), fence)
                    .map_err(VulkanError::from)?
            };

            // Hand the completion onward instead of parking on it. The command buffer, the
            // fence and every texture the draws reference move into the renderer's in-flight
            // list and are freed once the queue timeline passes this submit; `should_defer_finish`
            // has already established that nothing issued after this can execute alongside it.
            if let (true, Some(timeline)) = (defer, timeline) {
                let fence = VkSubmitFence::new(
                    self.renderer.gpu.clone(),
                    fence,
                    self.renderer.exported_scanout_fences.clone(),
                );
                let held = std::mem::take(&mut self.held);
                // What we render *into* is not in `held` and is not ours: the target may be a
                // present-blit shadow the renderer's LRU can evict, or an imported dmabuf its
                // cache can drop. Both free the image without a wait, so the record keeps them.
                let targets = std::iter::once(self.fb.buffer.clone())
                    .chain(self.fb.present.clone())
                    .collect();
                let glyph_staging = self.glyph_staging.take();
                // The staging buffers whose copies this command buffer carries. On the
                // synchronous path below they simply drop after the fence wait; here they have to
                // outlive a submit nobody is waiting for.
                let texture_staging = std::mem::take(&mut self.texture_staging);
                // Same reason, one level up: the chain's render pass and pipelines are bound to
                // this command buffer, and nobody is going to wait for it here.
                let blur_chains = std::mem::take(&mut self.blur_chains);
                self.renderer.add_in_flight(
                    timeline,
                    self.cbuf,
                    self.gpu_slot,
                    fence.clone(),
                    held,
                    targets,
                    glyph_staging,
                    texture_staging,
                    blur_chains,
                );
                // Same bookkeeping as the synchronous path below: the render pass's `final_layout`
                // leaves a scanout target in TRANSFER_SRC_OPTIMAL, and an offscreen was taken the
                // rest of the way to SHADER_READ_ONLY_OPTIMAL by the barrier recorded above. Every
                // later command is ordered after this submit, so recording it now is as true as
                // recording it after a wait — and it is what keeps `make_offscreen_sampleable` the
                // no-op it has to be here, since a barrier submitted from outside this command
                // buffer would put the round trip straight back.
                self.fb.buffer.set_layout(if self.fb.offscreen {
                    vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
                } else {
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL
                });
                return Ok(SyncPoint::from(fence));
            }

            // Timed apart from the submit above: this park is where a scanout frame spends
            // 12–14 ms of its budget, and it is the one this renderer means to stop paying on
            // the compositor thread. See `docs/fork/foundation.md`.
            {
                let _timed = synoik_vk::stats::retire(self.submit_site());
                dev.wait_for_fences(std::slice::from_ref(&fence), true, u64::MAX)?;
            }
            dev.destroy_fence(fence, None);
            dev.free_command_buffers(self.renderer.command_pool, std::slice::from_ref(&self.cbuf));
            // The wait above proves our recorded glyph copy has executed; the staging can go.
            self.glyph_staging = None;
        }
        // Reached only on the synchronous branch (the deferred one returned above): the fence is
        // signalled, so the queries are resolved and this does not block. A deferred frame's pair
        // is read at retirement instead — see `VulkanRenderer::retire_completed`.
        if let Some(slot) = self.gpu_slot {
            self.renderer.gpu_timer_collect_through(slot);
        }
        // The render pass's `final_layout` leaves the target in TRANSFER_SRC_OPTIMAL (see
        // `create_render_pass`); record it so readback is a no-op and `make_sampleable` knows the
        // source layout for its barrier. Unless we just took it further, above.
        self.fb.buffer.set_layout(if self.fb.offscreen {
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL
        } else {
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL
        });
        Ok(SyncPoint::signaled())
    }
}

impl Drop for VulkanFrame<'_, '_> {
    fn drop(&mut self) {
        if let Err(err) = self.finish_internal() {
            tracing::warn!("dropping VulkanFrame with unflushed work: {err}");
        }
    }
}

impl Frame for VulkanFrame<'_, '_> {
    type Error = VulkanError;
    type TextureId = VkTexture;

    fn context_id(&self) -> ContextId<VkTexture> {
        self.renderer.ctx_id()
    }

    fn clear(
        &mut self,
        color: Color32F,
        at: &[Rectangle<i32, Physical>],
    ) -> Result<(), VulkanError> {
        // Smithay's `Frame::clear` contract: `at` is the set of regions to clear, so an EMPTY slice
        // means clear NOTHING (mirroring `GlesFrame::clear`, which early-returns) — NOT "clear the
        // whole target". The damage tracker calls `clear(color, damage - opaque_regions)` before
        // drawing elements, and that difference is empty whenever the frame's damage is fully
        // covered by opaque elements (e.g. the cursor moving over an opaque window). Treating empty
        // as "whole target" would wipe the LOAD-preserved shadow on every such frame, so under
        // partial damage the settled scene flickers to the clear color — the exact bug this avoids.
        if at.is_empty() {
            return Ok(());
        }
        let attachment = vk::ClearAttachment {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            color_attachment: 0,
            clear_value: vk::ClearValue {
                color: vk::ClearColorValue {
                    float32: color.components(),
                },
            },
        };
        // `cmd_clear_attachments` rects are in physical framebuffer space (no projection), but
        // callers pass logical rects — so map each through the output transform, exactly as GLES's
        // `clear` reaches the framebuffer via the transform-aware solid draw. Identity for
        // `Normal`.
        let rects: Vec<vk::ClearRect> = at
            .iter()
            .map(|r| {
                let phys = self.transform.transform_rect_in(*r, &self.logical_size);
                vk::ClearRect {
                    rect: vk::Rect2D {
                        offset: vk::Offset2D {
                            x: phys.loc.x,
                            y: phys.loc.y,
                        },
                        extent: vk::Extent2D {
                            width: phys.size.w.max(0) as u32,
                            height: phys.size.h.max(0) as u32,
                        },
                    },
                    base_array_layer: 0,
                    layer_count: 1,
                }
            })
            .collect();
        // Record the cleared regions so the present-blit copies them (they are the non-opaque part
        // of the frame's damage; the draws cover the rest).
        self.present_damage.extend(rects.iter().map(|r| r.rect));
        unsafe {
            self.renderer.gpu.device.cmd_clear_attachments(
                self.cbuf,
                std::slice::from_ref(&attachment),
                &rects,
            );
        }
        Ok(())
    }

    fn draw_solid(
        &mut self,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        color: Color32F,
    ) -> Result<(), VulkanError> {
        // A `ClippedSurfaceRenderElement` armed a clip for this surface, and a surface can arrive
        // as a solid colour as easily as a texture (a single-pixel `wl_buffer`, a blocked-out
        // window). Honouring the clip only in `render_texture_from_to` left those square.

        if let Some(clip) = self.clip_override {
            return self.render_clipped_solid(dst, damage, color, clip);
        }

        let scissors = self.damage_scissors(dst, damage);
        if scissors.is_empty() {
            return Ok(());
        }
        let push = QuadPush {
            origin: [dst.loc.x as f32, dst.loc.y as f32],
            size: [dst.size.w as f32, dst.size.h as f32],
            proj: self.proj,
            target: self.target_dims(),
            color: color.components(),
            ..Default::default()
        };
        let dev = &self.renderer.gpu.device;
        let pipe = &self.renderer.solid_pipeline;
        unsafe {
            dev.cmd_bind_pipeline(self.cbuf, vk::PipelineBindPoint::GRAPHICS, pipe.pipeline);
            dev.cmd_push_constants(
                self.cbuf,
                pipe.layout,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                as_bytes(&push),
            );
            Self::draw_quad(dev, self.cbuf, &scissors);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn render_texture_from_to(
        &mut self,
        texture: &VkTexture,
        src: Rectangle<f64, BufferCoord>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        src_transform: Transform,
        alpha: f32,
    ) -> Result<(), VulkanError> {
        // A degenerate src/texture has nothing to sample (mirrors GLES `build_texture_mat`'s early
        // return). Every `src_transform` + `flipped()` case is otherwise handled by the sampling
        // `tex_transform`, so there is no unsupported case left to skip.
        if src.size.w <= 0.0 || src.size.h <= 0.0 || texture.width() == 0 || texture.height() == 0 {
            return Ok(());
        }

        // A `ClippedSurfaceRenderElement` armed a clip for this surface: swap to the
        // clipped-surface pipeline, which samples through the same `tex_transform` but
        // masks the result to a rounded geometry. See `render_clipped_texture`.
        if let Some(clip) = self.clip_override {
            return self.render_clipped_texture(
                texture,
                src,
                dst,
                damage,
                src_transform,
                alpha,
                clip,
            );
        }

        let scissors = self.damage_scissors(dst, damage);
        if scissors.is_empty() {
            return Ok(());
        }
        let push = QuadPush {
            origin: [dst.loc.x as f32, dst.loc.y as f32],
            size: [dst.size.w as f32, dst.size.h as f32],
            proj: self.proj,
            target: self.target_dims(),
            // texture.frag multiplies the sample by this. Both are premultiplied, so a uniform
            // `alpha` tint scales color and coverage together — matching the GLES oracle's
            // `color = texture2D(tex, uv) * alpha`. A `[1, 1, 1, alpha]` straight tint here would
            // leave rgb unattenuated and wash out every fade.
            color: [alpha, alpha, alpha, alpha],
            tex_transform: build_tex_transform(src, texture, src_transform),
            ..Default::default()
        };
        self.retain(texture);
        let dev = &self.renderer.gpu.device;
        let pipe = &self.renderer.texture_pipeline;
        let set = texture.descriptor_set();
        unsafe {
            dev.cmd_bind_pipeline(self.cbuf, vk::PipelineBindPoint::GRAPHICS, pipe.pipeline);
            dev.cmd_bind_descriptor_sets(
                self.cbuf,
                vk::PipelineBindPoint::GRAPHICS,
                pipe.layout,
                0,
                std::slice::from_ref(&set),
                &[],
            );
            dev.cmd_push_constants(
                self.cbuf,
                pipe.layout,
                vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                0,
                as_bytes(&push),
            );
            Self::draw_quad(dev, self.cbuf, &scissors);
        }
        Ok(())
    }

    fn transformation(&self) -> Transform {
        self.transform
    }

    // Logical size (transform-swapped), matching `GlesFrame::output_size`; see the inherent
    // `output_size` above — the getter intentionally returns `logical_size`, not `output_size`.
    #[allow(clippy::misnamed_getters)]
    fn output_size(&self) -> Size<i32, Physical> {
        self.logical_size
    }

    fn wait(&mut self, sync: &SyncPoint) -> Result<(), VulkanError> {
        sync.wait().map_err(|_| VulkanError::SyncInterrupted)
    }

    fn finish(mut self) -> Result<SyncPoint, VulkanError> {
        self.finish_internal()
    }
}

/// The output-transform 2×2 for the vertex projection (`proj` push field), column-major
/// `[m00, m10, m01, m11]` so the shader's `mat2(pc.proj)` reconstructs it. It rotates/flips the
/// (already y-down) ortho NDC — which places logical geometry into `[-1, 1]²` — into the physical
/// framebuffer's orientation.
///
/// Derivation: mirroring GLES's `current_projection = flip180 · transform.matrix() · ortho`, the
/// GL-y-up→Vulkan-y-down convention change conjugates the rotation by `diag(1, -1)`, i.e.
/// `proj = diag(1,-1) · T₂ · diag(1,-1)` — Smithay's `transform.matrix()` top-left 2×2 with its
/// off-diagonal entries negated (diagonal unchanged). Identity for `Normal`. Conjugation preserves
/// the determinant, so rotations stay rotations and flips stay flips (the det = −1 cases reverse
/// triangle winding — harmless only because every material pipeline uses `CullMode::NONE`).
fn ndc_transform(transform: Transform) -> [f32; 4] {
    // cgmath `Matrix3` is column-major: `m.x`/`m.y` are columns 0/1, so the top-left 2×2 is
    // m00 = m.x.x, m10 = m.x.y, m01 = m.y.x, m11 = m.y.y. Negate the off-diagonals (m10, m01).
    let m = transform.matrix();
    [m.x.x, -m.x.y, -m.y.x, m.y.y]
}

/// Premultiply a straight-alpha RGBA into the renderer's premultiplied convention.
///
/// The renderer is premultiplied end to end (see `build_pipeline`'s alpha convention), but the
/// toolkit above it keeps straight-alpha colors because that is how the GNOME SCSS states them
/// (`style::HOVER_WASH` is literally white at 10%). This is the boundary where the two meet: every
/// frame method taking a toolkit `Rgba` premultiplies through here before it reaches a push
/// constant. A no-op for opaque and fully transparent colors, which is precisely why feeding a
/// straight color to a premultiplied blend stayed invisible for so long.
pub(crate) fn premultiply(color: [f32; 4]) -> [f32; 4] {
    let a = color[3];
    [color[0] * a, color[1] * a, color[2] * a, a]
}

/// Pack a `glam::Mat3` into 3 `vec4` columns (`.xyz` used, `w = 0`) for a `mat3` push field.
pub(crate) fn pack_mat3(m: Mat3) -> [[f32; 4]; 3] {
    let col = |v: Vec3| [v.x, v.y, v.z, 0.0];
    [col(m.x_axis), col(m.y_axis), col(m.z_axis)]
}

/// The `tex_transform` sampling matrix mapping `v_uv ∈ [0,1]` across the quad to normalized texture
/// UV — the owned-renderer analogue of Smithay's `build_texture_mat`, folded so the input is the
/// unit quad coordinate rather than dst-local pixels (the `dest.size` scale cancels, so it doesn't
/// appear). Bakes: the buffer `src_transform` rotation/flip of the sampled content, the `src` crop,
/// normalization to UV, and (for y-inverted textures) a v-flip. Built from `src_transform` ONLY —
/// the output transform lives entirely in `proj`/position and must not enter here.
fn build_tex_transform(
    src: Rectangle<f64, BufferCoord>,
    texture: &VkTexture,
    src_transform: Transform,
) -> [[f32; 4]; 3] {
    let (tw, th) = (texture.width() as f32, texture.height() as f32);
    // `src.size` with the buffer transform applied (w/h swapped for 90/270) — GLES `dst_src_size`.
    let dss = src_transform.transform_size(src.size);
    let (dw, dh) = (dss.w as f32, dss.h as f32);

    // v_uv ∈ [0,1] → src-size pixels (dest.size folded out), then rotate/flip in pixel space...
    let mut m = Mat3::from_cols_array(src_transform.matrix().as_ref())
        * Mat3::from_scale(Vec2::new(dw, dh));
    // ...then the per-transform re-centering translation (in dst_src_size units), matching the
    // `translation` table in Smithay's `build_texture_mat`.
    let t = match src_transform {
        Transform::Normal | Transform::Flipped90 => Vec2::ZERO,
        Transform::_90 => Vec2::new(0.0, dw),
        Transform::_180 => Vec2::new(dw, dh),
        Transform::_270 => Vec2::new(dh, 0.0),
        Transform::Flipped => Vec2::new(dw, 0.0),
        Transform::Flipped180 => Vec2::new(0.0, dh),
        Transform::Flipped270 => Vec2::new(dh, dw),
    };
    m = Mat3::from_translation(t) * m;
    // Crop offset (buffer coords), then normalize to [0,1] UV.
    m = Mat3::from_translation(Vec2::new(src.loc.x as f32, src.loc.y as f32)) * m;
    m = Mat3::from_scale(Vec2::new(1.0 / tw, 1.0 / th)) * m;
    // y-inverted textures: flip v about the [0,1] centre. Our samplers are CLAMP_TO_EDGE (not GL
    // REPEAT), so this must be `v ↦ 1 - v` (translate·scale), not Smithay's naked `-v`.
    if texture.flipped() {
        m = Mat3::from_translation(Vec2::new(0.0, 1.0))
            * Mat3::from_scale(Vec2::new(1.0, -1.0))
            * m;
    }
    pack_mat3(m)
}

/// Normalize a buffer-space `src` sub-rectangle to `[u0, v0, du, dv]` texture coordinates for the
/// postprocess material's `src_rect` push constant. `[0, 0, 1, 1]` is the full texture.
fn normalized_src(src: Rectangle<f64, BufferCoord>, texture: &VkTexture) -> [f32; 4] {
    let (tw, th) = (texture.width() as f32, texture.height() as f32);
    [
        src.loc.x as f32 / tw,
        src.loc.y as f32 / th,
        src.size.w as f32 / tw,
        src.size.h as f32 / th,
    ]
}

/// Where a frame's submit came from, from the two facts that decide it. A free
/// function because it is needed once before the [`VulkanFrame`] exists (tagging
/// the GPU timestamp pair at `begin`) and again from the frame itself at submit
/// and retire — three call sites that must never disagree about what a frame is.
///
/// Takes two positional `bool`s, which is exactly the shape that swaps silently:
/// transposing them still compiles and inverts scanout/offscreen attribution for
/// the whole session. Pinned by `submit_site_names_the_frame_not_the_target`.
pub(super) fn submit_site_of(offscreen: bool, for_kms: bool) -> synoik_vk::stats::SubmitSite {
    if offscreen {
        synoik_vk::stats::SubmitSite::OffscreenFrame
    } else if for_kms {
        synoik_vk::stats::SubmitSite::KmsFrame
    } else {
        synoik_vk::stats::SubmitSite::DmabufFrame
    }
}

/// The whole-image rect for a `w`×`h` target.
fn full_rect(w: u32, h: u32) -> vk::Rect2D {
    vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent: vk::Extent2D {
            width: w,
            height: h,
        },
    }
}
