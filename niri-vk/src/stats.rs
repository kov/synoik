//! Process-wide counters for the two things a frame can do too many of: GPU
//! round trips and draw calls.
//!
//! A "submit" here means `vkQueueSubmit` followed by a fence wait — the only
//! shape this renderer uses. Each one is a full CPU↔GPU round trip, and on a
//! virtualized stack (Venus over virtio-gpu) that round trip costs milliseconds
//! regardless of how much work it carries, so *how many* a frame does is a more
//! useful number than how much it drew.
//!
//! These live in `niri-vk` rather than in the compositor's `frame_log` because
//! the submit path itself does — `Gpu::run_commands` is the one-shot submit every
//! upload, layout transition and blur chain goes through, and it cannot reach
//! back into the `niri` crate. The frame log reads these across a frame.
//!
//! Counting is unconditional and lock-free (two relaxed atomic adds); the timing
//! is gated on [`set_enabled`] so an unlogged session does not pay an
//! `Instant::now()` per submit.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

static SUBMITS: AtomicU64 = AtomicU64::new(0);
static SUBMIT_NANOS: AtomicU64 = AtomicU64::new(0);
static DRAWS: AtomicU64 = AtomicU64::new(0);
static SHAPES: AtomicU64 = AtomicU64::new(0);
static SHAPE_NANOS: AtomicU64 = AtomicU64::new(0);
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Whether to time submits as well as count them. Set once, by the frame log.
pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

/// Times a submit + fence wait, counting it immediately and banking its duration
/// on drop. Hold across `vkQueueSubmit` *and* the wait: the wait is where the
/// round trip actually costs.
pub struct SubmitTimer(Option<Instant>);

impl Drop for SubmitTimer {
    fn drop(&mut self) {
        if let Some(started) = self.0 {
            let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
            SUBMIT_NANOS.fetch_add(nanos, Ordering::Relaxed);
        }
    }
}

/// Record one GPU round trip. Counted even when timing is off, so the count is
/// always meaningful; only the clock reads are gated.
pub fn submit() -> SubmitTimer {
    SUBMITS.fetch_add(1, Ordering::Relaxed);
    SubmitTimer(ENABLED.load(Ordering::Relaxed).then(Instant::now))
}

/// Times one text shaping run — layout *or* measurement. Both matter: a measure
/// is a full cosmic-text shape with nothing to show for it, and layout code calls
/// it far more freely than it would call a draw.
pub struct ShapeTimer(Option<Instant>);

impl Drop for ShapeTimer {
    fn drop(&mut self) {
        if let Some(started) = self.0 {
            let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
            SHAPE_NANOS.fetch_add(nanos, Ordering::Relaxed);
        }
    }
}

/// Record one shaping run. Hold the guard for the shape.
pub fn shape() -> ShapeTimer {
    SHAPES.fetch_add(1, Ordering::Relaxed);
    ShapeTimer(ENABLED.load(Ordering::Relaxed).then(Instant::now))
}

/// Shaping runs since process start. The caller takes a delta across a frame.
pub fn shapes() -> u64 {
    SHAPES.load(Ordering::Relaxed)
}

/// Time spent shaping since the last call, clearing the counter.
pub fn take_shape_time() -> Duration {
    Duration::from_nanos(SHAPE_NANOS.swap(0, Ordering::Relaxed))
}

/// Record one `vkCmdDraw`.
pub fn draw() {
    DRAWS.fetch_add(1, Ordering::Relaxed);
}

/// Submits since process start. The caller takes a delta across a frame.
pub fn submits() -> u64 {
    SUBMITS.load(Ordering::Relaxed)
}

/// Draws since process start. The caller takes a delta across a frame.
pub fn draws() -> u64 {
    DRAWS.load(Ordering::Relaxed)
}

/// Time spent in submits since the last call, clearing the counter.
pub fn take_submit_time() -> Duration {
    Duration::from_nanos(SUBMIT_NANOS.swap(0, Ordering::Relaxed))
}
