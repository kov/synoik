//! Per-thread counters for the two things a frame can do too many of: GPU round
//! trips and draw calls.
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
//! **Per thread, not per process.** Every counter here is fed by a thread holding
//! `&mut` to a renderer and read by that same thread — the compositor's, in a
//! session; the test's own, under libtest. As process-wide atomics they were a
//! flake generator: a test asserting "this repaint cost no submit" could count a
//! *parallel* test's submit and fail, rarely enough to look like whatever change
//! was in front of you. Thread-local, each reader sees exactly the work it caused.
//! Nothing needs a cross-thread total, and a thread that never renders contributes
//! nothing to read.
//!
//! Counting is unconditional and lock-free (a `Cell` bump); the timing is gated on
//! [`set_enabled`] so an unlogged session does not pay an `Instant::now()` per
//! submit.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::LocalKey;
use std::time::{Duration, Instant};

thread_local! {
    static SUBMITS: Cell<u64> = const { Cell::new(0) };
    static SUBMIT_NANOS: Cell<u64> = const { Cell::new(0) };
    static RETIRE_NANOS: Cell<u64> = const { Cell::new(0) };
    static DRAWS: Cell<u64> = const { Cell::new(0) };
    static SHADED: Cell<u64> = const { Cell::new(0) };
    static SHAPES: Cell<u64> = const { Cell::new(0) };
    static SHAPE_NANOS: Cell<u64> = const { Cell::new(0) };
}

/// Whether timing is on. Process-wide, unlike the counters: it is configuration,
/// read once from the environment and true for every thread or none.
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Saturating, so a counter cannot wrap a delta into a nonsense number.
fn add(counter: &'static LocalKey<Cell<u64>>, n: u64) {
    counter.with(|c| c.set(c.get().saturating_add(n)));
}

fn take(counter: &'static LocalKey<Cell<u64>>) -> Duration {
    Duration::from_nanos(counter.with(|c| c.replace(0)))
}

/// Whether to time submits as well as count them. Set once, by the frame log.
pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

/// Where a submit came from.
///
/// The renderer has exactly two places that call `vkQueueSubmit` — a frame's `finish` and
/// `Gpu::run_commands` — so without a label every round trip a frame makes outside the one going
/// to KMS is indistinguishable from every other. On the live seat a frame issues 7–27 of them and
/// they are now what puts it over budget, and "which ones" is not answerable from the counters.
///
/// The split that matters is **not** what the submit renders into: a screencast render and a
/// mid-frame capture flush both target a non-offscreen buffer, and neither costs what the scanout
/// submit costs. Naming the *caller* is what makes the frame line say something.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SubmitSite {
    /// The frame a display controller scans out. The expensive one, and the only one whose fence
    /// has somewhere to go.
    KmsFrame,
    /// A frame rendered into a dmabuf that is not a scanout target: a screencast or screencopy
    /// buffer, whose consumer expects it finished.
    DmabufFrame,
    /// A frame rendered into an offscreen texture: a widget bake, a window preview, a snapshot,
    /// an xray or effect buffer.
    OffscreenFrame,
    /// A frame cut in half mid-record, so a captured region is complete before the separately
    /// submitted blur that samples it.
    CaptureFlush,
    /// Host memory into a texture: an icon, an shm client buffer, glyph coverage.
    Upload,
    /// A layout transition or queue-family acquire on a command buffer of its own.
    Transition,
    /// A dual-Kawase blur chain.
    Blur,
    /// Pixels back to the CPU. Always synchronous by definition.
    Readback,
}

impl SubmitSite {
    /// Every site, in the order they are reported. Keep this exhaustive — a site missing here is
    /// simply invisible, which is the problem this type exists to fix.
    pub const ALL: [SubmitSite; 8] = [
        SubmitSite::KmsFrame,
        SubmitSite::DmabufFrame,
        SubmitSite::OffscreenFrame,
        SubmitSite::CaptureFlush,
        SubmitSite::Upload,
        SubmitSite::Transition,
        SubmitSite::Blur,
        SubmitSite::Readback,
    ];

    /// How it appears in the frame line. Short: several of these share one line.
    pub const fn label(self) -> &'static str {
        match self {
            SubmitSite::KmsFrame => "scanout",
            SubmitSite::DmabufFrame => "dmabuf",
            SubmitSite::OffscreenFrame => "offscreen",
            SubmitSite::CaptureFlush => "capture",
            SubmitSite::Upload => "upload",
            SubmitSite::Transition => "transition",
            SubmitSite::Blur => "blur",
            SubmitSite::Readback => "readback",
        }
    }

    const fn index(self) -> usize {
        self as usize
    }
}

/// One site's share of a frame. Times are gated on [`set_enabled`]; the count never is.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct SiteTotals {
    pub submits: u64,
    /// `vkQueueSubmit` alone.
    pub submitting: Duration,
    /// Waiting for it. Carries no count of its own — see [`retire`].
    pub retiring: Duration,
}

impl SiteTotals {
    const ZERO: SiteTotals = SiteTotals {
        submits: 0,
        submitting: Duration::ZERO,
        retiring: Duration::ZERO,
    };
}

thread_local! {
    /// Per-site totals since the last [`take_sites`]. Take-and-clear rather than cumulative
    /// (unlike [`submits`]) because a caller wants a frame's breakdown, never a running one.
    static SITES: Cell<[SiteTotals; SubmitSite::ALL.len()]> =
        const { Cell::new([SiteTotals::ZERO; SubmitSite::ALL.len()]) };
}

