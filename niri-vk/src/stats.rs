//! Per-thread counters for the two things a frame can do too many of: GPU round
//! trips and draw calls.
//!
//! A "submit" here is `vkQueueSubmit`; a "retire" is the fence wait that follows
//! it. Together they are one full CPU↔GPU round trip. On a virtualized stack
//! (Venus over virtio-gpu) the wait tracks GPU work closely, but a submit issued
//! after the ring has been idle for a millisecond pays ~1 ms to wake the host ring
//! thread — flat, whatever it carries (`docs/fork/venus-cost.md` §9.4). A blocking
//! wait puts the ring back to sleep, so *how many* round trips a frame makes is a
//! more useful number than how much it drew.
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
    /// Host memory into one texture, on its own submit.
    Upload,
    /// New glyph coverage into the persistent atlas.
    UploadGlyphs,
    /// An shm client's buffer into its cached image, through the renderer's shared staging.
    UploadShm,
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
    pub const ALL: [SubmitSite; 10] = [
        SubmitSite::KmsFrame,
        SubmitSite::DmabufFrame,
        SubmitSite::OffscreenFrame,
        SubmitSite::CaptureFlush,
        SubmitSite::Upload,
        SubmitSite::UploadGlyphs,
        SubmitSite::UploadShm,
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
            SubmitSite::UploadGlyphs => "glyphs",
            SubmitSite::UploadShm => "shm",
            SubmitSite::Transition => "transition",
            SubmitSite::Blur => "blur",
            SubmitSite::Readback => "readback",
        }
    }

    /// Position in [`ALL`](Self::ALL), for the per-site arrays. `pub` because the
    /// frame log indexes its own GPU-time array with the same taxonomy — one
    /// vocabulary for where a submit came from, whether we are reporting the CPU's
    /// wait on it or the GPU time it actually cost.
    pub const fn index(self) -> usize {
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
    /// Waits so far this frame — only whether it is zero matters. See [`begin_frame`].
    static WAITS: Cell<u64> = const { Cell::new(0) };
    static FIRST_WAIT: Cell<Duration> = const { Cell::new(Duration::ZERO) };
    static FIRST_SITE: Cell<Option<SubmitSite>> = const { Cell::new(None) };
    static UPLOADED_BYTES: Cell<u64> = const { Cell::new(0) };
    /// Wall time inside GPU resource creation this frame, and how many creations. See [`creating`].
    static CREATE_NANOS: Cell<u64> = const { Cell::new(0) };
    static CREATES: Cell<u64> = const { Cell::new(0) };
    /// Wall time spent memcpying into mapped host-visible staging. See [`staging_write`].
    static STAGE_NANOS: Cell<u64> = const { Cell::new(0) };
    /// How many attributed scopes are open, and when the outermost one opened. See
    /// [`enter_attributed`].
    static ATTRIB_DEPTH: Cell<u32> = const { Cell::new(0) };
    static ATTRIB_START: Cell<Option<Instant>> = const { Cell::new(None) };
    /// Cumulative *union* of every attributed scope on this thread. Cumulative like [`draws`], so
    /// a caller takes deltas; unlike the per-bucket counters it is never cleared, because it is
    /// read at phase boundaries rather than once a frame.
    static ATTRIB_NANOS: Cell<u64> = const { Cell::new(0) };
}

/// Open a scope whose time some clause of the frame line already accounts for.
///
/// Every timer in this module and [`crate::stats`]'s caller-side twin (`frame_log::time_bake`)
/// opens one, so that "how much of this phase did we fail to explain?" has an answer. The naive
/// answer — the phase total minus the sum of the buckets — is **wrong**, because the buckets
/// nest: a widget bake allocates its texture (a creation) and shapes its label (a shaping run)
/// *inside* the bake it is already being timed as, so summing them counts that time two or three
/// times and can exceed the phase itself.
///
/// So this tracks the union instead: only the outermost scope's wall time is banked, and nesting
/// is free. Depth-counted rather than interval-merged because the guards are RAII and therefore
/// properly nested by construction — there is no partial overlap to merge.
///
/// Must be paired with [`leave_attributed`]. Every caller pairs them by tying both to the guard's
/// `Option<Instant>`, so a guard built while timing was off never enters and never leaves.
pub fn enter_attributed() {
    let depth = ATTRIB_DEPTH.with(Cell::get);
    if depth == 0 {
        ATTRIB_START.with(|c| c.set(Some(Instant::now())));
    }
    ATTRIB_DEPTH.with(|c| c.set(depth.saturating_add(1)));
}

/// Close a scope opened by [`enter_attributed`], banking the outermost one's wall time.
pub fn leave_attributed() {
    let depth = ATTRIB_DEPTH.with(Cell::get).saturating_sub(1);
    ATTRIB_DEPTH.with(|c| c.set(depth));
    if depth > 0 {
        return;
    }
    if let Some(started) = ATTRIB_START.with(|c| c.replace(None)) {
        let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        ATTRIB_NANOS.with(|c| c.set(c.get().saturating_add(nanos)));
    }
}

