#[cfg(feature = "vulkan")]
use std::cell::RefCell;
use std::time::Duration;

use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::GlesTexture;
use smithay::utils::{Scale, Transform};

use crate::animation::Clock;
use crate::render_helpers::dual_texture::DualTextureRenderElement;
use crate::render_helpers::memory::MemoryBuffer;
use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use crate::render_helpers::renderer::NiriRenderer;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
#[cfg(feature = "vulkan")]
use crate::render_helpers::vulkan::VkTexture;
use crate::render_helpers::RenderTarget;

pub const DELAY: Duration = Duration::from_millis(250);
pub const DURATION: Duration = Duration::from_millis(500);

#[derive(Debug)]
pub struct ScreenTransition {
    /// Texture to crossfade from for each render target.
    from_texture: [TextureBuffer<GlesTexture>; 3],
    /// The `RenderTarget::Output` contents captured into a renderer-neutral CPU buffer, populated
    /// only on Vulkan sessions (the owned Vulkan renderer can't sample the GLES textures above).
    /// `None` on GLES, where `from_texture` is used directly. The screencast/screen-capture
    /// targets still render through GLES, so only the Output slot needs this.
    #[cfg_attr(not(feature = "vulkan"), allow(dead_code))]
    output_neutral: Option<MemoryBuffer>,
    /// `output_neutral` uploaded once to a `VkTexture`, cached across the crossfade's frames.
    /// Re-uploading a full-screen buffer every frame would churn virtio-gpu blobs; the captured
    /// contents never change during a transition, so one upload suffices.
    #[cfg(feature = "vulkan")]
    output_vk: RefCell<Option<TextureBuffer<VkTexture>>>,
    /// Monotonic time when to start the crossfade.
    start_at: Duration,
    /// Clock to drive animations.
    clock: Clock,
}

impl ScreenTransition {
    pub fn new(
        from_texture: [TextureBuffer<GlesTexture>; 3],
        output_neutral: Option<MemoryBuffer>,
        delay: Duration,
        clock: Clock,
    ) -> Self {
        Self {
            from_texture,
            output_neutral,
            #[cfg(feature = "vulkan")]
            output_vk: RefCell::new(None),
            start_at: clock.now_unadjusted() + delay,
            clock,
        }
    }

    pub fn is_done(&self) -> bool {
        self.start_at + DURATION <= self.clock.now_unadjusted()
    }

    pub fn update_render_elements(&mut self, scale: Scale<f64>, transform: Transform) {
        // These textures should remain full-screen, even if scale or transform changes.
        for buffer in &mut self.from_texture {
            buffer.set_texture_scale(scale);
            buffer.set_texture_transform(transform);
        }
    }

    fn alpha(&self) -> f32 {
        // Screen transition ignores animation slowdown.
        let now = self.clock.now_unadjusted();

        if self.start_at + DURATION <= now {
            0.
        } else if self.start_at <= now {
            1. - (now - self.start_at).as_secs_f32() / DURATION.as_secs_f32()
        } else {
            1.
        }
    }

    #[cfg_attr(not(feature = "vulkan"), allow(unused_variables))]
    pub fn render<R: NiriRenderer>(
        &self,
        renderer: &mut R,
        target: RenderTarget,
    ) -> DualTextureRenderElement {
        let alpha = self.alpha();

        // On a Vulkan session the Output target composites through the owned renderer, which can't
        // sample the GLES `from_texture`. Upload the captured neutral buffer once and draw that.
        #[cfg(feature = "vulkan")]
        if target == RenderTarget::Output {
            if let Some(neutral) = &self.output_neutral {
                if let Some(vk) = renderer.try_as_vulkan_renderer() {
                    if self.output_vk.borrow().is_none() {
                        match TextureBuffer::from_memory_buffer(vk, neutral) {
                            Ok(tb) => *self.output_vk.borrow_mut() = Some(tb),
                            Err(err) => {
                                warn!("error uploading screen transition to Vulkan: {err:?}")
                            }
                        }
                    }

                    if let Some(mut tb) = self.output_vk.borrow().clone() {
                        // Keep the uploaded texture full-screen under the current scale/transform.
                        tb.set_texture_scale(self.from_texture[0].texture_scale());
                        tb.set_texture_transform(self.from_texture[0].texture_transform());
                        return DualTextureRenderElement::Vulkan(
                            TextureRenderElement::from_texture_buffer(
                                tb,
                                (0., 0.),
                                alpha,
                                None,
                                None,
                                Kind::Unspecified,
                            ),
                        );
                    }
                }
            }
        }

        let idx = match target {
            RenderTarget::Output => 0,
            RenderTarget::Screencast => 1,
            RenderTarget::ScreenCapture => 2,
        };

        DualTextureRenderElement::Gles(PrimaryGpuTextureRenderElement(
            TextureRenderElement::from_texture_buffer(
                self.from_texture[idx].clone(),
                (0., 0.),
                alpha,
                None,
                None,
                Kind::Unspecified,
            ),
        ))
    }
}
