use std::fmt;

use ash::vk;
use niri_vk::render::{as_bytes, BorderPush, PostprocessPush, QuadPush, ResizePush, ShadowPush};
use smithay::backend::renderer::sync::SyncPoint;
use smithay::backend::renderer::{Color32F, ContextId, Frame, Texture};
use smithay::utils::{Buffer as BufferCoord, Physical, Rectangle, Size, Transform};

use super::backdrop_blur::BackdropBlur;
use super::custom::{CustomAnimPush, CustomResizePush, CustomShaderType};
use super::error::VulkanError;
use super::renderer::VulkanRenderer;
use super::types::{VkFramebuffer, VkTexture};

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
    finished: bool,
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
        let cbuf = {
            let dev = &renderer.gpu.device;
            let alloc = vk::CommandBufferAllocateInfo::default()
                .command_pool(renderer.command_pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1);
            let cbuf = unsafe { dev.allocate_command_buffers(&alloc) }?[0];

            let begin_info = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            let render_area = vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent,
            };
            // Load op is DONT_CARE (see renderer::create_render_pass); callers clear explicitly.
            let pass_begin = vk::RenderPassBeginInfo::default()
                .render_pass(renderer.render_pass)
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
                dev.begin_command_buffer(cbuf, &begin_info)?;
                dev.cmd_begin_render_pass(cbuf, &pass_begin, vk::SubpassContents::INLINE);
                dev.cmd_set_viewport(cbuf, 0, std::slice::from_ref(&viewport));
                dev.cmd_set_scissor(cbuf, 0, std::slice::from_ref(&render_area));
            }
            cbuf
        };

        Ok(VulkanFrame {
            renderer,
            fb,
            cbuf,
            output_size,
            transform,
            logical_size: transform.transform_size(output_size),
            proj: ndc_transform(transform),
            held: Vec::new(),
            finished: false,
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

    /// Draw `texture` into `dst` with its corners rounded by `corner_radius` (physical pixels) —
    /// the owned-renderer equivalent of niri's `RoundedTextureRenderElement` GLES draw. A partial
    /// `src` is remapped by the shader; only flipped textures are out of scope and degrade to a
    /// no-op (a visible gap) rather than a wrong picture.
    ///
    /// Assumes the element's rounding geometry equals `dst` (true for the overview wallpaper, whose
    /// `geometry` is the whole view at the origin); the general `geometry != dst` clip is a later
    /// clipped-surface concern. Called from `RoundedTextureRenderElement`'s Vulkan draw, hence
    /// `pub(crate)`.
    pub(crate) fn render_rounded_texture(
        &mut self,
        texture: &VkTexture,
        src: Rectangle<f64, BufferCoord>,
        dst: Rectangle<i32, Physical>,
        corner_radius: f32,
        alpha: f32,
    ) -> Result<(), VulkanError> {
        // Skeleton scope: unflipped only (the shader remaps a partial `src` via `src_rect`).
        if texture.flipped() {
            tracing::warn!(
                "VulkanFrame::render_rounded_texture: flipped textures unsupported; skipping"
            );
            return Ok(());
        }

        let push = QuadPush {
            origin: [dst.loc.x as f32, dst.loc.y as f32],
            size: [dst.size.w as f32, dst.size.h as f32],
            proj: self.proj,
            target: self.target_dims(),
            corner_radius,
            // rounded_texture.frag multiplies the sample by this, so white-with-alpha modulates
            // alpha; the SDF coverage then cuts the corners.
            color: [1.0, 1.0, 1.0, alpha],
            src_rect: normalized_src(src, texture),
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
            dev.cmd_draw(self.cbuf, 6, 1, 0, 0);
        }
        Ok(())
    }

    /// Draw `texture` into `dst` with a horizontal alpha fade over `cutoff` (`[left, right]` in the
    /// sampled texture's u coordinate; `left >= right` disables it) — the owned-renderer equivalent
    /// of niri's `GradientFadeTextureRenderElement` (the MRU switcher fades clipped thumbnails).
    /// Same partial-`src`/unflipped scope as [`Self::render_rounded_texture`].
    pub(crate) fn render_gradient_fade(
        &mut self,
        texture: &VkTexture,
        src: Rectangle<f64, BufferCoord>,
        dst: Rectangle<i32, Physical>,
        cutoff: (f32, f32),
        alpha: f32,
    ) -> Result<(), VulkanError> {
        if texture.flipped() {
            tracing::warn!(
                "VulkanFrame::render_gradient_fade: flipped textures unsupported; skipping"
            );
            return Ok(());
        }

        let push = QuadPush {
            origin: [dst.loc.x as f32, dst.loc.y as f32],
            size: [dst.size.w as f32, dst.size.h as f32],
            proj: self.proj,
            target: self.target_dims(),
            color: [1.0, 1.0, 1.0, alpha],
            src_rect: normalized_src(src, texture),
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
            dev.cmd_draw(self.cbuf, 6, 1, 0, 0);
        }
        Ok(())
    }

    /// Draw a border ring — the owned-renderer equivalent of niri's `BorderRenderElement` (an
    /// angled gradient clipped to a rounded-rect ring). The caller (the element's Vulkan draw)
    /// fills every material field of `push` plus `origin`/`size` from `dst`; this sets `target`,
    /// binds the premultiplied-blend border pipeline (no texture), and draws the quad.
    pub(crate) fn render_border(&mut self, mut push: BorderPush) -> Result<(), VulkanError> {
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
            dev.cmd_draw(self.cbuf, 6, 1, 0, 0);
        }
        Ok(())
    }

    /// Draw a rounded-rectangle drop shadow — the owned-renderer equivalent of niri's
    /// `ShadowRenderElement` (a gaussian-blurred rounded rect with an optional window cutout).
    /// Like [`Self::render_border`], the caller fills the material fields plus `origin`/`size`;
    /// this sets `target`, binds the premultiplied-blend shadow pipeline (no texture), and draws.
    pub(crate) fn render_shadow(&mut self, mut push: ShadowPush) -> Result<(), VulkanError> {
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
            dev.cmd_draw(self.cbuf, 6, 1, 0, 0);
        }
        Ok(())
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
        mut push: PostprocessPush,
    ) -> Result<(), VulkanError> {
        if texture.flipped() {
            tracing::warn!(
                "VulkanFrame::render_postprocess: flipped textures unsupported; skipping"
            );
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
            dev.cmd_draw(self.cbuf, 6, 1, 0, 0);
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
        push: PostprocessPush,
    ) -> Result<(), VulkanError> {
        let tex = blur.intermediate();
        let (w, h) = tex.extent();
        let src = Rectangle::<f64, BufferCoord>::from_size(Size::from((w as f64, h as f64)));
        self.render_postprocess(tex, src, dst, push)
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
        mut push: ResizePush,
    ) -> Result<(), VulkanError> {
        if tex_prev.flipped() || tex_next.flipped() {
            tracing::warn!("VulkanFrame::render_resize: flipped textures unsupported; skipping");
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
            dev.cmd_draw(self.cbuf, 6, 1, 0, 0);
        }
        Ok(())
    }

    /// Draw a user **resize** animation shader (niri's `custom_resize`) over two window snapshots
    /// (`tex_prev` at set 0, `tex_next` at set 1). No-op (with a warning) if no custom resize
    /// shader is installed — the built-in crossfade is `render_resize`, so this path is purely
    /// the user override. The caller fills the material fields of `push`; this fills placement.
    // Consumed by the live custom-shader wiring (Stage 3); exercised now by the material test.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn render_custom_resize(
        &mut self,
        tex_prev: &VkTexture,
        tex_next: &VkTexture,
        dst: Rectangle<i32, Physical>,
        mut push: CustomResizePush,
    ) -> Result<(), VulkanError> {
        if tex_prev.flipped() || tex_next.flipped() {
            tracing::warn!("render_custom_resize: flipped textures unsupported; skipping");
            return Ok(());
        }
        push.origin = [dst.loc.x as f32, dst.loc.y as f32];
        push.size = [dst.size.w as f32, dst.size.h as f32];
        push.target = self.target_dims();

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
            dev.cmd_draw(self.cbuf, 6, 1, 0, 0);
        }
        Ok(())
    }

    /// Draw a user **close** or **open** animation shader (niri's `custom_close`/`custom_open`)
    /// over one window snapshot (`texture` at set 0). No-op (with a warning) if that slot has
    /// no shader installed. `ty` must be `Close` or `Open` — resize uses
    /// [`Self::render_custom_resize`].
    // Consumed by the live custom-shader wiring (Stage 3); exercised now by the material test.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn render_custom_anim(
        &mut self,
        ty: CustomShaderType,
        texture: &VkTexture,
        dst: Rectangle<i32, Physical>,
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
        push.origin = [dst.loc.x as f32, dst.loc.y as f32];
        push.size = [dst.size.w as f32, dst.size.h as f32];
        push.target = self.target_dims();

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
            dev.cmd_draw(self.cbuf, 6, 1, 0, 0);
        }
        Ok(())
    }

    /// Record the present-blit into `self.cbuf` (already past `cmd_end_render_pass`): transition
    /// the imported dmabuf to a transfer destination, blit the R8G8B8A8 shadow
    /// (`self.fb.buffer`, left in `TRANSFER_SRC_OPTIMAL` by the render pass) into it —
    /// `vkCmdBlitImage` converts component order, so RGBA lands as the BGRA bytes
    /// `Argb8888`/`Xrgb8888` scanout wants — then leave it in `GENERAL` for the display engine.
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

        unsafe {
            let to_dst = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::UNDEFINED)
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

            let blit = vk::ImageBlit::default()
                .src_subresource(layers)
                .src_offsets([
                    vk::Offset3D { x: 0, y: 0, z: 0 },
                    vk::Offset3D {
                        x: w as i32,
                        y: h as i32,
                        z: 1,
                    },
                ])
                .dst_subresource(layers)
                .dst_offsets([
                    vk::Offset3D { x: 0, y: 0, z: 0 },
                    vk::Offset3D {
                        x: w as i32,
                        y: h as i32,
                        z: 1,
                    },
                ]);
            dev.cmd_blit_image(
                self.cbuf,
                self.fb.buffer.image(),
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                present.image(),
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                std::slice::from_ref(&blit),
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
        // The render pass's `final_layout` leaves the target in TRANSFER_SRC_OPTIMAL (see
        // `create_render_pass`); record it so readback is a no-op and `make_sampleable` knows the
        // source layout for its barrier.
        self.fb
            .buffer
            .set_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL);
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
        let (fb_w, fb_h) = self.fb.buffer.extent();
        let extent = vk::Extent2D {
            width: fb_w,
            height: fb_h,
        };
        let attachment = vk::ClearAttachment {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            color_attachment: 0,
            clear_value: vk::ClearValue {
                color: vk::ClearColorValue {
                    float32: color.components(),
                },
            },
        };
        // Clear the given rects, or the whole target when none were provided.
        // `cmd_clear_attachments` rects are in physical framebuffer space (no projection),
        // but callers pass logical rects (e.g. `render_elements` clears the logical output
        // rect) — so map each through the output transform, exactly as GLES's `clear`
        // reaches the framebuffer via the transform-aware solid draw. Identity for
        // `Normal`.
        let full = vk::ClearRect {
            rect: vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent,
            },
            base_array_layer: 0,
            layer_count: 1,
        };
        let rects: Vec<vk::ClearRect> = if at.is_empty() {
            vec![full]
        } else {
            at.iter()
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
                .collect()
        };
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
        _damage: &[Rectangle<i32, Physical>],
        color: Color32F,
    ) -> Result<(), VulkanError> {
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
            dev.cmd_draw(self.cbuf, 6, 1, 0, 0);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn render_texture_from_to(
        &mut self,
        texture: &VkTexture,
        src: Rectangle<f64, BufferCoord>,
        dst: Rectangle<i32, Physical>,
        _damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        src_transform: Transform,
        alpha: f32,
    ) -> Result<(), VulkanError> {
        // Skeleton scope: `Transform::Normal`, unflipped only. A partial `src` is handled by the
        // shader's `src_rect` remap; other transforms would draw wrong pixels, so degrade to a
        // no-op (a visible gap) rather than lie.
        if src_transform != Transform::Normal || texture.flipped() {
            tracing::warn!(
                "VulkanFrame::render_texture_from_to: unsupported (transform={src_transform:?}, \
                 flipped={}); skipping",
                texture.flipped(),
            );
            return Ok(());
        }

        let push = QuadPush {
            origin: [dst.loc.x as f32, dst.loc.y as f32],
            size: [dst.size.w as f32, dst.size.h as f32],
            proj: self.proj,
            target: self.target_dims(),
            // texture.frag multiplies the sample by this, so white-with-alpha modulates alpha.
            color: [1.0, 1.0, 1.0, alpha],
            src_rect: normalized_src(src, texture),
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
            dev.cmd_draw(self.cbuf, 6, 1, 0, 0);
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

/// Normalize a buffer-space `src` sub-rectangle to `[u0, v0, du, dv]` texture coordinates for the
/// sampling materials' `src_rect` push constant. `[0, 0, 1, 1]` is the full texture.
fn normalized_src(src: Rectangle<f64, BufferCoord>, texture: &VkTexture) -> [f32; 4] {
    let (tw, th) = (texture.width() as f32, texture.height() as f32);
    [
        src.loc.x as f32 / tw,
        src.loc.y as f32 / th,
        src.size.w as f32 / tw,
        src.size.h as f32 / th,
    ]
}