/// Union of every attributed scope this thread has run, cumulative. Callers take a delta across
/// the span they care about — see `FrameLog::phase`.
pub fn attributed() -> Duration {
    Duration::from_nanos(ATTRIB_NANOS.with(Cell::get))
}

fn bank(site: SubmitSite, nanos: u64, retire: bool) {
    let mut sites = SITES.with(Cell::get);
    let slot = &mut sites[site.index()];
    let d = Duration::from_nanos(nanos);
    if retire {
        slot.retiring = slot.retiring.saturating_add(d);
        // The frame's first wait, kept apart — see `begin_frame` for why it is not comparable to
        // the others. Recorded on the wait rather than the submit because a deferred submit never
        // waits at all, and it is the first *wait* that inherits the previous frame's tail.
        let waits = WAITS.with(Cell::get);
        WAITS.set(waits.saturating_add(1));
        if waits == 0 {
            FIRST_WAIT.set(d);
            FIRST_SITE.set(Some(site));
        }
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
            leave_attributed();
        }
    }
}

fn timer(site: SubmitSite, total: &'static LocalKey<Cell<u64>>, retire: bool) -> SubmitTimer {
    let started = ENABLED.load(Ordering::Relaxed).then(Instant::now);
    if started.is_some() {
        enter_attributed();
    }
    SubmitTimer {
        started,
        site,
        total,
        retire,
    }
}

/// Begin a frame's accounting: the next wait is that frame's first.
///
/// The first wait is worth knowing apart from every other. Every submit is chained on the queue
/// timeline, so the frame's *first* one cannot start until the previous frame's work — including
/// the scanout submit the CPU walked away from — has finished on the GPU. Whichever site happens
/// to go first therefore absorbs the tail of the last frame, and its per-site figure reads as
/// though that site were expensive. Without this split, "1 upload in 20.45 ms" and "the previous
/// frame had 20 ms left to run" are the same number.
pub fn begin_frame() {
    FIRST_WAIT.set(Duration::ZERO);
    FIRST_SITE.set(None);
    WAITS.set(0);
}

/// The first wait of the frame and where it was paid, or `None` if the frame waited for nothing.
/// Clears, like [`take_sites`].
pub fn take_first_wait() -> Option<(SubmitSite, Duration)> {
    let site = FIRST_SITE.replace(None)?;
    Some((site, FIRST_WAIT.replace(Duration::ZERO)))
}

/// Bytes staged into GPU images since the last call, clearing the counter. Distinguishes a frame
/// that made many small round trips from one that moved a wallpaper: the first wants fewer
/// submits, the second wants to not be on the frame at all.
pub fn take_uploaded_bytes() -> u64 {
    UPLOADED_BYTES.with(|c| c.replace(0))
}

/// Record `bytes` of host memory staged for a GPU image. Call once per upload, whatever shape it
/// takes; unlike the timers this is never gated, since it costs an add.
pub fn uploaded(bytes: u64) {
    add(&UPLOADED_BYTES, bytes);
}

/// Times the creation of a GPU resource — an image plus its memory, a descriptor set, a
/// framebuffer, a pipeline.
///
/// Worth its own bucket because on a virtualized driver these are **not** free and **not**
/// submits. `vkAllocateMemory` and `vkCreateImageView` are asynchronous and cost microseconds, but
/// `vkCreateImage` round-trips whenever venus misses its image-requirements cache — keyed on the
/// whole `VkImageCreateInfo`, `extent` included — and a miss on the dmabuf/DRM-modifier shape runs
/// 0.06–0.7 ms (`docs/fork/venus-cost.md` §9.1). So a frame that allocates can spend milliseconds
/// somewhere the submit accounting cannot see. That is exactly the shape of the unattributed CPU on
/// the seat's worst frames — collect time that is neither a fence wait nor a bake — and this is the
/// counter that says whether it is this or something else.
///
/// **Call this at the constructor that actually creates, never at a caller.** Two rules follow
/// from that, and both were violated before they were written down:
///
/// - *Never above a cache lookup.* `import_dmabuf_as_texture` timed itself from the top, so every
///   cache **hit** — the whole point of that cache — reported a creation that never happened.
/// - *Never nested.* `create_buffer` wrapped its own call to `Texture::new_color_target`, so one
///   offscreen counted as two resources.
///
/// The rule makes the number mean one thing: GPU images and allocations that were really made.
/// A site that creates nothing must not appear here at all, or the counter cannot answer the
/// question it exists for — is this frame allocating, or reusing?
///
/// Counted even when timing is off, so the count stays meaningful on its own; the clock reads are
/// gated like every other timer here.
pub fn creating() -> CreateTimer {
    add(&CREATES, 1);
    let started = ENABLED.load(Ordering::Relaxed).then(Instant::now);
    if started.is_some() {
        enter_attributed();
    }
    CreateTimer(started)
}

pub struct CreateTimer(Option<Instant>);

impl Drop for CreateTimer {
    fn drop(&mut self) {
        if let Some(started) = self.0 {
            let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
            add(&CREATE_NANOS, nanos);
            leave_attributed();
        }
    }
}

