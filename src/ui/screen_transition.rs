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

/// The frozen screen a transition crossfades from, in whichever form the active renderer can
/// sample. Exactly one form is live — an enum rather than a pair of `Option`s so that every
/// consumer has to handle the Vulkan arm: a `DualTextureRenderElement::Gles` drawn on the owned
/// Vulkan renderer is a silent no-op, so "forgot the Vulkan arm" must not compile.
///
/// Both variants hold one entry per render target: block-out rules key off the target, so a single
/// shared buffer would show a screencast exactly what block-out exists to hide.
#[derive(Debug)]
enum FrozenScreen {
    /// GLES textures, sampled directly.
    Gles([TextureBuffer<GlesTexture>; RenderTarget::COUNT]),
    /// Renderer-neutral CPU captures, uploaded to `VkTexture`s on demand. The owned Vulkan
    /// renderer can't sample a GLES texture, so a Vulkan session captures the frozen screen
    /// this way.
    Neutral {
        buffers: [MemoryBuffer; RenderTarget::COUNT],
        /// `buffers` uploaded to `VkTexture`s, cached across the crossfade's frames.
        /// Re-uploading a full-screen buffer every frame would churn virtio-gpu blobs; the
        /// captured contents never change during a transition, so one upload per target
        /// suffices. Uploaded lazily: a session with no cast never pays for the screencast
        /// targets.
        #[cfg(feature = "vulkan")]
        vk: RefCell<[Option<TextureBuffer<VkTexture>>; RenderTarget::COUNT]>,
    },
}

#[derive(Debug)]
pub struct ScreenTransition {
    /// The screen to crossfade from.
    from: FrozenScreen,
    /// The output's current scale and transform. The frozen screen must stay full-screen even if
    /// these change mid-crossfade, so they're re-applied to the sampled texture every frame rather
    /// than taken from the buffer's capture-time values.
    scale: Scale<f64>,
    transform: Transform,
    /// Monotonic time when to start the crossfade.
    start_at: Duration,
    /// Clock to drive animations.
    clock: Clock,
}

impl ScreenTransition {
    /// Crossfade from GLES textures of the frozen screen (a GLES session).
    pub fn from_gles(
        from_texture: [TextureBuffer<GlesTexture>; RenderTarget::COUNT],
        scale: Scale<f64>,
        transform: Transform,
        delay: Duration,
        clock: Clock,
    ) -> Self {
        Self::new(
            FrozenScreen::Gles(from_texture),
            scale,
            transform,
            delay,
            clock,
        )
    }

    /// Crossfade from renderer-neutral captures of the frozen screen (a Vulkan session). The owned
    /// renderer can't sample a GLES texture, so no GLES texture is baked in the first place.
    pub fn from_neutrals(
        buffers: [MemoryBuffer; RenderTarget::COUNT],
        scale: Scale<f64>,
        transform: Transform,
        delay: Duration,
        clock: Clock,
    ) -> Self {
        let from = FrozenScreen::Neutral {
            buffers,
            #[cfg(feature = "vulkan")]
            vk: RefCell::new(Default::default()),
        };
        Self::new(from, scale, transform, delay, clock)
    }

    fn new(
        from: FrozenScreen,
        scale: Scale<f64>,
        transform: Transform,
        delay: Duration,
        clock: Clock,
    ) -> Self {
        Self {
            from,
            scale,
            transform,
            start_at: clock.now_unadjusted() + delay,
            clock,
        }
    }

    pub fn is_done(&self) -> bool {
        self.start_at + DURATION <= self.clock.now_unadjusted()
    }

    pub fn update_render_elements(&mut self, scale: Scale<f64>, transform: Transform) {
        self.scale = scale;
        self.transform = transform;

        // These textures should remain full-screen, even if scale or transform changes.
        if let FrozenScreen::Gles(from_texture) = &mut self.from {
            for buffer in from_texture {
                buffer.set_texture_scale(scale);
                buffer.set_texture_transform(transform);
            }
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

    /// The element to crossfade from, or `None` if the frozen screen can't be drawn on `renderer` —
    /// a failed Vulkan upload, or a neutral capture asked to draw on GLES. Callers must skip the
    /// crossfade rather than substitute anything: there is no second copy of the frozen screen.
    #[cfg_attr(not(feature = "vulkan"), allow(unused_variables))]
    pub fn render<R: NiriRenderer>(
        &self,
        renderer: &mut R,
        target: RenderTarget,
    ) -> Option<DualTextureRenderElement> {
        let alpha = self.alpha();
        let idx = target as usize;

        let from_texture = match &self.from {
            FrozenScreen::Gles(from_texture) => from_texture,

            // Upload this target's captured neutral buffer once and draw that.
            #[cfg_attr(not(feature = "vulkan"), allow(unused_variables))]
            FrozenScreen::Neutral {
                buffers,
                #[cfg(feature = "vulkan")]
                vk,
            } => {
                #[cfg(feature = "vulkan")]
                if let Some(renderer) = renderer.try_as_vulkan_renderer() {
                    if vk.borrow()[idx].is_none() {
                        match TextureBuffer::from_memory_buffer(renderer, &buffers[idx]) {
                            Ok(tb) => vk.borrow_mut()[idx] = Some(tb),
                            Err(err) => {
                                warn!("error uploading screen transition to Vulkan: {err:?}")
                            }
                        }
                    }

                    let mut tb = vk.borrow()[idx].clone()?;
                    // Keep the uploaded texture full-screen under the current scale/transform.
                    tb.set_texture_scale(self.scale);
                    tb.set_texture_transform(self.transform);
                    return Some(DualTextureRenderElement::Vulkan(
                        TextureRenderElement::from_texture_buffer(
                            tb,
                            (0., 0.),
                            alpha,
                            None,
                            None,
                            Kind::Unspecified,
                        ),
                    ));
                }

                // A neutral capture is only taken on a Vulkan session, and there it is the only
                // copy of the frozen screen — a GLES renderer can't draw it. Don't fall through to
                // the Gles arm below: it would sample whatever `from_texture` we don't have.
                return None;
            }
        };

        Some(DualTextureRenderElement::Gles(
            PrimaryGpuTextureRenderElement(TextureRenderElement::from_texture_buffer(
                from_texture[idx].clone(),
                (0., 0.),
                alpha,
                None,
                None,
                Kind::Unspecified,
            )),
        ))
    }
}
