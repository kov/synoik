// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The screenshot flash — `Flashspot` (`js/ui/screenshot.js:3153-3179`).
//!
//! A white rectangle over the captured area at full opacity, easing to transparent. It is the only
//! feedback a caller of `org.gnome.Shell.Screenshot.FlashArea` gets that anything happened, which
//! is why the portal calls it separately from the capture itself.

use std::time::Duration;

use smithay::backend::renderer::element::Kind;
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle};

use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};

/// `FLASHSPOT_ANIMATION_OUT_TIME` (`screenshot.js:3153`).
pub const OUT_TIME: Duration = Duration::from_millis(500);

/// `.flashspot { background-color: white }` (`_misc.scss:30`).
const COLOR: [f32; 4] = [1., 1., 1., 1.];

#[derive(Debug, Default)]
pub struct FlashSpot {
    /// The area in *global* logical coordinates, and when the flash started.
    active: Option<(Rectangle<i32, Logical>, Duration)>,
}

impl FlashSpot {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a flash over `area`, replacing any flash already running.
    ///
    /// Replacing rather than queueing is deliberate: two captures in quick succession should look
    /// like two flashes over the newest area, not one flash waiting its turn behind a stale one.
    pub fn fire(&mut self, area: Rectangle<i32, Logical>, now: Duration) {
        self.active = Some((area, now));
    }

    pub fn is_animating(&self, now: Duration) -> bool {
        self.active
            .is_some_and(|(_, start)| now.saturating_sub(start) < OUT_TIME)
    }

    /// Drop a finished flash so it stops being asked about.
    pub fn advance(&mut self, now: Duration) {
        if !self.is_animating(now) {
            self.active = None;
        }
    }

    /// `Clutter.AnimationMode.EASE_OUT_QUAD` from opacity 255 to 0 (`screenshot.js:3168-3171`).
    fn alpha(&self, now: Duration) -> Option<(Rectangle<i32, Logical>, f32)> {
        let (area, start) = self.active?;
        let elapsed = now.saturating_sub(start);
        if elapsed >= OUT_TIME {
            return None;
        }
        let t = elapsed.as_secs_f64() / OUT_TIME.as_secs_f64();
        // ease-out quad on a 1 -> 0 ramp is (1 - t)^2.
        let alpha = (1. - t) * (1. - t);
        Some((area, alpha as f32))
    }

    pub fn render(
        &self,
        output: &Output,
        output_loc: Point<i32, Logical>,
        now: Duration,
        push: &mut dyn FnMut(SolidColorRenderElement),
    ) {
        let Some((area, alpha)) = self.alpha(now) else {
            return;
        };

        // The area is global; this output draws the part of it that lands on this output.
        let size = crate::utils::output_size(output);
        let output_geo = Rectangle::new(output_loc, (size.w as i32, size.h as i32).into());
        let Some(visible) = area.intersection(output_geo) else {
            return;
        };
        let local = Rectangle::new(visible.loc - output_loc, visible.size);

        let buffer = SolidColorBuffer::new(local.size.to_f64(), COLOR);
        push(SolidColorRenderElement::from_buffer(
            &buffer,
            local.loc.to_f64(),
            alpha,
            Kind::Unspecified,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The flash starts opaque and ends gone, and its curve is ease-out — most of the fade happens
    /// early, which is what makes it read as a camera flash rather than a slow white wash.
    #[test]
    fn the_flash_fades_out_fast_then_slow() {
        let mut flash = FlashSpot::new();
        let area = Rectangle::new(Point::from((0, 0)), (100, 100).into());
        flash.fire(area, Duration::ZERO);

        let alpha = |ms| flash.alpha(Duration::from_millis(ms)).map(|(_, a)| a);

        assert_eq!(alpha(0), Some(1.), "starts fully opaque");
        assert_eq!(alpha(500), None, "and is gone at the end");
        assert_eq!(alpha(10_000), None, "and stays gone");

        let quarter = alpha(125).unwrap();
        let half = alpha(250).unwrap();
        assert!(
            (quarter - 0.5625).abs() < 1e-5,
            "ease-out quad at t=0.25, got {quarter}"
        );
        assert!(
            half < 0.5,
            "more than half the fade is done by halfway, got {half}"
        );
    }

    /// A flash fired while one is running replaces it, rather than being dropped or queued.
    #[test]
    fn a_second_flash_replaces_the_first() {
        let mut flash = FlashSpot::new();
        let first = Rectangle::new(Point::from((0, 0)), (10, 10).into());
        let second = Rectangle::new(Point::from((50, 50)), (20, 20).into());

        flash.fire(first, Duration::ZERO);
        flash.fire(second, Duration::from_millis(250));

        let (area, alpha) = flash.alpha(Duration::from_millis(250)).unwrap();
        assert_eq!(area, second);
        assert_eq!(alpha, 1., "the new flash starts from the top");
        assert!(flash.is_animating(Duration::from_millis(700)));
    }
}