/// Times a host write into mapped `HOST_VISIBLE` staging memory.
///
/// Its own bucket rather than part of [`creating`] because it is a different cost with a different
/// fix: not a round trip to the host but a straight memcpy into a mapping whose pages have never
/// been touched, which on this VM runs at ~7 GB/s against ~58 GB/s once the same buffer is warm.
/// The mapping is cached, not write-combined (`docs/fork/venus-cost.md` §9.2). It scales
/// with payload, so it is the number that describes a wallpaper (48 MiB ≈ 8 ms) and rounds to
/// nothing for an icon. Read together with the uploaded-bytes counter.
pub fn staging_write() -> StagingTimer {
    let started = ENABLED.load(Ordering::Relaxed).then(Instant::now);
    if started.is_some() {
        enter_attributed();
    }
    StagingTimer(started)
}

pub struct StagingTimer(Option<Instant>);

impl Drop for StagingTimer {
    fn drop(&mut self) {
        if let Some(started) = self.0 {
            let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
            add(&STAGE_NANOS, nanos);
            leave_attributed();
        }
    }
}

/// Time spent writing host bytes into staging since the last call. Clears.
pub fn take_staging_write() -> Duration {
    Duration::from_nanos(STAGE_NANOS.with(|c| c.replace(0)))
}

/// GPU resource creations since the last call and the wall time they took, clearing both.
pub fn take_creates() -> (u64, Duration) {
    (
        CREATES.with(|c| c.replace(0)),
        Duration::from_nanos(CREATE_NANOS.with(|c| c.replace(0))),
    )
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
            leave_attributed();
        }
    }
}

/// Record one shaping run. Hold the guard for the shape.
pub fn shape() -> ShapeTimer {
    add(&SHAPES, 1);
    let started = ENABLED.load(Ordering::Relaxed).then(Instant::now);
    if started.is_some() {
        enter_attributed();
    }
    ShapeTimer(started)
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

    /// The whole reason the residual is a union and not a sum: the buckets nest. A widget bake
    /// allocates its texture and shapes its label *inside* itself, so adding `baking + creates +
    /// shaping` counts that stretch two or three times — a sum that can exceed the phase it is
    /// supposed to explain, and would report a negative or zero residual precisely on the frames
    /// that have one.
    ///
    /// Pinned on the invariant rather than on wall clock: only the outermost scope banks, so an
    /// inner close must move nothing at all. That is exact, and a sleep-based version would be
    /// both slower and flakier.
    /// Spin until the monotonic clock ticks.
    ///
    /// A scope containing nothing can span **zero** nanoseconds — two back-to-back `Instant::now()`
    /// reads can return the same value — which made the assertions below flake about one run in
    /// ten. Cheaper and more reliable than sleeping: this returns in nanoseconds.
    fn until_the_clock_moves() {
        let t = Instant::now();
        while t.elapsed().is_zero() {
            std::hint::spin_loop();
        }
    }

    #[test]
    fn nested_attributed_scopes_bank_once_not_once_each() {
        let before = attributed();

        enter_attributed(); // a bake
        enter_attributed(); // its texture allocation, inside the bake
        until_the_clock_moves();
        leave_attributed();
        assert_eq!(
            attributed(),
            before,
            "an inner scope closing banked time while its outer scope was still open"
        );

        leave_attributed();
        assert!(
            attributed() > before,
            "closing the outermost scope banked nothing"
        );
    }

    /// Unbalanced leaves must not run the depth below zero and start banking every scope from a
    /// stale start — the counter would then grow without bound and the residual would go negative.
    #[test]
    fn an_unbalanced_leave_does_not_corrupt_the_depth() {
        leave_attributed();
        leave_attributed();

        let before = attributed();
        enter_attributed();
        until_the_clock_moves();
        leave_attributed();
        assert!(attributed() > before, "the counter stopped working");
    }

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

    /// The frame's **first** wait is kept apart, and it is the first — not the longest.
    ///
    /// Every submit is chained on the queue timeline, so a frame's first wait also drains whatever
    /// the previous frame left running. Whichever site happens to go first therefore reads as
    /// expensive. Reporting the longest instead would defeat the whole purpose: the point is to
    /// find out whether the big number is the site's own cost or the previous frame's tail.
    #[test]
    fn the_frames_first_wait_is_reported_apart_from_the_rest() {
        set_enabled(true);
        begin_frame();

        // A short wait first, a long one after. The short one is the answer.
        drop(retire(SubmitSite::UploadShm));
        {
            let _slow = retire(SubmitSite::Blur);
            std::thread::sleep(Duration::from_millis(2));
        }

        let (site, _wait) = take_first_wait().expect("the frame waited, so there is a first wait");
        assert_eq!(
            site,
            SubmitSite::UploadShm,
            "the first wait was reported as the longest one instead of the first"
        );
        assert!(
            take_first_wait().is_none(),
            "taking the first wait did not clear it, so the next frame inherits this one"
        );

        // A frame that never waits has no first wait to report.
        begin_frame();
        drop(submit(SubmitSite::KmsFrame));
        assert!(
            take_first_wait().is_none(),
            "a frame that only submitted, and never waited, reported a wait"
        );
        set_enabled(false);
    }
}
