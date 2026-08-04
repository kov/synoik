// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

use std::cell::RefCell;
use std::time::Duration;

use smithay::backend::renderer::element::Kind;
use smithay::utils::{Scale, Transform};

use crate::animation::Clock;
use crate::render_helpers::captured_texture::CapturedTextureRenderElement;
use crate::render_helpers::memory::MemoryBuffer;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::render_helpers::RenderTarget;

pub const DELAY: Duration = Duration::from_millis(250);
pub const DURATION: Duration = Duration::from_millis(500);

/// The frozen screen a transition crossfades from.
///
/// Holds one entry per render target: block-out rules key off the target, so a single shared buffer
/// would show a screencast exactly what block-out exists to hide.
//
/// The screen to crossfade from: renderer-neutral CPU captures, uploaded to `VkTexture`s on
/// demand.
#[derive(Debug)]
struct FrozenScreen {
    buffers: [MemoryBuffer; RenderTarget::COUNT],
    /// `buffers` uploaded to `VkTexture`s, cached across the crossfade's frames. Re-uploading a
    /// full-screen buffer every frame would churn virtio-gpu blobs; the captured contents never
    /// change during a transition, so one upload per target suffices. Uploaded lazily: a session
    /// with no cast never pays for the screencast targets.
    vk: RefCell<[Option<TextureBuffer<VkTexture>>; RenderTarget::COUNT]>,
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
    /// Crossfade from renderer-neutral captures of the frozen screen (a Vulkan session). The owned
    /// renderer can't sample a GLES texture, so no GLES texture is baked in the first place.
    pub fn from_neutrals(
        buffers: [MemoryBuffer; RenderTarget::COUNT],
        scale: Scale<f64>,
        transform: Transform,
        delay: Duration,
        clock: Clock,
    ) -> Self {
        let from = FrozenScreen {
            buffers,
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

    /// The element to crossfade from, or `None` if the frozen screen can't be drawn — a failed
    /// Vulkan upload. Callers must skip the crossfade rather than substitute anything: there is no
    /// second copy of the frozen screen.
    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        target: RenderTarget,
    ) -> Option<CapturedTextureRenderElement> {
        let alpha = self.alpha();
        let idx = target as usize;

        let FrozenScreen { buffers, vk } = &self.from;

        // Upload this target's captured neutral buffer once and draw that.
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
        Some(CapturedTextureRenderElement(
            TextureRenderElement::from_texture_buffer(
                tb,
                (0., 0.),
                alpha,
                None,
                None,
                Kind::Unspecified,
            ),
        ))
    }
}
