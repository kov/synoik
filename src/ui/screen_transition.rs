#[cfg(feature = "vulkan")]
use std::cell::RefCell;
use std::time::Duration;

use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::gles::{GlesError, GlesFrame, GlesRenderer, GlesTexture};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Physical, Rectangle, Scale, Transform};

use crate::animation::Clock;
use crate::backend::tty::{TtyFrame, TtyRenderer, TtyRendererError};
use crate::render_helpers::memory::MemoryBuffer;
use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use crate::render_helpers::renderer::NiriRenderer;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
#[cfg(feature = "vulkan")]
use crate::render_helpers::vulkan::{VkTexture, VulkanError, VulkanFrame, VulkanRenderer};
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
    ) -> ScreenTransitionRenderElement {
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
                        return ScreenTransitionRenderElement::Vulkan(
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

        ScreenTransitionRenderElement::Gles(PrimaryGpuTextureRenderElement(
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

/// The screen-transition crossfade element. On GLES/Tty it draws the captured GLES texture; on the
/// owned Vulkan renderer it draws the neutral capture uploaded to a `VkTexture`. A concrete enum
/// (like [`ResizeRenderElement`](crate::render_helpers::resize::ResizeRenderElement)) so it can be
/// a single `OutputRenderElements<R>` arm regardless of `R`.
#[derive(Debug)]
pub enum ScreenTransitionRenderElement {
    Gles(PrimaryGpuTextureRenderElement),
    #[cfg(feature = "vulkan")]
    Vulkan(TextureRenderElement<VkTexture>),
}

impl Element for ScreenTransitionRenderElement {
    fn id(&self) -> &Id {
        match self {
            ScreenTransitionRenderElement::Gles(e) => e.id(),
            #[cfg(feature = "vulkan")]
            ScreenTransitionRenderElement::Vulkan(e) => e.id(),
        }
    }

    fn current_commit(&self) -> CommitCounter {
        match self {
            ScreenTransitionRenderElement::Gles(e) => e.current_commit(),
            #[cfg(feature = "vulkan")]
            ScreenTransitionRenderElement::Vulkan(e) => e.current_commit(),
        }
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        match self {
            ScreenTransitionRenderElement::Gles(e) => e.geometry(scale),
            #[cfg(feature = "vulkan")]
            ScreenTransitionRenderElement::Vulkan(e) => e.geometry(scale),
        }
    }

    fn transform(&self) -> Transform {
        match self {
            ScreenTransitionRenderElement::Gles(e) => e.transform(),
            #[cfg(feature = "vulkan")]
            ScreenTransitionRenderElement::Vulkan(e) => e.transform(),
        }
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        match self {
            ScreenTransitionRenderElement::Gles(e) => e.src(),
            #[cfg(feature = "vulkan")]
            ScreenTransitionRenderElement::Vulkan(e) => e.src(),
        }
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        match self {
            ScreenTransitionRenderElement::Gles(e) => e.damage_since(scale, commit),
            #[cfg(feature = "vulkan")]
            ScreenTransitionRenderElement::Vulkan(e) => e.damage_since(scale, commit),
        }
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        match self {
            ScreenTransitionRenderElement::Gles(e) => e.opaque_regions(scale),
            #[cfg(feature = "vulkan")]
            ScreenTransitionRenderElement::Vulkan(e) => e.opaque_regions(scale),
        }
    }

    fn alpha(&self) -> f32 {
        match self {
            ScreenTransitionRenderElement::Gles(e) => e.alpha(),
            #[cfg(feature = "vulkan")]
            ScreenTransitionRenderElement::Vulkan(e) => e.alpha(),
        }
    }

    fn kind(&self) -> Kind {
        match self {
            ScreenTransitionRenderElement::Gles(e) => e.kind(),
            #[cfg(feature = "vulkan")]
            ScreenTransitionRenderElement::Vulkan(e) => e.kind(),
        }
    }
}

impl RenderElement<GlesRenderer> for ScreenTransitionRenderElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        match self {
            ScreenTransitionRenderElement::Gles(e) => RenderElement::<GlesRenderer>::draw(
                e,
                frame,
                src,
                dst,
                damage,
                opaque_regions,
                cache,
            ),
            // The Vulkan arm is never constructed on a GLES session.
            #[cfg(feature = "vulkan")]
            ScreenTransitionRenderElement::Vulkan(_) => Ok(()),
        }
    }

    fn underlying_storage(&self, renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        match self {
            ScreenTransitionRenderElement::Gles(e) => e.underlying_storage(renderer),
            #[cfg(feature = "vulkan")]
            ScreenTransitionRenderElement::Vulkan(_) => None,
        }
    }
}

impl<'render> RenderElement<TtyRenderer<'render>> for ScreenTransitionRenderElement {
    fn draw(
        &self,
        frame: &mut TtyFrame<'_, '_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), TtyRendererError<'render>> {
        match self {
            ScreenTransitionRenderElement::Gles(e) => RenderElement::<TtyRenderer>::draw(
                e,
                frame,
                src,
                dst,
                damage,
                opaque_regions,
                cache,
            ),
            #[cfg(feature = "vulkan")]
            ScreenTransitionRenderElement::Vulkan(_) => Ok(()),
        }
    }

    fn underlying_storage(
        &self,
        renderer: &mut TtyRenderer<'render>,
    ) -> Option<UnderlyingStorage<'_>> {
        match self {
            ScreenTransitionRenderElement::Gles(e) => e.underlying_storage(renderer),
            #[cfg(feature = "vulkan")]
            ScreenTransitionRenderElement::Vulkan(_) => None,
        }
    }
}

#[cfg(feature = "vulkan")]
impl RenderElement<VulkanRenderer> for ScreenTransitionRenderElement {
    fn draw(
        &self,
        frame: &mut VulkanFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), VulkanError> {
        match self {
            // Degraded no-op on Vulkan (PrimaryGpuTextureRenderElement), only reached if the
            // neutral upload failed and we fell back to the GLES texture.
            ScreenTransitionRenderElement::Gles(e) => RenderElement::<VulkanRenderer>::draw(
                e,
                frame,
                src,
                dst,
                damage,
                opaque_regions,
                cache,
            ),
            ScreenTransitionRenderElement::Vulkan(e) => RenderElement::<VulkanRenderer>::draw(
                e,
                frame,
                src,
                dst,
                damage,
                opaque_regions,
                cache,
            ),
        }
    }

    fn underlying_storage(&self, _renderer: &mut VulkanRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}
