use std::fmt;

use ash::vk;
use glam::{Mat3, Vec2, Vec3};
use niri_vk::render::{
    as_bytes, BorderPush, ClippedTexturePush, PostprocessPush, QuadPush, ResizePush, ShadowPush,
    TextPush, IDENTITY_PROJ,
};
use smithay::backend::renderer::sync::SyncPoint;
use smithay::backend::renderer::{Color32F, ContextId, Frame, Texture};
use smithay::utils::{Buffer as BufferCoord, Physical, Point, Rectangle, Size, Transform};

use super::backdrop_blur::BackdropBlur;
use super::custom::{CustomAnimPush, CustomResizePush, CustomShaderType};
use super::error::VulkanError;
use super::renderer::{transition_image, VulkanRenderer};
use super::types::{GlyphRun, VkFramebuffer, VkTexture};

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
    pub niri_scale: f32,
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
    finished: bool,
    /// Leave the target in `SHADER_READ_ONLY_OPTIMAL` when the frame finishes, by
    /// recording the transition into *this* command buffer. See
    /// [`finish_sampleable`](Self::finish_sampleable).
    sampleable_on_finish: bool,
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
        // (DONT_CARE) when this is a present-blit scanout target that already holds a valid prior
        // frame — the basis for damage-preserving (partial-damage) rendering. The present-blit
        // shadow is a single buffer reused across frames, so from its perspective its buffer-age is
        // always 1; DrmCompositor's per-dmabuf damage (age ≥ 1) is a superset of what the shadow
        // needs preloaded, so LOAD + redrawing that damage always lands a fully-correct frame. A
        // fresh (just-reallocated) shadow is `UNDEFINED` (nothing to preserve) so it discards
        // (DONT_CARE) — aligned with the swapchain realloc that makes DrmCompositor full-damage.
        // `finish_internal` only records `TRANSFER_SRC_OPTIMAL` after a successful submit, so an
        // errored frame never leaves a "valid" layout over undefined content. The LOAD pass is
        // render-pass-compatible with the base pass (identical attachment/subpass layout), so
        // `framebuffer` (built against the base pass) and every pipeline bind unchanged.
        let preserve =
            fb.present.is_some() && fb.buffer.layout() == vk::ImageLayout::TRANSFER_SRC_OPTIMAL;
        let render_pass = if preserve {
            renderer.continuation_render_pass
        } else {
            renderer.render_pass
        };

        let render_area = vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent,
        };
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
        renderer.gpu_timer_begin(cbuf);

        renderer.record_pending_dmabuf_acquires(cbuf);

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
            held: Vec::new(),
            clip_override: None,
            present_damage: Vec::new(),
            finished: false,
            sampleable_on_finish: false,
        })
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
            niri_scale: clip.niri_scale,
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
    // Non-test dead until the overlay draw layer (rounded-chrome pass) calls it; exercised now by
    // the `vulkan_rounded_rect_fills_and_cuts_corners` test.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn render_rounded_rect(
        &mut self,
        color: [f32; 4],
        corner_radius: f32,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
    ) -> Result<(), VulkanError> {
        self.render_rounded_rect_impl(color, corner_radius, 0.0, dst, damage)
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
        self.render_rounded_rect_impl(color, corner_radius, stroke_width.max(0.), dst, damage)
    }

    fn render_rounded_rect_impl(
        &mut self,
        color: [f32; 4],
        corner_radius: f32,
        stroke_width: f32,
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
            corner_radius,
            stroke_width,
            // The toolkit states colors straight-alpha (as the SCSS does); the renderer is
            // premultiplied. This is that boundary.
            color: premultiply(color),
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
        niri_scale: f32,
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
            niri_scale,
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
            niri_alpha: 1.0,
            _pad0: 0.0,
        };
        self.render_shadow(push, area, damage)
    }

    /// Draw the postprocess-and-clip material: sample `texture` (from `src`) into `dst`, applying
    /// the saturation / noise / premultiplied-bg + general rounded-corner clip carried by `push`.
    /// The caller fills the material fields (`geo_size`, `corner_radius`, `bg_color`,
    /// `input_to_geo`, `niri_scale`, `niri_alpha`, `saturation`, `noise`); this fills the placement
    /// (`origin`/`size`/`target`/`src_rect`), binds the premultiplied-blend postprocess pipeline +
    /// the texture's descriptor set, and draws the quad. The owned-renderer equivalent of niri's
    /// clipped-surface / framebuffer-effect postprocess shader. Same unflipped scope as the other
    /// sampling materials.
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
    /// differ, e.g. the overview zoom trick), leaves `dest` in `SHADER_READ_ONLY_OPTIMAL`, then
    /// **flushes** (submits + fence-waits) and re-opens a fresh command buffer on the LOAD-variant
    /// [`continuation pass`](VulkanRenderer::continuation_render_pass) so the preserved scene can
    /// be drawn over. Flushing (rather than a same-cbuf barrier) keeps `dest` fully written
    /// before the caller's blur — which runs on its own one-shot submission (`render_blur`) —
    /// samples it, matching this renderer's synchronous per-submit model. `dest` must be a
    /// `SAMPLED | TRANSFER_DST` offscreen (i.e. from [`Offscreen::create_buffer`]); its whole
    /// extent is filled.
    // Consumed by the live FramebufferEffectElement wiring (Stage 3); exercised now by the
    // render-pass-split test.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn capture_region(
        &mut self,
        src_region: Rectangle<i32, Physical>,
        dest: &VkTexture,
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

        let old_cbuf = self.cbuf;
        // Poison the frame across the flush + re-open: after `cmd_end_render_pass` (and especially
        // after `free_command_buffers`) `old_cbuf` is no longer a valid recording target, so if any
        // fallible step below returns early, `Drop`/`finish` must NOT try to end a pass on it.
        // Cleared once `new_cbuf` is installed, making the frame live again.
        self.finished = true;
        let dev = &self.renderer.gpu.device;
        let new_cbuf = unsafe {
            dev.cmd_end_render_pass(old_cbuf);

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
            dev.cmd_pipeline_barrier(
                old_cbuf,
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
            dev.cmd_blit_image(
                old_cbuf,
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
            dev.cmd_pipeline_barrier(
                old_cbuf,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                std::slice::from_ref(&to_read),
            );

            // Flush the capture so `dest` is fully written before the caller's separately-submitted
            // blur samples it, then re-open a fresh command buffer on the continuation pass.
            self.renderer.gpu_timer_end(old_cbuf);
            dev.end_command_buffer(old_cbuf)?;
            let fence = dev.create_fence(&vk::FenceCreateInfo::default(), None)?;
            let submit = vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&old_cbuf));
            dev.queue_submit(
                self.renderer.gpu.queue,
                std::slice::from_ref(&submit),
                fence,
            )?;
            dev.wait_for_fences(std::slice::from_ref(&fence), true, u64::MAX)?;
            dev.destroy_fence(fence, None);
            dev.free_command_buffers(self.renderer.command_pool, std::slice::from_ref(&old_cbuf));
            // Bank this segment's GPU time *before* re-arming: the pool has only
            // two slots, and the continuation buffer's reset would otherwise
            // discard this submit's pair unread.
            self.renderer.gpu_timer_collect();

            let alloc = vk::CommandBufferAllocateInfo::default()
                .command_pool(self.renderer.command_pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1);
            let new_cbuf = dev.allocate_command_buffers(&alloc)?[0];
            let begin_info = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            let pass_begin = vk::RenderPassBeginInfo::default()
                .render_pass(continuation)
                .framebuffer(framebuffer)
                .render_area(render_area);
            dev.begin_command_buffer(new_cbuf, &begin_info)?;
            // Re-arm for the continuation buffer, still outside its render pass.
            self.renderer.gpu_timer_begin(new_cbuf);
            dev.cmd_begin_render_pass(new_cbuf, &pass_begin, vk::SubpassContents::INLINE);
            dev.cmd_set_viewport(new_cbuf, 0, std::slice::from_ref(&viewport));
            dev.cmd_set_scissor(new_cbuf, 0, std::slice::from_ref(&render_area));
            new_cbuf
        };

        self.cbuf = new_cbuf;
        // The frame is live again on the fresh command buffer.
        self.finished = false;
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
    /// — per-frame allocation is Venus blob churn), captures `src_region` of the target into it
    /// via [`Self::capture_region`], then records the blur. The element then composites
    /// [`BackdropBlur::intermediate`] with [`Self::render_postprocess`] in its `draw`.
    pub(crate) fn capture_backdrop(
        &mut self,
        slot: &mut Option<BackdropBlur>,
        src_region: Rectangle<i32, Physical>,
        size: Size<i32, BufferCoord>,
        passes: Option<usize>,
        offset: f32,
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

        let dims = (size.w as u32, size.h as u32);
        let reuse = slot.as_ref().is_some_and(|b| b.matches(dims, passes));
        if !reuse {
            *slot = Some(BackdropBlur::new(self.renderer, size, passes)?);
        }
        let bb = slot.as_mut().expect("just populated");
        self.capture_region(src_region, bb.capture())?;
        bb.run_blur(offset)?;
        Ok(())
    }

    /// Composite a captured/blurred backdrop into `dst` — the `draw` half of the framebuffer
    /// effect, paired with [`Self::capture_backdrop`]. Samples the cached
    /// [`BackdropBlur::intermediate`] (blurred output, or the raw capture when blur is off)
    /// across its whole extent; the caller fills the material fields of `push` (`geo_size`,
    /// `corner_radius`, `input_to_geo`, `niri_scale`, `saturation`, `noise`, `bg_color`), this
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
    /// `corner_radius`, `clamped_progress`, `clip_to_geometry`, `niri_scale`, `niri_alpha`);
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
    /// the imported dmabuf to a transfer destination, blit the R8G8B8A8 shadow
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
            dev.cmd_pipeline_barrier(
                self.cbuf,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                std::slice::from_ref(&to_dst),
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

    /// Finish this frame, leaving its target ready to be sampled by a later draw.
    ///
    /// The transition to `SHADER_READ_ONLY_OPTIMAL` is recorded into the frame's
    /// own command buffer, so it rides the submit that was happening anyway.
    /// [`VulkanRenderer::make_sampleable`] does the same transition through
    /// `run_commands` — a second command buffer, submit and fence wait — which for
    /// an offscreen bake doubles the round trips for what is one pipeline barrier.
    /// A bake's GPU work is negligible and measurably pixel-independent (a
    /// 1920x1080 bake costs the same as a 220x32 one), so those round trips *are*
    /// the cost.
    ///
    /// Offscreen targets only. A scanout frame must stay in `TRANSFER_SRC_OPTIMAL`
    /// for the present blit, so it uses plain `finish`.
    pub fn finish_sampleable(mut self) -> Result<SyncPoint, VulkanError> {
        self.sampleable_on_finish = true;
        self.finish_internal()
    }

    fn finish_internal(&mut self) -> Result<SyncPoint, VulkanError> {
        if self.finished {
            return Ok(SyncPoint::signaled());
        }
        self.finished = true;
        let dev = &self.renderer.gpu.device;
        unsafe {
            dev.cmd_end_render_pass(self.cbuf);
            // Present-blit scanout (KMS planes wanting `Argb8888`/`Xrgb8888`): the render pass left
            // the R8G8B8A8 shadow in `TRANSFER_SRC_OPTIMAL` (its subpass→EXTERNAL dependency
            // already makes the writes available to a transfer read), so blit it into
            // the imported dmabuf, reordering RGBA→BGRA, then release the dmabuf to the
            // display engine.
            if let Some(present) = self.fb.present.as_ref() {
                self.record_present_blit(present);
            }
            // The render pass's `final_layout` left the target in
            // TRANSFER_SRC_OPTIMAL; take it the rest of the way here rather than
            // in a command buffer of its own.
            if self.sampleable_on_finish {
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
            self.renderer.gpu_timer_end(self.cbuf);
            dev.end_command_buffer(self.cbuf)?;
            let fence = dev.create_fence(&vk::FenceCreateInfo::default(), None)?;
            let submit =
                vk::SubmitInfo::default().command_buffers(std::slice::from_ref(&self.cbuf));
            dev.queue_submit(
                self.renderer.gpu.queue,
                std::slice::from_ref(&submit),
                fence,
            )?;
            dev.wait_for_fences(std::slice::from_ref(&fence), true, u64::MAX)?;
            dev.destroy_fence(fence, None);
            dev.free_command_buffers(self.renderer.command_pool, std::slice::from_ref(&self.cbuf));
        }
        // The fence is signalled, so the queries are resolved and this does not
        // block.
        self.renderer.gpu_timer_collect();
        // The render pass's `final_layout` leaves the target in TRANSFER_SRC_OPTIMAL (see
        // `create_render_pass`); record it so readback is a no-op and `make_sampleable` knows the
        // source layout for its barrier. Unless we just took it further, above.
        self.fb.buffer.set_layout(if self.sampleable_on_finish {
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
