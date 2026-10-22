// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The system bell's visual half — mutter's `MetaBell` (`src/core/bell.c`) and the flash it asks
//! the compositor for (`meta_compositor_flash_display` / `_flash_window`,
//! `src/compositor/compositor.c:1350-1438`).
//!
//! A bell comes from a client (`xdg_system_bell_v1.ring`, or GTK3's `gtk_shell1.system_bell`).
//! With `visual-bell` on, it flashes the whole screen or, under `visual-bell-type = frame-flash`,
//! the ringing window — a black overlay easing in to 75% and back out. The audible half
//! (`audible-bell`) is [`crate::synoik::State::ring_bell`]'s call into
//! [`crate::sound_player`].
//!
//! **Divergence:** a double flash (the first bell, or one after a 3 s quiet) is two complete
//! flashes here. mutter asks Clutter for `repeat_count = 2` with auto-reverse, which is *three*
//! 50 ms passes (`clutter-timeline.c:1141`: it stops once `current_repeat` reaches the count,
//! counting from 0), and destroys the overlay when the third pass ends — at peak darkness, so its
//! second flash snaps off instead of fading. Kept as two whole flashes by Gustavo's call.

use std::time::Duration;

/// `MIN_TIME_BETWEEN_VISUAL_ALERTS_MS` (`bell.c:31`): at most 2 Hz, under WCAG's 3 Hz
/// photosensitivity limit, and global rather than per window (`bell.c:117-143`).
pub const MIN_TIME_BETWEEN_VISUAL_ALERTS: Duration = Duration::from_millis(500);

/// `MIN_TIME_BETWEEN_DOUBLE_VISUAL_ALERT_MS` (`bell.c:32`): a bell after this long a quiet
/// flashes twice.
pub const MIN_TIME_BETWEEN_DOUBLE_VISUAL_ALERT: Duration = Duration::from_millis(3000);

/// `FLASH_TIME_MS` (`compositor.c:1350`): one pass, clear to dark or back.
pub const FLASH_TIME: Duration = Duration::from_millis(50);

/// The overlay's peak: black at opacity 192 (`compositor.c:1376,1384`).
pub const PEAK_ALPHA: f32 = 192. / 255.;

/// `org.gnome.desktop.wm.preferences visual-bell-type` (`GDesktopVisualBellType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VisualBellType {
    /// Flash the whole screen. The schema default.
    #[default]
    FullscreenFlash,
    /// Flash the ringing window, or the whole screen when the bell names none
    /// (`bell_flash_frame`, `bell.c:91-99`).
    FrameFlash,
}

/// The rate limiter `bell_visual_notify` keeps in `display->last_visual_bell_time_us`.
#[derive(Debug, Default)]
pub struct VisualBell {
    /// When the last bell that actually flashed rang. A gated bell does not move it
    /// (`bell.c:130-133`), so a steady stream of bells flashes every 500 ms, not never.
    last: Option<Duration>,
}

impl VisualBell {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ring at `now`: the number of flashes owed, or `None` when gated. mutter's timestamp
    /// starts at 0 against a monotonic clock that is far past 3 s, so the first bell ever is a
    /// double flash; `None` stands in for that here.
    pub fn ring(&mut self, now: Duration) -> Option<u32> {
        let since = self.last.map(|last| now.saturating_sub(last));
        if since.is_some_and(|since| since < MIN_TIME_BETWEEN_VISUAL_ALERTS) {
            return None;
        }
        self.last = Some(now);
        Some(match since {
            Some(since) if since < MIN_TIME_BETWEEN_DOUBLE_VISUAL_ALERT => 1,
            _ => 2,
        })
    }
}

/// One running flash: `n_flashes` dark-and-back cycles of [`FLASH_TIME`] each way.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BellFlash {
    start: Duration,
    n_flashes: u32,
}

impl BellFlash {
    pub fn new(start: Duration, n_flashes: u32) -> Self {
        Self { start, n_flashes }
    }

    /// The overlay's alpha at `now`, or `None` once the flash is over. Each pass is
    /// `EASE_IN_QUAD` on the timeline's progress (`compositor.c:1382`); a reversed pass runs the
    /// same timeline backwards, so the way out is the mirror of the way in.
    pub fn alpha(&self, now: Duration) -> Option<f32> {
        let elapsed = now.checked_sub(self.start)?;
        let pass_ns = FLASH_TIME.as_nanos();
        let pass = elapsed.as_nanos() / pass_ns;
        if pass >= u128::from(self.n_flashes) * 2 {
            return None;
        }
        let p = (elapsed.as_nanos() % pass_ns) as f64 / pass_ns as f64;
        let progress = if pass.is_multiple_of(2) { p } else { 1. - p };
        Some(PEAK_ALPHA * (progress * progress) as f32)
    }

    pub fn is_animating(&self, now: Duration) -> bool {
        self.alpha(now).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    /// The first bell is a double flash; one within 3 s of the last is single; one within
    /// 500 ms is dropped and does not reset the clock it was measured against.
    #[test]
    fn bells_are_rate_limited_and_doubled_after_quiet() {
        let mut bell = VisualBell::new();
        assert_eq!(
            bell.ring(ms(10_000)),
            Some(2),
            "the first bell flashes twice"
        );
        assert_eq!(bell.ring(ms(10_100)), None, "inside 500 ms: dropped");
        assert_eq!(
            bell.ring(ms(10_500)),
            Some(1),
            "500 ms after the last *flash*, not after the dropped bell"
        );
        assert_eq!(bell.ring(ms(13_499)), Some(1), "under 3 s: single");
        assert_eq!(bell.ring(ms(16_499)), Some(2), "3 s of quiet: double again");
    }

    /// One flash is in-and-out over two passes, peaking at the end of the first; a double
    /// flash is two whole flashes and ends clear, not at peak (the divergence in the module
    /// doc).
    #[test]
    fn a_flash_eases_in_and_out_per_pass() {
        let single = BellFlash::new(ms(1000), 1);
        assert_eq!(single.alpha(ms(1000)), Some(0.), "starts clear");
        let quarter = single.alpha(ms(1025)).unwrap();
        assert!(
            (quarter - PEAK_ALPHA * 0.25).abs() < 1e-6,
            "ease-in quad at half a pass, got {quarter}"
        );
        assert!(
            (single.alpha(ms(1075)).unwrap() - quarter).abs() < 1e-6,
            "the way out mirrors the way in"
        );
        assert_eq!(single.alpha(ms(1100)), None, "gone after two passes");
        assert_eq!(single.alpha(ms(999)), None, "not before it starts");

        let double = BellFlash::new(ms(0), 2);
        assert!(
            double.alpha(ms(149)).unwrap() > 0.9 * PEAK_ALPHA,
            "the second flash reaches peak"
        );
        let near_end = double.alpha(ms(199)).unwrap();
        assert!(
            near_end < 0.01,
            "the second flash fades out instead of snapping off at peak, got {near_end}"
        );
        assert_eq!(double.alpha(ms(200)), None);
    }
}
