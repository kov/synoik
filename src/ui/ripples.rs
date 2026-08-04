// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The hot-corner ripple — `Ripples` (`js/ui/ripples.js`) styled `.ripple-box`
//! (`_corner-ripple.scss`).
//!
//! Three translucent quarter-discs pinned to the corner, expanding and fading on staggered
//! delays. GNOME animates one St actor per ripple, scaling it about a pivot at the corner; we
//! bake the disc once and magnify it by tagging the buffer at a fraction of the output scale,
//! which is the same picture with one texture.

use std::cell::RefCell;
use std::time::Duration;

use smithay::backend::renderer::element::Kind;
use smithay::output::Output;
use smithay::utils::{Logical, Physical, Point, Size, Transform};

use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::ui::widget::{bake, BakeCache, Painter, Rgba};

/// `$ripple_size` (`_corner-ripple.scss:3`).
const RIPPLE_SIZE: f64 = 50.;

/// `.ripple-box` is `$ripple_size + 2px` square, "plus + 2px for the border (box-shadow)"
/// (`_corner-ripple.scss:8-10`). Its `border-radius` on the far corner equals that full size,
/// so the box *is* a quarter-disc of this radius pinned to the corner.
const BOX_SIZE: f64 = RIPPLE_SIZE + 2.;

/// `background-color: rgba(255,255,255,0.2)` (`_corner-ripple.scss:6`).
const FILL: Rgba = [1., 1., 1., 0.2];

/// `box-shadow: 0 0 2px 2px rgba(255,255,255,0.2)` (`_corner-ripple.scss:7`) — no offset,
/// 2 px blur, 2 px spread, the same white.
const SHADOW_BLUR: f64 = 2.;
const SHADOW_SPREAD: f64 = 2.;

/// The largest `finalScale` any wave reaches (`ripples.js:108`). One texture serves all three
/// waves, magnified per wave, so it is baked at the largest of them: magnifying past the size a
/// shape was rasterized at is what softens its edge, and this way none of them does.
const MAX_SCALE: f64 = 1.5;

/// One of the three concentric ripples. "The exact parameters were found by trial and error, so
/// don't look for them to make perfect sense mathematically" (`ripples.js:103-110`).
struct Wave {
    delay: Duration,
    duration: Duration,
    start_scale: f64,
    start_opacity: f64,
    final_scale: f64,
}

const fn wave(
    delay: u64,
    duration: u64,
    start_scale: f64,
    start_opacity: f64,
    final_scale: f64,
) -> Wave {
    Wave {
        delay: Duration::from_millis(delay),
        duration: Duration::from_millis(duration),
        start_scale,
        start_opacity,
        final_scale,
    }
}

//                        delay  time  scale  opacity  => scale
const WAVES: [Wave; 3] = [
    wave(0, 830, 0.25, 1.0, 1.5),
    wave(50, 1000, 0.0, 0.7, 1.25),
    wave(350, 1000, 0.0, 0.3, 1.0),
];

/// How long the whole animation runs: the last wave's delay plus its duration.
pub const DURATION: Duration = Duration::from_millis(350 + 1000);

#[derive(Default)]
pub struct Ripples {
    /// The corner in *global* logical coordinates, and when the animation started.
    active: Option<(Point<f64, Logical>, Duration)>,
    cache: RefCell<BakeCache>,
}

impl Ripples {
    pub fn new() -> Self {
        Self::default()
    }

    /// Play the ripple at `corner` (global logical coordinates), restarting any ripple already
    /// running. gnome-shell only plays it when the toggle actually started an overview animation
    /// (`HotCorner._toggleOverview`, `layout.js:1253-1255`).
    pub fn play(&mut self, corner: Point<f64, Logical>, now: Duration) {
        self.active = Some((corner, now));
    }

    pub fn is_animating(&self, now: Duration) -> bool {
        self.active
            .is_some_and(|(_, start)| now.saturating_sub(start) < DURATION)
    }

    /// Drop a finished ripple so it stops being asked about.
    pub fn advance(&mut self, now: Duration) {
        if !self.is_animating(now) {
            self.active = None;
        }
    }

    /// Each wave's `(scale, alpha)` at `now`, skipping the ones that haven't started or have
    /// finished.
    fn waves(&self, now: Duration) -> impl Iterator<Item = (f64, f32)> + '_ {
        let elapsed = self
            .active
            .map(|(_, start)| now.saturating_sub(start))
            .unwrap_or(Duration::MAX);

