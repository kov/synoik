use std::fmt;

use ash::vk;
use niri_vk::render::{as_bytes, QuadPush};
use smithay::backend::renderer::sync::SyncPoint;
use smithay::backend::renderer::{Color32F, ContextId, Frame, Texture};
use smithay::utils::{Buffer as BufferCoord, Physical, Rectangle, Size, Transform};

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
        let extent = vk::Extent2D {
            width: fb.buffer.width,
            height: fb.buffer.height,
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
            let render_area = vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 0 },
                extent,
            };
            // Load op is DONT_CARE (see renderer::create_render_pass); callers clear explicitly.
            let pass_begin = vk::RenderPassBeginInfo::default()
                .render_pass(renderer.render_pass)
                .framebuffer(fb.buffer.framebuffer)
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
        [self.fb.buffer.width as f32, self.fb.buffer.height as f32]
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
        let extent = vk::Extent2D {
            width: self.fb.buffer.width,
            height: self.fb.buffer.height,
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
            corner_radius: 0.0,
            _pad0: 0.0,
            color: color.components(),
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
        // Skeleton scope: full-`src`, `Transform::Normal`, unflipped only. Anything else would
        // draw wrong pixels, so degrade to a no-op (a visible gap) rather than lie.
        let full_src = src.loc.x == 0.0
            && src.loc.y == 0.0
            && src.size.w as u32 == texture.width()
            && src.size.h as u32 == texture.height();
        if src_transform != Transform::Normal || texture.flipped() || !full_src {
            tracing::warn!(
                "VulkanFrame::render_texture_from_to: unsupported (transform={src_transform:?}, \
                 flipped={}, full_src={full_src}); skipping",
                texture.flipped(),
            );
            return Ok(());
        }

        let push = QuadPush {
            origin: [dst.loc.x as f32, dst.loc.y as f32],
            size: [dst.size.w as f32, dst.size.h as f32],
            target: self.target_dims(),
            corner_radius: 0.0,
            _pad0: 0.0,
            // texture.frag multiplies the sample by this, so white-with-alpha modulates alpha.
            color: [1.0, 1.0, 1.0, alpha],
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