fn bank(site: SubmitSite, nanos: u64, retire: bool) {
    let mut sites = SITES.with(Cell::get);
    let slot = &mut sites[site.index()];
    let d = Duration::from_nanos(nanos);
    if retire {
        slot.retiring = slot.retiring.saturating_add(d);
    } else {
        slot.submitting = slot.submitting.saturating_add(d);
    }
    SITES.set(sites);
}

/// Banks its lifetime into the running total and into its site's. Inert when timing
/// is off, so call sites can be unconditional.
pub struct SubmitTimer {
    started: Option<Instant>,
    site: SubmitSite,
    total: &'static LocalKey<Cell<u64>>,
    retire: bool,
}

impl Drop for SubmitTimer {
    fn drop(&mut self) {
        if let Some(started) = self.started {
            let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
            add(self.total, nanos);
            bank(self.site, nanos, self.retire);
        }
    }
}

fn timer(site: SubmitSite, total: &'static LocalKey<Cell<u64>>, retire: bool) -> SubmitTimer {
    SubmitTimer {
        started: ENABLED.load(Ordering::Relaxed).then(Instant::now),
        site,
        total,
        retire,
    }
}

/// Record one GPU round trip. Hold the guard across `vkQueueSubmit` only — the
/// wait belongs to [`retire`]. Counted even when timing is off, so the count is
/// always meaningful; only the clock reads are gated.
pub fn submit(site: SubmitSite) -> SubmitTimer {
    add(&SUBMITS, 1);
    let mut sites = SITES.with(Cell::get);
    let slot = &mut sites[site.index()];
    slot.submits = slot.submits.saturating_add(1);
    SITES.set(sites);
    timer(site, &SUBMIT_NANOS, false)
}

/// Time a wait for already-submitted work to complete. Hold the guard across the
/// fence wait.
///
/// Uncounted on purpose: a retire is not a round trip of its own, and the frame
/// that pays for one need not be the frame that issued the submit.
pub fn retire(site: SubmitSite) -> SubmitTimer {
    timer(site, &RETIRE_NANOS, true)
}

/// Every site's share since the last call, clearing them. Indexed by
/// [`SubmitSite::ALL`]'s order.
pub fn take_sites() -> [SiteTotals; SubmitSite::ALL.len()] {
    SITES.replace([SiteTotals::ZERO; SubmitSite::ALL.len()])
}

/// Times one text shaping run — layout *or* measurement. Both matter: a measure
/// is a full cosmic-text shape with nothing to show for it, and layout code calls
/// it far more freely than it would call a draw.
pub struct ShapeTimer(Option<Instant>);

impl Drop for ShapeTimer {
    fn drop(&mut self) {
        if let Some(started) = self.0 {
            let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
            add(&SHAPE_NANOS, nanos);
        }
    }
}

/// Record one shaping run. Hold the guard for the shape.
pub fn shape() -> ShapeTimer {
    add(&SHAPES, 1);
    ShapeTimer(ENABLED.load(Ordering::Relaxed).then(Instant::now))
}

/// Shaping runs on this thread. The caller takes a delta across a frame.
pub fn shapes() -> u64 {
    SHAPES.with(Cell::get)
}

/// Time spent shaping since the last call, clearing the counter.
pub fn take_shape_time() -> Duration {
    take(&SHAPE_NANOS)
}

/// Record one `vkCmdDraw` covering `pixels` shaded fragments (the drawn quad clipped to its
/// scissor).
///
/// The pixel count is the number that matters. Measured on this stack, holding the draw count
/// fixed and shrinking the damage rect collapses a frame's cost to the bare submit overhead — so
/// what a frame costs is how many fragments it shades, not how many draws it issues. Reported as
/// an overdraw multiple of the output area, because that is the form you can act on.
pub fn draw(pixels: u64) {
    add(&DRAWS, 1);
    add(&SHADED, pixels);
}

/// Fragments shaded on this thread. The caller takes a delta across a frame.
pub fn shaded() -> u64 {
    SHADED.with(Cell::get)
}

/// Submits on this thread. The caller takes a delta across a frame.
pub fn submits() -> u64 {
    SUBMITS.with(Cell::get)
}

/// Draws on this thread. The caller takes a delta across a frame.
pub fn draws() -> u64 {
    DRAWS.with(Cell::get)
}

/// Time spent enqueueing submits since the last call, clearing the counter.
pub fn take_submit_time() -> Duration {
    take(&SUBMIT_NANOS)
}

/// Time spent waiting for submitted work to complete since the last call,
/// clearing the counter.
pub fn take_retire_time() -> Duration {
    take(&RETIRE_NANOS)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A counter must not see work another thread did. This is what makes an
    /// assertion like "this repaint cost no submit" trustworthy under libtest,
    /// which runs tests in parallel against their own renderers — as process-wide
    /// atomics these counters made such tests fail on a neighbour's work.
    #[test]
    fn a_counter_does_not_see_another_threads_work() {
        let before = (submits(), shapes(), draws());

        std::thread::spawn(|| {
            let _submit = submit(SubmitSite::KmsFrame);
            let _shape = shape();
            draw(1234);
        })
        .join()
        .unwrap();

        assert_eq!(
            (submits(), shapes(), draws()),
            before,
            "another thread's work landed in this thread's counters"
        );

        let _submit = submit(SubmitSite::KmsFrame);
        assert_eq!(
            submits(),
            before.0 + 1,
            "this thread's own submit is counted"
        );
    }
}