        WAVES.iter().filter_map(move |w| {
            let t = elapsed.checked_sub(w.delay)?.as_secs_f64() / w.duration.as_secs_f64();
            if t >= 1. {
                return None;
            }

            // Scale runs `EASE_OUT_QUAD`, opacity `EASE_IN_QUAD` to zero: linear motion reads as
            // unrealistic, and a linear fade disappears too early to see the middle of the travel
            // (`ripples.js:48-55`).
            let scale =
                w.start_scale + (w.final_scale - w.start_scale) * (1. - (1. - t) * (1. - t));
            let alpha = w.start_opacity.sqrt() * (1. - t * t);
            Some((scale, alpha as f32))
        })
    }

    /// The quarter-disc, baked once per output scale, at [`MAX_SCALE`].
    ///
    /// Both shapes are drawn as rounded rects centred on the buffer's top-left corner, so only
    /// their bottom-right quadrant lands inside the buffer — the same "let the rect run past the
    /// bake buffer on the side that should stay square" idiom the rest of the toolkit uses for a
    /// per-corner `border-radius`.
    fn texture(&self, renderer: &mut VulkanRenderer, scale: f64) -> anyhow::Result<VkTexture> {
        // The shadow's fringe reaches ~3σ past the spread box; pad so it doesn't clip.
        let pad = SHADOW_BLUR * 1.5 + SHADOW_SPREAD + 1.;
        let logical = Size::<f64, Logical>::from((BOX_SIZE + pad, BOX_SIZE + pad));
        let bake_scale = scale * MAX_SCALE;

        bake(
            renderer,
            &mut self.cache.borrow_mut(),
            bake_scale,
            logical,
            0,
            |_| Ok(()),
            |frame, phys: Size<i32, Physical>, ()| {
                let mut p = Painter::new(frame, bake_scale, phys);
                p.clear(crate::ui::widget::style::TRANSPARENT)?;

                let disc = |radius: f64| {
                    smithay::utils::Rectangle::new(
                        Point::from((-radius, -radius)),
                        Size::from((radius * 2., radius * 2.)),
                    )
                };

                let spread = BOX_SIZE + SHADOW_SPREAD;
                p.drop_shadow(disc(spread), spread, SHADOW_BLUR, (0., 0.), FILL)?;
                p.fill_rounded(disc(BOX_SIZE), BOX_SIZE, FILL)?;
                Ok(())
            },
        )
    }

    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        output: &Output,
        output_loc: Point<i32, Logical>,
        now: Duration,
        push: &mut dyn FnMut(TextureRenderElement<VkTexture>),
    ) {
        let Some((corner, _)) = self.active else {
            return;
        };
        if !self.is_animating(now) {
            return;
        }

        // The corner belongs to one output; the others draw nothing.
        let size = crate::utils::output_size(output);
        let local = corner - output_loc.to_f64();
        if local.x < 0. || local.y < 0. || local.x >= size.w || local.y >= size.h {
            return;
        }

        let scale = output.current_scale().fractional_scale();
        let texture = match self.texture(renderer, scale) {
            Ok(texture) => texture,
            Err(err) => {
                warn!("error baking the hot corner ripple: {err:?}");
                return;
            }
        };

        // Topmost first: the third wave is the one gnome-shell raises above the others
        // (`ripples.js:99-101`), so it is pushed first.
        for (wave_scale, alpha) in self.waves(now).collect::<Vec<_>>().into_iter().rev() {
            if wave_scale <= 0. || alpha <= 0. {
                continue;
            }
            // Dividing the buffer scale magnifies: a buffer tagged at a fraction of the output
            // scale covers correspondingly more logical area, pinned at its top-left — which is
            // the corner, and so is the pivot gnome-shell scales about.
            let buffer = TextureBuffer::from_texture(
                renderer,
                texture.clone(),
                scale * MAX_SCALE / wave_scale,
                Transform::Normal,
                Vec::new(),
            );
            push(TextureRenderElement::from_texture_buffer(
                buffer,
                local,
                alpha,
                None,
                None,
                Kind::Unspecified,
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    /// The three waves are staggered: only the first is on screen at the start, and the last
    /// outlives the other two (`ripples.js:107-110`).
    #[test]
    fn the_waves_are_staggered() {
        let mut r = Ripples::new();
        r.play(Point::from((0., 0.)), Duration::ZERO);

        assert_eq!(r.waves(at(0)).count(), 1, "only the first wave starts at 0");
        assert_eq!(r.waves(at(60)).count(), 2, "the second joins at 50 ms");
        assert_eq!(r.waves(at(400)).count(), 3, "the third at 350 ms");
        assert_eq!(r.waves(at(900)).count(), 2, "the first ends at 830 ms");
        assert_eq!(r.waves(at(1100)).count(), 1, "the second at 1050 ms");
        assert_eq!(r.waves(at(1400)).count(), 0, "the third at 1350 ms");
    }

    /// Every wave grows and fades, ending invisible — never the other way round.
    #[test]
    fn the_waves_expand_and_fade() {
        let mut r = Ripples::new();
        r.play(Point::from((0., 0.)), Duration::ZERO);

        let first = |ms| r.waves(at(ms)).next().unwrap();

        let (scale_0, alpha_0) = first(0);
        let (scale_mid, alpha_mid) = first(400);
        assert!(
            (scale_0 - 0.25).abs() < 1e-9,
            "the first wave starts at 0.25, got {scale_0}"
        );
        assert_eq!(alpha_0, 1., "and at full opacity (sqrt(1.0))");
        assert!(scale_mid > scale_0, "it expands");
        assert!(alpha_mid < alpha_0, "while fading");

        let (scale_end, alpha_end) = first(829);
        assert!(scale_end < 1.5, "it never overshoots its final scale");
        assert!(alpha_end < 0.01, "and is all but gone at the end");
    }

    /// The opacity curve is ease-*in*, so the ripple stays visible through the middle of its
    /// travel rather than vanishing immediately (`ripples.js:50-55`).
    #[test]
    fn the_fade_is_back_loaded() {
        let mut r = Ripples::new();
        r.play(Point::from((0., 0.)), Duration::ZERO);

        let alpha = |ms| r.waves(at(ms)).next().unwrap().1;
        assert!(
            alpha(415) > 0.7,
            "more than 70% of the opacity survives to halfway, got {}",
            alpha(415)
        );
    }

    /// A second trigger restarts the animation rather than being ignored or queued.
    #[test]
    fn a_second_ripple_restarts_the_animation() {
        let mut r = Ripples::new();
        r.play(Point::from((0., 0.)), Duration::ZERO);
        assert!(!r.is_animating(at(2000)));

        r.play(Point::from((1920., 0.)), at(2000));
        assert!(r.is_animating(at(2000)));
        assert_eq!(
            r.waves(at(2000)).count(),
            1,
            "starts over from the first wave"
        );
        assert!(r.is_animating(at(3000)));
    }
}
