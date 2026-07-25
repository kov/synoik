//! Process-wide counters for the two things a frame can do too many of: GPU
//! round trips and draw calls.
//!
//! A "submit" here is `vkQueueSubmit`; a "retire" is the fence wait that follows
//! it. Together they are one full CPU↔GPU round trip, and on a virtualized stack
//! (Venus over virtio-gpu) that round trip costs milliseconds regardless of how
//! much work it carries, so *how many* a frame does is a more useful number than
//! how much it drew.
//!
//! The two are timed **separately** even though today every submit in this
//! renderer is immediately retired by its own caller. The cost lives almost
//! entirely in the retire — a scanout submit enqueues in microseconds and then
//! parks for 12–14 ms — so a change that stops blocking on the fence and hands it
//! to KMS instead (`docs/fork/renderer-synchronous-submits.md`) would collapse the
//! submit time to nothing and read as a saving, when the wait had only moved. Two
//! numbers make a removed wait tell itself apart from a moved one, and a retire
//! deliberately carries no count: once waits are deferred, the retire that a frame
//! pays for need not be the submit that frame issued.
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
static RETIRE_NANOS: AtomicU64 = AtomicU64::new(0);
static SCANOUT_SUBMITS: AtomicU64 = AtomicU64::new(0);
static SCANOUT_NANOS: AtomicU64 = AtomicU64::new(0);
static SCANOUT_RETIRE_NANOS: AtomicU64 = AtomicU64::new(0);
static DRAWS: AtomicU64 = AtomicU64::new(0);
static SHADED: AtomicU64 = AtomicU64::new(0);
static SHAPES: AtomicU64 = AtomicU64::new(0);
static SHAPE_NANOS: AtomicU64 = AtomicU64::new(0);
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Whether to time submits as well as count them. Set once, by the frame log.
pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

/// What a submit renders into. Worth telling apart: on this stack a submit into an
/// offscreen costs a fraction of a millisecond, while one into the KMS scanout buffer
/// costs most of a refresh interval — which is a different problem with a different fix.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SubmitKind {
    /// An offscreen texture, an upload, a transition, a blur chain, a readback.
    Offscreen,
    /// The dmabuf a display controller scans out.
    Scanout,
}

/// Banks its lifetime into a total and, for a scanout, into that kind's total as
/// well. Inert when timing is off, so call sites can be unconditional.
pub struct SubmitTimer {
    started: Option<Instant>,
    total: &'static AtomicU64,
    scanout: Option<&'static AtomicU64>,
}

impl Drop for SubmitTimer {
    fn drop(&mut self) {
        if let Some(started) = self.started {
            let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
            self.total.fetch_add(nanos, Ordering::Relaxed);
            if let Some(scanout) = self.scanout {
                scanout.fetch_add(nanos, Ordering::Relaxed);
            }
        }
    }
}

fn timer(kind: SubmitKind, total: &'static AtomicU64, scanout: &'static AtomicU64) -> SubmitTimer {
    SubmitTimer {
        started: ENABLED.load(Ordering::Relaxed).then(Instant::now),
        total,
        scanout: (kind == SubmitKind::Scanout).then_some(scanout),
    }
}

/// Record one GPU round trip. Hold the guard across `vkQueueSubmit` only — the
/// wait belongs to [`retire`]. Counted even when timing is off, so the count is
/// always meaningful; only the clock reads are gated.
pub fn submit(kind: SubmitKind) -> SubmitTimer {
    SUBMITS.fetch_add(1, Ordering::Relaxed);
    if kind == SubmitKind::Scanout {
        SCANOUT_SUBMITS.fetch_add(1, Ordering::Relaxed);
    }
    timer(kind, &SUBMIT_NANOS, &SCANOUT_NANOS)
}

/// Time a wait for already-submitted work to complete. Hold the guard across the
/// fence wait.
///
/// Uncounted on purpose: a retire is not a round trip of its own, and the frame
/// that pays for one need not be the frame that issued the submit.
pub fn retire(kind: SubmitKind) -> SubmitTimer {
    timer(kind, &RETIRE_NANOS, &SCANOUT_RETIRE_NANOS)
}

/// Scanout submits since process start. The caller takes a delta across a frame.
pub fn scanout_submits() -> u64 {
    SCANOUT_SUBMITS.load(Ordering::Relaxed)
}

/// Time spent enqueueing scanout submits since the last call, clearing the counter.
pub fn take_scanout_submit_time() -> Duration {
    Duration::from_nanos(SCANOUT_NANOS.swap(0, Ordering::Relaxed))
}

/// Time spent waiting for scanout work to complete since the last call, clearing
/// the counter.
pub fn take_scanout_retire_time() -> Duration {
    Duration::from_nanos(SCANOUT_RETIRE_NANOS.swap(0, Ordering::Relaxed))
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

/// Record one `vkCmdDraw` covering `pixels` shaded fragments (the drawn quad clipped to its
/// scissor).
///
/// The pixel count is the number that matters. Measured on this stack, holding the draw count
/// fixed and shrinking the damage rect collapses a frame's cost to the bare submit overhead — so
/// what a frame costs is how many fragments it shades, not how many draws it issues. Reported as
/// an overdraw multiple of the output area, because that is the form you can act on.
pub fn draw(pixels: u64) {
    DRAWS.fetch_add(1, Ordering::Relaxed);
    SHADED.fetch_add(pixels, Ordering::Relaxed);
}

/// Fragments shaded since process start. The caller takes a delta across a frame.
pub fn shaded() -> u64 {
    SHADED.load(Ordering::Relaxed)
}

/// Submits since process start. The caller takes a delta across a frame.
pub fn submits() -> u64 {
    SUBMITS.load(Ordering::Relaxed)
}

/// Draws since process start. The caller takes a delta across a frame.
pub fn draws() -> u64 {
    DRAWS.load(Ordering::Relaxed)
}

/// Time spent enqueueing submits since the last call, clearing the counter.
pub fn take_submit_time() -> Duration {
    Duration::from_nanos(SUBMIT_NANOS.swap(0, Ordering::Relaxed))
}

/// Time spent waiting for submitted work to complete since the last call,
/// clearing the counter.
pub fn take_retire_time() -> Duration {
    Duration::from_nanos(RETIRE_NANOS.swap(0, Ordering::Relaxed))
}
