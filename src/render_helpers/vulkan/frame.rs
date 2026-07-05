use std::fmt;

use ash::vk;
use niri_vk::render::{as_bytes, BorderPush, PostprocessPush, QuadPush, ResizePush, ShadowPush};
use smithay::backend::renderer::sync::SyncPoint;
use smithay::backend::renderer::{Color32F, ContextId, Frame, Texture};
use smithay::utils::{Buffer as BufferCoord, Physical, Rectangle, Size, Transform};

use super::custom::{CustomAnimPush, CustomResizePush, CustomShaderType};
use super::error::VulkanError;
use super::renderer::VulkanRenderer;
use super::types::{VkFramebuffer, VkTexture};

/// An in-progress render into a bound [`VkFramebuffer`]. Records draws into one command buffer
/// begun in [`VulkanFrame::begin`] and submitted (synchronously, fence-waited) in
/// [`Frame::finish`] / on drop.
pub struct VulkanFrame<'frame, 'buffer> {
    renderer: &'frame mut VulkanRenderer,
    fb: &'frame mut VkFramebuffer<'buffer>,
    cbuf: vk::CommandBuffer,
    output_size: Size<i32, Physical>,
    transform: Transform,
    /// `output_size` with `transform` applied (Smithay's convention); == `output_size` for Normal.
    _size: Size<i32, Physical>,
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
            _size: transform.transform_size(output_size),
            finished: false,
        })
    }

    /// Render-target size in pixels, as `[w, h]` floats for the shader's NDC conversion.
    fn target_dims(&self) -> [f32; 2] {
        let (w, h) = self.fb.buffer.extent();
        [w as f32, h as f32]
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
            target: self.target_dims(),
            corner_radius,
            // rounded_texture.frag multiplies the sample by this, so white-with-alpha modulates
            // alpha; the SDF coverage then cuts the corners.
            color: [1.0, 1.0, 1.0, alpha],
            src_rect: normalized_src(src, texture),
            ..Default::default()
        };
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
            target: self.target_dims(),
            color: [1.0, 1.0, 1.0, alpha],
            src_rect: normalized_src(src, texture),
            cutoff: [cutoff.0, cutoff.1],
            ..Default::default()
        };
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
        push.target = self.target_dims();
        push.src_rect = normalized_src(src, texture);

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
        push.target = self.target_dims();

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

    fn finish_internal(&mut self) -> Result<SyncPoint, VulkanError> {
        if self.finished {
            return Ok(SyncPoint::signaled());
        }
        self.finished = true;
        let dev = &self.renderer.gpu.device;
        unsafe {
            dev.cmd_end_render_pass(self.cbuf);
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
                .map(|r| vk::ClearRect {
                    rect: vk::Rect2D {
                        offset: vk::Offset2D {
                            x: r.loc.x,
                            y: r.loc.y,
                        },
                        extent: vk::Extent2D {
                            width: r.size.w.max(0) as u32,
                            height: r.size.h.max(0) as u32,
                        },
                    },
                    base_array_layer: 0,
                    layer_count: 1,
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
            target: self.target_dims(),
            // texture.frag multiplies the sample by this, so white-with-alpha modulates alpha.
            color: [1.0, 1.0, 1.0, alpha],
            src_rect: normalized_src(src, texture),
            ..Default::default()
        };
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

    fn output_size(&self) -> Size<i32, Physical> {
        self.output_size
    }

    fn wait(&mut self, sync: &SyncPoint) -> Result<(), VulkanError> {
        sync.wait().map_err(|_| VulkanError::SyncInterrupted)
    }

    fn finish(mut self) -> Result<SyncPoint, VulkanError> {
        self.finish_internal()
    }
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
