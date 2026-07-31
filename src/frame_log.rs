//! Frame-timing instrumentation: which frames took too long, and what was going
//! on when they did.
//!
//! The compositor already carries Tracy spans, but those need a build flag *and*
//! a profiler attached to a live session — no use for "leave this running and
//! tell me what stuttered". This is the always-available complement: opt in with
//! an environment variable, get journal lines for the frames that missed their
//! budget, and a periodic summary the rest of the time.
//!
//! # Enabling
//!
//! `NIRI_FRAME_LOG` takes a comma-separated list:
//!
//! | value | meaning |
//! |---|---|
//! | unset, `0`, `off` | disabled (the default) |
//! | `1`, `on` | log frames over the output's refresh interval |
//! | `<number>` | log frames over that many milliseconds |
//! | `all` | log every frame (very noisy — for short captures) |
//! | `summary=<secs>` | how often to emit the rolling summary; `0` turns it off |
//! | `gpu` | also time the GPU passes (see [`gpu_timing`]) |
//!
//! So `NIRI_FRAME_LOG=1` for everyday use, `NIRI_FRAME_LOG=8,summary=5,gpu` to
//! chase something specific, `NIRI_FRAME_LOG=all` to capture a few seconds in
//! full.
//!
//! # What it measures
//!
//! Two independent things, because they fail independently:
//!
//! **The `gpu` option works on the dev VM as of 2026-07-26.** It used to produce
//! nothing, for two independent reasons that looked identical from the log. The
//! virtio-gpu/Venus stack advertised timestamp queries and resolved every one to
//! zero — fixed host-side (`docs/fork/venus-timestamp-gap.md`, §13 of
//! `docs/fork/venus-cost.md`), and this VMM now measures 100% usable pairs. And
//! the flag was set too late for the renderer to see it (see [`gpu_timing`]), so
//! the query pool was never allocated in the first place.
//!
//! A pair that comes back unusable is still counted (`N lost`) instead of quietly
//! averaged in as zero, because the failure mode was a *rate*, not all-or-nothing:
//! a GPU time with samples missing is a floor, and the line says so. A nonzero
//! `lost` on a stack that is supposed to be fixed is itself the finding.
//!
//! - **Frame cost**, phase by phase ([`Phase`]), measured on the compositor thread. Note the render
//!   phase *includes* GPU execution: the Vulkan renderer submits and fence-waits synchronously, so
//!   `finish` does not return until the GPU is done. A slow `submit` is therefore ambiguous between
//!   CPU and GPU until the `gpu` option splits it out. The submit counters do say which side of the
//!   round trip it went to: `N submits in X, waiting Y` separates enqueueing the work from parking
//!   on its fence, and today essentially all of it is the parking.
//! - **Missed deadlines**, from comparing when a frame actually reached the screen against the
//!   presentation time it was built for. That is what a user perceives as a stutter, and it can
//!   happen with every frame cost looking healthy. Deliberately *not* the gap in the DRM vblank
//!   sequence — see [`FrameLog::presented`] for why that measures idleness on a damage-driven
//!   compositor. A miss comes with the frame's *headroom* — how much of the cycle was left when we
//!   handed it to KMS ([`FrameLog::queued`]) — because "late with 8ms of slack" and "late because
//!   we handed it over late" are different bugs and the lateness alone cannot tell them apart.
//!
//! Neither is much use without knowing what the frame was *doing*, so a logged
//! frame carries its [`FrameContext`]: element count, whether the damage was
//! forced full, how many widget bakes ran (an uncached bake is a full GPU
//! round-trip), and the overview/animation state.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

thread_local! {
    /// Number of widget bakes this thread has run. A bake is an uncached
    /// rasterization into its own GPU texture — a render pass, a submit and a
    /// fence wait each — so a frame doing several is a prime stutter suspect.
    ///
    /// A free-standing counter rather than a field because bakes happen deep
    /// inside widget code that has no reason to know about frame logging; the
    /// frame log samples the delta across a frame.
    ///
    /// Per-thread rather than process-wide, because a bake runs on whichever
    /// thread owns the `VulkanRenderer` that pays for it
    /// (`ui::widget::bake_uncached_sized` takes `&mut VulkanRenderer`), and each
    /// reader wants only its own thread's: the frame log samples the compositor
    /// thread, and a test samples its own. As a process-wide count it was a
    /// flake — libtest runs tests in parallel, so a test asserting "this repaint
    /// re-baked nothing" could read a *different* test's bake and fail. Rare
    /// enough to survive many green runs and then be blamed on whatever change
    /// is in front of you; it surfaced under `NIRI_VK_VALIDATION=1`, which
    /// perturbs timing enough to lose the race about one run in five.
    static BAKES: Cell<u64> = const { Cell::new(0) };

    /// Nanoseconds this thread spent baking during the frame being built.
    ///
    /// This lives *inside* the `collect` phase, which is where the live seat put
    /// 22ms of a 31ms frame with only 18 elements on screen. A phase total says a
    /// frame was slow; this says which part of the widget path it was in. Shaping
    /// — the other half — is counted by [`niri_vk::stats`], because it happens on
    /// both the draw and the measure path and only the renderer crate sees both.
    ///
    /// Per-thread for the same reason as [`BAKES`].
    static BAKE_NANOS: Cell<u64> = const { Cell::new(0) };

    /// Which widget baked, keyed by the `#[track_caller]` location of the call into
    /// [`time_bake`] — i.e. the widget's own line, not the toolkit helper it went
    /// through.
    ///
    /// A count alone says a frame paid for a re-rasterization; it does not say
    /// *whose*, and the answer decides what to fix. Finding that the panel re-baked
    /// on every frame of the overview animation took a throwaway `track_caller`
    /// patch and a bespoke test run, all of which this replaces.
    ///
    /// Recorded even when frame logging is off, like [`BAKES`] and unlike
    /// [`BAKE_NANOS`], because the guardrail tests read it without a log. A hash
    /// insert against a GPU round trip is not a cost worth gating.
    static BAKE_SITES: RefCell<HashMap<SiteKey, SiteTally>> = RefCell::new(HashMap::new());

    /// Timestamp pairs the renderer has read back, tagged with the frame they
    /// belong to: `(sequence, Some(duration) | None for a pair that came back
    /// unusable)`.
    ///
    /// Tagged rather than summed into one counter because a deferred submit is
    /// measured *after* the frame that issued it has already finished on the CPU
    /// — the sample surfaces one or two frames later, when the queue timeline
    /// passes it. Summing would silently move an overview frame's 11ms onto the
    /// idle frame that happened to retire it, which is exactly the attribution
    /// the instrument exists to provide. [`FrameLog::end`] parks a frame's line
    /// until its samples land; see [`FrameLog::parked`].
    ///
    /// Per-thread for the same reason as [`BAKES`], and it fixes the same latent
    /// flake: samples were a process-wide counter that the timing test drained
    /// first to compensate, which only works while no other test renders at the
    /// same moment.
    static GPU_SAMPLES: RefCell<Vec<(u64, niri_vk::stats::SubmitSite, Option<Duration>)>> =
        const { RefCell::new(Vec::new()) };

    /// A frame's GPU time subdivided by where in its command buffer it went.
    /// Separate from [`GPU_SAMPLES`] because these are a *subdivision* of samples
    /// already promised and counted, not promises of their own: they must never
    /// move `count` or `lost`, or a frame would park waiting for a sample that
    /// was only ever a breakdown of another one.
    static GPU_PHASE_SAMPLES: RefCell<Vec<(u64, niri_vk::stats::GpuPhase, Duration)>> =
        const { RefCell::new(Vec::new()) };

    /// How many samples the renderer has promised for the frame being built —
    /// one per submit it stamped. [`FrameLog::end`] waits for exactly this many
    /// before emitting the line, so a frame is never reported with a partial GPU
    /// total that reads like a complete one.
    static GPU_EXPECTED: Cell<u64> = const { Cell::new(0) };
}

/// Samples to keep before dropping the oldest. Only reached when something
/// promised a sample and never delivered it — a submit that failed, or a render
/// outside any logged frame — so the cap is a leak guard, not a working limit.
const MAX_PENDING_SAMPLES: usize = 64;

/// Where a bake came from: `(file, line)` of the widget's call, from
/// `#[track_caller]`. `Location` itself is not hashable, and the pair is what a
/// reader wants anyway.
type SiteKey = (&'static str, u32);

/// What one site did: `(bakes, nanoseconds)`. Nanoseconds stay zero unless frame
/// logging is on — see [`BAKE_SITES`].
type SiteTally = (u64, u64);

/// One widget's bakes over some window of time. See [`take_bake_sites`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BakeSite {
    /// Source file of the widget that baked, workspace-relative (`src/ui/panel.rs`).
    pub file: &'static str,
    pub line: u32,
    /// How many times it baked.
    pub bakes: u64,
    /// How long those bakes took in total. Zero unless frame logging is on — the
    /// count is always recorded, the timing is not.
    pub time: Duration,
}

impl std::fmt::Display for BakeSite {
    /// `ui/panel.rs:1540 ×3 4.20ms` — the leading `src/` is dropped, since every
    /// site has it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let file = self.file.strip_prefix("src/").unwrap_or(self.file);
        write!(f, "{file}:{} ×{}", self.line, self.bakes)?;
        if !self.time.is_zero() {
            write!(f, " {}", ms(self.time))?;
        }
        Ok(())
    }
}

/// Take and clear this thread's per-site bake tallies, most expensive first (ties
/// broken by count, then location, so the order is stable for tests).
///
/// Draining rather than accumulating, so a caller measures a window it defines: the
/// frame log takes them per frame, and a test takes them per rendered frame of an
/// animation. See [`BAKE_SITES`].
pub fn take_bake_sites() -> Vec<BakeSite> {
    let mut sites: Vec<BakeSite> = BAKE_SITES.with(|s| {
        s.borrow_mut()
            .drain()
            .map(|((file, line), (bakes, nanos))| BakeSite {
                file,
                line,
                bakes,
                time: Duration::from_nanos(nanos),
            })
            .collect()
    });
    sites.sort_by(|a, b| {
        b.time
            .cmp(&a.time)
            .then(b.bakes.cmp(&a.bakes))
            .then(a.file.cmp(b.file))
            .then(a.line.cmp(&b.line))
    });
    sites
}

/// Whether GPU timing was requested, sampled once from the environment on the
/// first read. The renderer reads this at construction to decide whether to
/// allocate a timestamp query pool, so it must answer the same way for the whole
/// process — and, crucially, it must answer *correctly* before
/// [`FrameLog::from_env`] has run: the tty backend builds its renderer while
/// bringing up the device, which is earlier. Deriving it from the environment on
/// demand rather than having `from_env` push it is what makes the two orders
/// equivalent. See [`gpu_timing`].
static GPU_TIMING: OnceLock<bool> = OnceLock::new();

/// Numbers the frames the log has begun, so a GPU sample read back later can say
/// which frame it belongs to.
///
/// Process-wide rather than per-thread only so the numbers never collide between
/// a test's renderer and the compositor's; the samples themselves are per-thread.
static FRAME_SEQ: AtomicU64 = AtomicU64::new(0);

/// The frame currently being built, or **0** for "no frame is". Read by the
/// renderer through [`current_frame_seq`] when it stamps a command buffer.
///
/// The zero matters. Rendering also happens *between* frames — screenshots,
/// screencast, a test's own renderer — and tagging those samples with the last
/// frame's number would credit its line with GPU time it never spent, or park a
/// frame waiting for a sample that had already been spent elsewhere. Sequences
/// start at 1, so a sample tagged 0 matches no frame and ages out of
/// [`GPU_SAMPLES`], which is what should happen to a measurement with no line to
/// go on.
static CURRENT_SEQ: AtomicU64 = AtomicU64::new(0);

/// Whether anything is listening. Sampled by the scoped timers so an unlogged
/// session does not pay two `Instant::now()` calls per bake and per shaped run.
static ENABLED: AtomicBool = AtomicBool::new(false);

/// The log writer's dropped-line counter, and how many drops the last summary already reported.
///
/// Logging is non-blocking (`main`), so a burst the writer thread cannot keep up with is *dropped*
/// rather than allowed to stall the compositor. A dropped frame line is a hole in exactly the
/// analyses this log exists for — an inter-flip gap that never happened, a miss that reads as
/// absent — so the holes have to announce themselves. Silence here would be the worst outcome:
/// data that looks complete and is not.
static DROPPED_LINES: Mutex<Option<(tracing_appender::non_blocking::ErrorCounter, u64)>> =
    Mutex::new(None);

/// Tell the frame log where to read dropped-line counts. Called once from `main`, after the
/// subscriber is built; without it the summary simply never mentions drops.
pub fn watch_dropped_lines(counter: tracing_appender::non_blocking::ErrorCounter) {
    *DROPPED_LINES.lock().unwrap() = Some((counter, 0));
}

/// Drops since the last call, or `None` if nothing is watching / nothing was dropped.
fn take_dropped_lines() -> Option<u64> {
    let mut guard = DROPPED_LINES.lock().unwrap();
    let (counter, reported) = guard.as_mut()?;
    dropped_delta(counter.dropped_lines() as u64, reported)
}

/// How many drops are new since `reported`, advancing it. Split out from the counter so the
/// arithmetic is testable: `ErrorCounter` is cumulative and cannot be constructed by hand, and
/// getting this wrong is quiet in both directions — report the total every time and every summary
/// screams about drops that already happened, forget to advance and drops are never mentioned.
fn dropped_delta(total: u64, reported: &mut u64) -> Option<u64> {
    let new = total.saturating_sub(*reported);
    *reported = total;
    (new > 0).then_some(new)
}

/// Accumulates its lifetime into this thread's bake time — and its originating
/// widget's — when dropped. The timing is inert when frame logging is off, so call
/// sites can be unconditional; the site's *count* is recorded either way.
pub struct Timed {
    started: Option<Instant>,
    site: (&'static str, u32),
}

impl Drop for Timed {
    fn drop(&mut self) {
        let nanos = self.started.map_or(0, |started| {
            u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX)
        });
        if nanos > 0 {
            BAKE_NANOS.with(|c| c.set(c.get().saturating_add(nanos)));
        }
        if self.started.is_some() {
            niri_vk::stats::leave_attributed();
        }
        BAKE_SITES.with(|s| {
            let mut s = s.borrow_mut();
            let entry = s.entry(self.site).or_insert((0, 0));
            entry.0 = entry.0.saturating_add(1);
            entry.1 = entry.1.saturating_add(nanos);
        });
    }
}

/// Time a widget bake — an uncached rasterization into its own GPU texture. Hold
/// the returned guard for the operation.
///
/// `#[track_caller]`, and every toolkit helper between a widget and here carries it
/// too, so the recorded site is the widget's own line rather than the last hop
/// inside `ui::widget`. That chain is what makes [`take_bake_sites`] name something
/// actionable; a break anywhere in it silently collapses every widget onto one
/// helper line. [`bake_sites_name_the_widget_not_the_helper`] pins it.
///
/// [`bake_sites_name_the_widget_not_the_helper`]: crate::ui::widget::tests
#[track_caller]
pub fn time_bake() -> Timed {
    BAKES.with(|c| c.set(c.get().saturating_add(1)));
    let caller = std::panic::Location::caller();
    let started = ENABLED.load(Ordering::Relaxed).then(Instant::now);
    if started.is_some() {
        // A bake is the outermost of the nesting: it allocates and shapes inside itself. See
        // `niri_vk::stats::enter_attributed` for why the residual is a union and not a sum.
        niri_vk::stats::enter_attributed();
    }
    Timed {
        started,
        site: (caller.file(), caller.line()),
    }
}

/// Widget bakes **this thread** has run since it started. Exposed so a test can
/// assert that a repaint did **not** re-bake — a bake is a GPU round trip, so "did
/// this stay cached?" is a correctness question about frame cost that pixels
/// cannot answer. A test owns its renderer, so its own thread's count is exactly
/// the bakes it caused; see [`BAKES`].
pub fn bakes() -> u64 {
    BAKES.with(Cell::get)
}

/// Whether the renderer should measure GPU pass durations, i.e. whether
/// `NIRI_FRAME_LOG` carries the `gpu` option. See [`FrameLog::from_env`].
///
/// Reads the environment itself instead of waiting to be told, because the first
/// caller is the renderer's constructor and on the tty backend that runs *before*
/// the frame log exists (the device, and its renderer, come up while the backend
/// is being built). Pushing the flag from `from_env` left the query pool
/// unallocated for the whole session, so `gpu` logged nothing — no samples and no
/// losses, which reads exactly like a device that cannot timestamp.
pub fn gpu_timing() -> bool {
    *GPU_TIMING
        .get_or_init(|| std::env::var("NIRI_FRAME_LOG").is_ok_and(|raw| wants_gpu_timing(&raw)))
}

/// Does this `NIRI_FRAME_LOG` value ask for GPU timing? Split out from
/// [`gpu_timing`] so the token matching is testable: the flag itself is a
/// process-wide [`OnceLock`] read from the real environment, which a test cannot
/// set without racing every other test in the binary.
fn wants_gpu_timing(raw: &str) -> bool {
    raw.split(',')
        .map(str::trim)
        .any(|part| part.eq_ignore_ascii_case("gpu"))
}

/// Which frame the log is building, or 0 outside one. See [`CURRENT_SEQ`].
pub fn current_frame_seq() -> u64 {
    CURRENT_SEQ.load(Ordering::Relaxed)
}

/// Promise one GPU sample for the frame being built, so [`FrameLog::end`] knows
/// how many to wait for. Called by the renderer when it stamps a command buffer.
///
/// A no-op outside a frame: that submit's sample has no line to land on, so
/// promising it would park the *next* frame forever.
pub fn expect_gpu_sample() {
    if current_frame_seq() == 0 {
        return;
    }
    GPU_EXPECTED.with(|c| c.set(c.get().saturating_add(1)));
}

/// Report a submit's measured GPU duration against the frame that issued it,
/// tagged with the site the submit came from.
pub fn add_gpu_time(seq: u64, site: niri_vk::stats::SubmitSite, duration: Duration) {
    push_gpu_sample(seq, site, Some(duration));
}

/// Report a timestamp pair that came back unusable. Counted, not silently
/// dropped: a stack that writes only some of its timestamps would otherwise make
/// the reported total a sum over an unknown subset of the frame's passes, i.e. a
/// number that reads like a total and is a floor. With the loss count beside it
/// the reader can tell the two apart.
pub fn add_gpu_lost(seq: u64, site: niri_vk::stats::SubmitSite) {
    push_gpu_sample(seq, site, None);
}

/// Report one phase's share of a submit already reported through
/// [`add_gpu_time`]. Dropped silently outside a frame, like the sample it
/// subdivides.
pub fn add_gpu_phase(seq: u64, phase: niri_vk::stats::GpuPhase, duration: Duration) {
    GPU_PHASE_SAMPLES.with(|s| {
        let mut s = s.borrow_mut();
        if s.len() >= MAX_PENDING_SAMPLES {
            s.remove(0);
        }
        s.push((seq, phase, duration));
    });
}

fn push_gpu_sample(seq: u64, site: niri_vk::stats::SubmitSite, sample: Option<Duration>) {
    GPU_SAMPLES.with(|s| {
        let mut s = s.borrow_mut();
        if s.len() >= MAX_PENDING_SAMPLES {
            s.remove(0);
        }
        s.push((seq, site, sample));
    });
}

/// What the renderer measured for one frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GpuSamples {
    /// Summed duration of the pairs that came back usable.
    pub time: Duration,
    /// The same time, split by the submit each pair belongs to. A frame's `gpu`
    /// figure is a sum over its submits, and "23ms of GPU" says nothing about
    /// whether the effects offscreen or the scanout pass is what to attack —
    /// which is the whole question when a frame is over budget. Indexed by
    /// `SubmitSite::index`.
    pub by_site: [Duration; niri_vk::stats::SubmitSite::ALL.len()],
    /// The same time again, split by *where inside* the command buffer it went.
    /// Orthogonal to `by_site`, not a refinement of it: `by_site` says which
    /// submit, `by_phase` says prepass / render pass / present. Once the submit
    /// discipline folded everything into the frame's own buffer, `by_site` alone
    /// could only ever answer "scanout", so this is the split that can name what
    /// to attack. Indexed by `GpuPhase::index`.
    pub by_phase: [Duration; niri_vk::stats::GpuPhase::ALL.len()],
    /// Pairs that came back unusable.
    pub lost: u64,
    /// Pairs of either kind, i.e. how many of the promised samples have landed.
    pub count: u64,
}

impl GpuSamples {
    fn add(&mut self, site: niri_vk::stats::SubmitSite, sample: Option<Duration>) {
        self.count += 1;
        match sample {
            Some(time) => {
                self.time += time;
                self.by_site[site.index()] += time;
            }
            None => self.lost += 1,
        }
    }

    /// A subdivision of time already added by [`add`](Self::add) — it must not
    /// touch `time`, `count` or `lost`.
    fn add_phase(&mut self, phase: niri_vk::stats::GpuPhase, duration: Duration) {
        self.by_phase[phase.index()] += duration;
    }
}

/// Take every sample belonging to `seq`, leaving the rest.
fn take_gpu_samples_for(seq: u64) -> GpuSamples {
    GPU_SAMPLES.with(|s| {
        let mut s = s.borrow_mut();
        let mut out = GpuSamples::default();
        s.retain(|&(at, site, sample)| {
            if at == seq {
                out.add(site, sample);
                false
            } else {
                true
            }
        });
        GPU_PHASE_SAMPLES.with(|p| {
            p.borrow_mut().retain(|&(at, phase, d)| {
                if at == seq {
                    out.add_phase(phase, d);
                    false
                } else {
                    true
                }
            })
        });
        out
    })
}

/// Take every sample this thread has banked, whatever frame it belongs to. For
/// tests, which render without a frame log and so have no sequence to match.
pub fn take_gpu_samples() -> GpuSamples {
    GPU_SAMPLES.with(|s| {
        let mut out = GpuSamples::default();
        for (_, site, sample) in s.borrow_mut().drain(..) {
            out.add(site, sample);
        }
        GPU_PHASE_SAMPLES.with(|p| {
            for (_, phase, d) in p.borrow_mut().drain(..) {
                out.add_phase(phase, d);
            }
        });
        out
    })
}

/// One measured stretch of a frame. In the order they run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Phase {
    /// Building the scene: layout geometry, widget baking, text shaping — all the
    /// work that decides *what* to draw.
    Elements,
    /// Collecting the render elements for this output into a vec.
    Collect,
    /// `DrmCompositor::render_frame` — recording and submitting the frame, and
    /// (because the renderer is synchronous) waiting for the GPU to finish it.
    Submit,
    /// Handing the finished buffer to KMS.
    Queue,
    /// Waking clients for the next frame.
    Callbacks,
    /// Screencast, recording and screencopy captures riding on this redraw.
    Captures,
}

impl Phase {
    /// In the order they run, which is the order a log line prints them — so a
    /// run of lines reads down the page as the frame reads across.
    const ALL: [Phase; 6] = [
        Phase::Elements,
        Phase::Collect,
        Phase::Submit,
        Phase::Queue,
        Phase::Callbacks,
        Phase::Captures,
    ];

    fn label(self) -> &'static str {
        match self {
            Phase::Elements => "elements",
            Phase::Collect => "collect",
            Phase::Submit => "submit",
            Phase::Queue => "queue",
            Phase::Callbacks => "callbacks",
            Phase::Captures => "captures",
        }
    }
}

/// What the frame was doing, for the log line. Cheap to collect — every field is
/// already at hand where a frame is assembled.
#[derive(Debug, Clone, Copy, Default)]
pub struct FrameContext {
    /// How many render elements this output's frame ended up with.
    pub elements: usize,
    /// Damage tracking was bypassed and the whole output redrawn.
    pub full_damage: bool,
    /// Some animation is still running, so another frame is already due.
    pub animating: bool,
    /// Where the overview sits on its 0..2 state axis, if it is open at all.
    pub overview_state: Option<f64>,
    /// The output's physical area in pixels, to express shading as an overdraw multiple.
    pub output_px: u64,
}

/// What to log, parsed from the environment.
#[derive(Debug, Clone, Copy)]
struct Settings {
    /// Log a frame whose total exceeds this. `None` means "the output's refresh
    /// interval", which is the honest budget but is only known per output.
    threshold: Option<Duration>,
    /// Log every frame regardless.
    log_all: bool,
    /// How often to emit the rolling summary. `None` disables it.
    summary_every: Option<Duration>,
    /// Bank every frame in a bounded in-memory ring instead of writing it out as
    /// it happens, and dump the ring to a file on `SIGUSR1`. `None` = off.
    ///
    /// This exists because `all` is an observer effect that hides from its own
    /// instrument: formatting a ~600-char line and handing it to journald costs
    /// the compositor thread real time *per frame*, but it happens after `total`
    /// is measured, so it inflates the miss rate while leaving the reported frame
    /// cost untouched. Banking the raw record is a move of data the frame already
    /// built — no formatting, no allocation beyond the ring, no I/O.
    ring: Option<usize>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            threshold: None,
            log_all: false,
            summary_every: Some(Duration::from_secs(10)),
            ring: None,
        }
    }
}

/// Largest presentation gap [`Stats::cadence`] counts on its own; anything longer
/// lands in this bucket. Four cycles is 67ms at 60Hz — well past "a hitch".
const CADENCE_MAX: usize = 4;

/// Rolling per-output tallies, reset every time a summary is emitted.
#[derive(Debug, Default)]
struct Stats {
    frames: u64,
    /// Every frame's total, kept so the summary can report percentiles rather
    /// than just a mean — a stutter is a tail event and a mean hides it.
    totals: Vec<Duration>,
    worst: Duration,
    over_budget: u64,
    /// Frames the display never got: gaps in the DRM vblank sequence.
    dropped: u64,
    gpu_total: Duration,
    /// Unusable timestamp pairs over the same window. See [`GPU_LOST`].
    gpu_lost: u64,
    /// Microseconds left in the cycle when each presented frame was handed to
    /// KMS, signed — negative means we handed it over past its own deadline.
    /// See [`FrameLog::queued`].
    headroom_us: Vec<i64>,
    /// How many refresh cycles apart consecutive presentations landed, capped at
    /// [`CADENCE_MAX`]. Index 0 counts same-cycle (two flips reported in one
    /// refresh, which should not happen), 1 counts every-cycle, 2 counts
    /// every-other, and the last bucket is "that or worse".
    ///
    /// The other numbers say what a *frame* did; this says what the *screen*
    /// did, which is the thing a person actually perceives. A run of misses can
    /// be a smooth half rate — visually fine, just 30fps — or a 60fps stream
    /// with a 2-cycle hole punched in it every second, which reads as a hitch.
    /// "22 dropped" cannot tell those apart and they want opposite fixes.
    cadence: [u64; CADENCE_MAX + 1],
    /// The same histogram keyed on what each frame *aimed at* rather than where it landed —
    /// cycles from the previous flip to the frame's target vblank, so index 1 is "aimed at the
    /// next cycle" and index `n` means `n - 1` idle cycles in front of it.
    ///
    /// [`Self::cadence`] cannot answer "does idleness cause misses", because a missed frame lands
    /// a cycle late by construction and so files itself under 2. This one is fixed before the
    /// outcome is known. Both are logged: the pair is what separates intent from result.
    aim: [u64; CADENCE_MAX + 1],
}

impl Stats {
    fn record(&mut self, total: Duration, over: bool, gpu: Duration, gpu_lost: u64) {
        self.frames += 1;
        self.totals.push(total);
        self.worst = self.worst.max(total);
        self.over_budget += u64::from(over);
        self.gpu_total += gpu;
        self.gpu_lost += gpu_lost;
    }

    /// The `p`th percentile by nearest-rank, on a copy sorted in place. Only
    /// called when a summary is due, so the sort is off the per-frame path.
    fn percentile(sorted: &[Duration], p: f64) -> Duration {
        if sorted.is_empty() {
            return Duration::ZERO;
        }
        let rank = ((p / 100. * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
        sorted[rank - 1]
    }
}

/// One phase's stretch of a frame: how long it took, and how much of that some other clause of
/// the line already explains.
///
/// The gap between the two is the point. A phase total says a frame was slow; the buckets
/// (bakes, creations, shaping, submits, waits) say what *kind* of slow — and whatever is left
/// over is work no counter has ever seen. The live seat's first overview open spent 15.95 ms in
/// `collect` against ~5 ms of buckets, and that 11 ms sat unnoticed because reading it required
/// subtracting five numbers by hand off a journal line.
#[derive(Debug, Clone, Copy)]
struct Span {
    phase: Phase,
    wall: Duration,
    attributed: Duration,
}

/// The frame being timed right now.
#[derive(Debug, Clone)]
struct InFlight {
    output: String,
    /// This frame's number, so a GPU sample read back after the frame has ended
    /// can still find it. See [`FRAME_SEQ`].
    seq: u64,
    started: Instant,
    /// When the current phase began — every mark closes one span and opens the next.
    phase_started: Instant,
    /// The attributed-scope union at that same moment, so a span can report how much of itself the
    /// frame line's other clauses account for. See [`niri_vk::stats::enter_attributed`].
    phase_attributed: Duration,
    phase: Option<Phase>,
    spans: Vec<Span>,
    bakes_at_start: u64,
    shapes_at_start: u64,
    submits_at_start: u64,
    draws_at_start: u64,
    shaded_at_start: u64,
    context: FrameContext,
}

/// What a finished frame cost, beyond its per-phase wall clock: the counters and
/// timers that live in process-wide statics because the code that feeds them sits
/// too deep to carry a log handle.
#[derive(Debug, Default, Clone)]
struct Totals {
    gpu: Duration,
    /// `gpu`, split by the submit it was spent in. Indexed by `SubmitSite::index`.
    gpu_sites: [Duration; niri_vk::stats::SubmitSite::ALL.len()],
    /// `gpu`, split by where inside the command buffer it went. Indexed by
    /// `GpuPhase::index`.
    gpu_phases: [Duration; niri_vk::stats::GpuPhase::ALL.len()],
    /// Timestamp pairs the renderer could not use. Nonzero means `gpu` is a
    /// floor, not a total. See [`GPU_LOST`].
    gpu_lost: u64,
    bakes: u64,
    baking: Duration,
    /// Which widgets those bakes belong to. A bake count says a frame paid for a
    /// re-rasterization; only this says what to go and fix.
    bake_sites: Vec<BakeSite>,
    shapes: u64,
    shaping: Duration,
    /// GPU round trips (`vkQueueSubmit` + fence wait). The number to watch on a
    /// virtualized stack, where each one costs milliseconds no matter how little
    /// work it carries.
    submits: u64,
    /// Just the `vkQueueSubmit` calls. Near zero — the round trip is paid below.
    submitting: Duration,
    /// Waiting for submitted work to finish. Timed apart from the enqueue so that
    /// deferring a wait can be told from removing one: a wait handed to KMS leaves
    /// here and does not reappear, while a wait merely moved to the next frame
    /// shows up in that frame's retire. Carries no count, because a retire need not
    /// belong to the frame that issued its submit.
    retiring: Duration,
    /// The same submits, broken down by where they came from. Without this the line says a
    /// frame made fifteen round trips and nothing about which fifteen — and the fix for a bake
    /// is not the fix for an upload. Indexed by `SubmitSite::ALL`'s order.
    sites: [niri_vk::stats::SiteTotals; niri_vk::stats::SubmitSite::ALL.len()],
    /// The frame's *first* wait and where it was paid. Not comparable to the others: every submit
    /// is chained on the queue timeline, so the first one cannot begin until the previous frame —
    /// including the scanout submit the CPU walked away from — has finished on the GPU. Whichever
    /// site goes first absorbs that tail and reads as expensive.
    first_wait: Option<(niri_vk::stats::SubmitSite, Duration)>,
    /// Bytes staged into GPU images. Separates a frame that made many small round trips from one
    /// that moved a wallpaper — different costs, different fixes.
    uploaded: u64,
    /// GPU resources created, and the wall time it took. Not a submit and not free: on a
    /// virtualized driver a `vkCreateImage` round-trips to the host whenever venus misses its
    /// image-requirements cache, so this is collect time that the submit breakdown structurally
    /// cannot see. Added because the seat's worst frames had ~50ms that was neither a fence wait
    /// nor a bake.
    creates: (u64, Duration),
    /// Wall time memcpying host bytes into mapped staging. Separate from `creates` because it is a
    /// different cost with a different fix — first-touch page faults on a freshly mapped buffer,
    /// scaling with payload (`docs/fork/venus-cost.md` §9.2; the mapping itself is cached and does
    /// ~58 GB/s once warm) — and folding it in made a wallpaper frame read as 9.96ms of
    /// "creation".
    staging_write: Duration,
    draws: u64,
    /// Fragments shaded. The number that actually predicts a frame's cost: holding draws fixed
    /// and shrinking the damage rect collapses a frame to its bare submit overhead.
    shaded: u64,
}

/// How many entries the ring holds by default: ~22 minutes at 50 fps. Sized off the
/// fact that the ring fills from *compositor start*, not run start — a measured
/// `drive-workload.sh heavy` pair is ~14.8k entries on its own, so anything close to
/// that evicts the head of the run behind whatever idle frames preceded it. Dumping
/// drains the ring, so the exact way to scope a run is still to dump right before it;
/// this size is what makes forgetting to survivable. Each entry is the record the
/// frame already built, so the cost is memory (~600 B/entry, ~39 MB here), not frame
/// time — and only for sessions that asked for `ring`.
const DEFAULT_RING: usize = 65536;

/// One banked entry, in the order it happened. Frames are kept *unformatted* —
/// formatting is the cost this whole mechanism exists to move off the frame path —
/// while summaries, which are one line per 10 s, are formatted as they happen so
/// the dump needs no aggregation state to reconstruct them.
#[derive(Debug)]
// Boxing the big variant is the usual fix and is wrong here: `Frame` is what the
// ring is almost entirely made of, so a box would add exactly the per-frame
// allocation this mechanism exists to avoid. `Line` is one entry per summary
// period, so the wasted padding on those is a few hundred bytes per 10 seconds.
#[allow(clippy::large_enum_variant)]
enum Entry {
    Frame {
        frame: InFlight,
        total: Duration,
        totals: Totals,
        budget: Option<Duration>,
    },
    Line(String),
}

/// A finished frame waiting for the GPU samples it was promised. See
/// [`FrameLog::parked`].
#[derive(Debug)]
struct Parked {
    frame: InFlight,
    total: Duration,
    totals: Totals,
    budget: Option<Duration>,
    /// Samples the renderer promised, and how many have arrived.
    expected: u64,
    arrived: u64,
}

/// How many finished frames to hold for their samples before giving up on the
/// oldest and emitting it with what it has.
///
/// Deferral keeps at most a couple of submits outstanding, so reaching this means
/// a promised sample is never coming — a submit that errored out, or a device
/// that stopped signalling. Emitting is the right failure: a late line with a
/// short GPU total beats a frame that never appears in the log at all.
const MAX_PARKED_FRAMES: usize = 4;

/// What a frame actually cost, which is not what it *took* once the CPU stopped waiting for the
/// GPU.
///
/// `total` is wall time on the compositor thread. Under a synchronous finish that already contains
/// the GPU, because the thread parked on the fence — so adding [`Totals::gpu`] would double-count.
/// Under async scanout the thread walks away and the GPU work lands on the flip instead, so `total`
/// contains none of it and the budget verdict built on `total` alone reads **every frame as
/// comfortable**: a live seat reported "0 over budget" for a session in which 19% of deep-overview
/// frames needed more than a refresh interval of CPU + GPU (p50 13.83 ms, p90 16.98 ms).
///
/// [`Totals::retiring`] is how long the frame *did* park, so `gpu - retiring` is the GPU time
/// nobody waited for, and one expression is right in both configurations — and in the mixed case
/// where a frame defers its scanout but still waits on an offscreen. It is an upper bound: GPU
/// execution can overlap the CPU recording that follows it, so the true cost is somewhere between
/// `total` and this.
fn frame_cost(total: Duration, totals: &Totals) -> Duration {
    total + totals.gpu.saturating_sub(totals.retiring)
}

/// See the [module docs](self).
#[derive(Debug)]
pub struct FrameLog {
    settings: Option<Settings>,
    in_flight: Option<InFlight>,
    /// Frames whose CPU work is done but whose GPU samples have not all landed —
    /// the renderer walked away from the submit, so the measurement arrives a
    /// frame or two later. Their lines are held back rather than emitted with a
    /// partial total, which is what keeps `gpu` honest once deferral is on.
    /// Bounded by [`MAX_PARKED_FRAMES`]; empty whenever GPU timing is off, since
    /// nothing promises a sample then.
    parked: VecDeque<Parked>,
    /// Banked entries awaiting a `SIGUSR1` dump. Empty unless `ring` is set.
    ring: VecDeque<Entry>,
    /// How many dumps this process has written, so successive windows land in
    /// successive files instead of overwriting one another.
    dumps: u64,
    stats: HashMap<String, Stats>,
    /// Per output, the last frame handed to KMS: what it aimed at and when we let
    /// go of it. Read back in [`FrameLog::presented`] to turn "late" into "late
    /// even though we were done in time" or "late because we were late".
    queued: HashMap<String, (Duration, Duration)>,
    /// Per output, when the last frame reached the screen — the other end of a
    /// presentation interval. See [`Stats::cadence`].
    last_presented: HashMap<String, Duration>,
    last_summary: Instant,
}

impl FrameLog {
    /// Read `NIRI_FRAME_LOG`. Anything unparseable is reported and ignored rather
    /// than failing the session — this is a debugging aid, and a typo in a
    /// session file should not cost you a desktop.
    pub fn from_env() -> Self {
        let settings = std::env::var("NIRI_FRAME_LOG")
            .ok()
            .and_then(|raw| Self::parse(&raw));

        ENABLED.store(settings.is_some(), Ordering::Relaxed);
        niri_vk::stats::set_enabled(settings.is_some());

        if let Some(settings) = &settings {
            tracing::info!(
                "frame logging on: {}, summary {}{}",
                match (settings.log_all, settings.threshold) {
                    (true, _) => "every frame".to_owned(),
                    (false, Some(t)) => format!("over {t:?}"),
                    (false, None) => "over the refresh interval".to_owned(),
                },
                match settings.summary_every {
                    Some(every) => format!("every {every:?}"),
                    None => "off".to_owned(),
                },
                if gpu_timing() {
                    ", with GPU timing"
                } else {
                    ""
                },
            );
        }

        // Reserved up front: growing by doubling would memcpy the whole ring
        // *inside* `end()`, which at these sizes is milliseconds on the frame path —
        // a self-inflicted over-budget frame, once per threshold, landing mid-run
        // and attributed to the workload.
        let ring = settings
            .as_ref()
            .and_then(|s| s.ring)
            .map(VecDeque::with_capacity)
            .unwrap_or_default();

        Self {
            settings,
            in_flight: None,
            parked: VecDeque::new(),
            ring,
            dumps: 0,
            stats: HashMap::new(),
            queued: HashMap::new(),
            last_presented: HashMap::new(),
            last_summary: Instant::now(),
        }
    }

    fn parse(raw: &str) -> Option<Settings> {
        let mut settings = Settings::default();
        let mut enabled = false;

        for part in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let (key, value) = match part.split_once('=') {
                Some((k, v)) => (k.trim(), Some(v.trim())),
                None => (part, None),
            };

            match (key, value) {
                ("0" | "off" | "false", None) => return None,
                ("1" | "on" | "true", None) => enabled = true,
                ("all", None) => {
                    enabled = true;
                    settings.log_all = true;
                }
                // The flag itself comes from the environment, in `gpu_timing` — by
                // now the renderer has almost certainly already read it. All this
                // arm does is keep `gpu` alone a valid way to turn logging on.
                ("gpu", None) => enabled = true,
                ("ring", None) => {
                    enabled = true;
                    settings.ring = Some(DEFAULT_RING);
                }
                ("ring", Some(v)) => match v.parse::<usize>() {
                    Ok(0) => settings.ring = None,
                    Ok(n) => {
                        enabled = true;
                        settings.ring = Some(n);
                    }
                    Err(_) => tracing::warn!("NIRI_FRAME_LOG: bad ring size {v:?}, ignoring"),
                },
                ("summary", Some(v)) => match v.parse::<u64>() {
                    Ok(0) => settings.summary_every = None,
                    Ok(secs) => settings.summary_every = Some(Duration::from_secs(secs)),
                    Err(_) => tracing::warn!("NIRI_FRAME_LOG: bad summary period {v:?}, ignoring"),
                },
                _ => match key.trim_end_matches("ms").parse::<f64>() {
                    Ok(ms) if ms > 0. => {
                        enabled = true;
                        settings.threshold = Some(Duration::from_secs_f64(ms / 1000.));
                    }
                    _ => tracing::warn!("NIRI_FRAME_LOG: unknown option {part:?}, ignoring"),
                },
            }
        }

        enabled.then_some(settings)
    }

    pub fn is_enabled(&self) -> bool {
        self.settings.is_some()
    }

    /// Start timing a frame for `output`. Any frame still in flight is dropped —
    /// a redraw that bailed before [`end`](Self::end) has nothing worth reporting.
    pub fn begin(&mut self, output: &str) {
        if self.settings.is_none() {
            return;
        }

        // The frame's first wait is measured apart from the rest; tell the counters where the
        // frame starts. See `niri_vk::stats::begin_frame`.
        niri_vk::stats::begin_frame();

        // Anything the last frame promised and never delivered is not this frame's;
        // clear it so a failed submit cannot park the next line forever.
        GPU_EXPECTED.with(|c| c.set(0));

        let seq = FRAME_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
        CURRENT_SEQ.store(seq, Ordering::Relaxed);

        let now = Instant::now();
        self.in_flight = Some(InFlight {
            output: output.to_owned(),
            seq,
            started: now,
            phase_started: now,
            phase_attributed: niri_vk::stats::attributed(),
            phase: None,
            spans: Vec::with_capacity(Phase::ALL.len()),
            bakes_at_start: bakes(),
            shapes_at_start: niri_vk::stats::shapes(),
            submits_at_start: niri_vk::stats::submits(),
            draws_at_start: niri_vk::stats::draws(),
            shaded_at_start: niri_vk::stats::shaded(),
            context: FrameContext::default(),
        });
    }

    /// Close the running phase (if any) and open `phase`. Everything between two
    /// marks is attributed to the phase named by the earlier one, so the marks go
    /// *before* the work they name.
    pub fn phase(&mut self, phase: Phase) {
        let Some(frame) = self.in_flight.as_mut() else {
            return;
        };

        let now = Instant::now();
        let attributed = niri_vk::stats::attributed();
        if let Some(previous) = frame.phase.replace(phase) {
            frame.spans.push(Span {
                phase: previous,
                wall: now - frame.phase_started,
                attributed: attributed.saturating_sub(frame.phase_attributed),
            });
        }
        frame.phase_started = now;
        frame.phase_attributed = attributed;
    }

    /// Attach the frame's context. Call once per frame, wherever the numbers are
    /// already known.
    pub fn set_context(&mut self, context: FrameContext) {
        if let Some(frame) = self.in_flight.as_mut() {
            frame.context = context;
        }
    }

    /// Finish the frame, log it if it went over budget, and roll it into the
    /// stats. `refresh` is the output's refresh interval, which is the default
    /// budget.
    pub fn end(&mut self, refresh: Option<Duration>) {
        let Some(settings) = self.settings else {
            return;
        };
        let Some(mut frame) = self.in_flight.take() else {
            return;
        };
        // Whatever renders next — a screenshot, a screencast, the next frame's own bakes before
        // its `begin` — is not this frame's. See `CURRENT_SEQ`.
        CURRENT_SEQ.store(0, Ordering::Relaxed);

        let now = Instant::now();
        if let Some(last) = frame.phase.take() {
            frame.spans.push(Span {
                phase: last,
                wall: now - frame.phase_started,
                attributed: niri_vk::stats::attributed().saturating_sub(frame.phase_attributed),
            });
        }
        let total = now - frame.started;
        let expected = GPU_EXPECTED.with(|c| c.replace(0));
        let samples = take_gpu_samples_for(frame.seq);
        let totals = Totals {
            gpu: samples.time,
            gpu_sites: samples.by_site,
            gpu_phases: samples.by_phase,
            gpu_lost: samples.lost,
            bakes: bakes() - frame.bakes_at_start,
            baking: Duration::from_nanos(BAKE_NANOS.with(|c| c.replace(0))),
            bake_sites: take_bake_sites(),
            shapes: niri_vk::stats::shapes() - frame.shapes_at_start,
            shaping: niri_vk::stats::take_shape_time(),
            submits: niri_vk::stats::submits() - frame.submits_at_start,
            submitting: niri_vk::stats::take_submit_time(),
            retiring: niri_vk::stats::take_retire_time(),
            sites: niri_vk::stats::take_sites(),
            first_wait: niri_vk::stats::take_first_wait(),
            uploaded: niri_vk::stats::take_uploaded_bytes(),
            creates: niri_vk::stats::take_creates(),
            staging_write: niri_vk::stats::take_staging_write(),
            draws: niri_vk::stats::draws() - frame.draws_at_start,
            shaded: niri_vk::stats::shaded() - frame.shaded_at_start,
        };

        // The budget: an explicit threshold if given, else the refresh interval.
        // With neither (a headless output with no refresh) nothing is "too long",
        // so only `all` logs.
        let budget = settings.threshold.or(refresh);

        self.parked.push_back(Parked {
            frame,
            total,
            totals,
            budget,
            expected,
            arrived: samples.count,
        });
        self.flush_parked(settings);

        self.maybe_summarize(now);
    }

    /// Emit every parked frame whose samples have all landed, oldest first, plus
    /// the oldest unconditionally if the queue has outgrown [`MAX_PARKED_FRAMES`].
    ///
    /// Order matters more than promptness here: frames are emitted in the order
    /// they ran, so a run of lines still reads as a timeline even though each one
    /// now appears a frame or two after the work it describes. With GPU timing
    /// off nothing is ever promised, so every frame is complete on arrival and
    /// this is the same synchronous emit it always was.
    fn flush_parked(&mut self, settings: Settings) {
        // Top up every parked frame, not just the front: samples arrive in submit
        // order, but a synchronous frame can complete behind a deferred one.
        for parked in &mut self.parked {
            if parked.arrived < parked.expected {
                let late = take_gpu_samples_for(parked.frame.seq);
                parked.totals.gpu += late.time;
                for (dst, add) in parked
                    .totals
                    .gpu_phases
                    .iter_mut()
                    .zip(late.by_phase.iter())
                {
                    *dst += *add;
                }
                for (dst, add) in parked.totals.gpu_sites.iter_mut().zip(late.by_site.iter()) {
                    *dst += *add;
                }
                parked.totals.gpu_lost += late.lost;
                parked.arrived += late.count;
            }
        }

        while let Some(front) = self.parked.front() {
            if front.arrived < front.expected && self.parked.len() <= MAX_PARKED_FRAMES {
                break;
            }
            let parked = self.parked.pop_front().expect("front exists");
            self.emit(parked, settings);
        }
    }

    fn emit(&mut self, parked: Parked, settings: Settings) {
        let Parked {
            frame,
            total,
            totals,
            budget,
            ..
        } = parked;
        let cost = frame_cost(total, &totals);
        let over = budget.is_some_and(|budget| cost > budget);

        if let Some(cap) = settings.ring {
            // Bank the record unformatted, and format *nothing* here — not even the
            // over-budget frames. An earlier version kept a live `warn!` for those,
            // on the grounds that they are rare and worth seeing without waiting for
            // a dump. That reintroduced the exact confound this mode exists to
            // remove, and did it selectively on the tail: the expensive frames paid
            // a ~600-char format plus a journald write that the healthy ones did not,
            // so the heavy band carried a self-inflicted cost while the light band
            // did not. Biasing *against* the frames under study is still bias. The
            // dump has every one of them; `=1` is the mode for watching live.
            while self.ring.len() >= cap {
                self.ring.pop_front();
            }
            self.ring.push_back(Entry::Frame {
                frame: frame.clone(),
                total,
                totals: totals.clone(),
                budget,
            });
        } else if over || settings.log_all {
            let line = Self::format_frame(&frame, total, &totals, budget);
            if over {
                tracing::warn!("{line}");
            } else {
                tracing::debug!("{line}");
            }
        }

        // The summary's percentiles are the same quantity its over-budget count is, or the two
        // disagree: "p50 1.24ms, 0 over budget" was a true statement about a session whose frames
        // needed 13.83ms.
        self.stats
            .entry(frame.output)
            .or_default()
            .record(cost, over, totals.gpu, totals.gpu_lost);
    }

    fn format_frame(
        frame: &InFlight,
        total: Duration,
        totals: &Totals,
        budget: Option<Duration>,
    ) -> String {
        let mut line = format!("frame on {} took {}", frame.output, ms(total));
        // Only when the two differ, i.e. only when the frame walked away from GPU work that
        // `took` therefore does not contain. See [`frame_cost`].
        let cost = frame_cost(total, totals);
        if cost != total {
            let _ = write!(line, " +{} unwaited GPU = {}", ms(cost - total), ms(cost));
        }
        if let Some(budget) = budget {
            let _ = write!(line, " (budget {})", ms(budget));
        }

        // Phases in a fixed order rather than the order recorded, so successive
        // lines line up when you read a run of them.
        line.push_str(" —");
        for phase in Phase::ALL {
            let mine = frame.spans.iter().filter(|s| s.phase == phase);
            let (spent, attributed) = mine.fold((Duration::ZERO, Duration::ZERO), |(w, a), s| {
                (w + s.wall, a + s.attributed)
            });
            if spent.is_zero() {
                continue;
            }
            let _ = write!(line, " {} {}", phase.label(), ms(spent));
            // Only where some of the phase *is* accounted for: a phase with no buckets at all
            // (`queue`, `callbacks`) would otherwise claim its whole self as a mystery, which is
            // not a finding — nothing ever promised to explain it.
            let residual = spent.saturating_sub(attributed);
            if !attributed.is_zero() && residual >= UNATTRIBUTED_FLOOR {
                let _ = write!(line, " ({} unattributed)", ms(residual));
            }
        }
        if !totals.gpu.is_zero() {
            let _ = write!(line, " (gpu {}", ms(totals.gpu));
            if totals.gpu_lost > 0 {
                let _ = write!(line, ", {} lost", totals.gpu_lost);
            }
            // The split, biggest first, and only when more than one site
            // contributed — on a frame with a single submit it would just restate
            // the total.
            let mut by_site: Vec<_> = niri_vk::stats::SubmitSite::ALL
                .iter()
                .map(|site| (*site, totals.gpu_sites[site.index()]))
                .filter(|(_, d)| !d.is_zero())
                .collect();
            if by_site.len() > 1 {
                by_site.sort_by_key(|(_, d)| std::cmp::Reverse(*d));
                line.push_str(": ");
                for (i, (site, d)) in by_site.iter().enumerate() {
                    if i > 0 {
                        line.push_str(", ");
                    }
                    let _ = write!(line, "{} {}", site.label(), ms(*d));
                }
            }
            // The phase split, in execution order rather than biggest-first: this
            // one is read as a shape ("where did the frame go"), and reordering it
            // per frame makes two lines impossible to compare by eye.
            let by_phase: Vec<_> = niri_vk::stats::GpuPhase::ALL
                .iter()
                .map(|phase| (*phase, totals.gpu_phases[phase.index()]))
                .collect();
            if by_phase.iter().any(|(_, d)| !d.is_zero()) {
                line.push_str(" [");
                for (i, (phase, d)) in by_phase.iter().enumerate() {
                    if i > 0 {
                        line.push_str(", ");
                    }
                    let _ = write!(line, "{} {}", phase.label(), ms(*d));
                }
                line.push(']');
            }
            line.push(')');
        } else if totals.gpu_lost > 0 {
            let _ = write!(line, " (gpu unmeasured, {} lost)", totals.gpu_lost);
        }

        let ctx = &frame.context;
        let _ = write!(line, "; {} elements", ctx.elements);
        let _ = write!(line, ", {} draws", totals.draws);
        if ctx.output_px > 0 && totals.shaded > 0 {
            let _ = write!(
                line,
                " covering {:.1}x the output",
                totals.shaded as f64 / ctx.output_px as f64
            );
        }
        // Submits before bakes and shaping: on a virtualized GPU the round-trip
        // count is usually the headline, and both of those are ways of spending it.
        // Enqueue and wait are printed apart because they move apart: a wait handed
        // to KMS leaves the line, a wait deferred to the next frame reappears there.
        // A zero wait is omitted rather than printed, so its absence is visible.
        if totals.submits > 0 {
            let _ = write!(
                line,
                ", {} submits in {}",
                totals.submits,
                ms(totals.submitting)
            );
            if !totals.retiring.is_zero() {
                let _ = write!(line, ", waiting {}", ms(totals.retiring));
            }
            // Where they came from, worst wait first. A frame's round trips used to be one
            // undifferentiated number, which said a frame made fifteen of them and nothing about
            // which — and a bake, an upload and a blur chain are three different fixes. Sites that
            // submitted nothing are omitted; a site that submitted and waited for nothing prints
            // `0.00ms`, which is the whole point on a deferred scanout.
            let mut by_site: Vec<_> = niri_vk::stats::SubmitSite::ALL
                .iter()
                .zip(&totals.sites)
                .filter(|(_, t)| t.submits > 0)
                .collect();
            by_site.sort_by_key(|(_, t)| std::cmp::Reverse(t.retiring));
            if !by_site.is_empty() {
                line.push_str(" (");
                // The first wait leads, because the rest cannot be read without it: it carries
                // whatever was left of the previous frame, so the site that happens to go first
                // looks expensive whether or not it is.
                if let Some((site, wait)) = totals.first_wait {
                    let _ = write!(line, "first {} {}; ", site.label(), ms(wait));
                }
                for (i, (site, t)) in by_site.iter().enumerate() {
                    if i > 0 {
                        line.push_str(", ");
                    }
                    let _ = write!(line, "{} {} in {}", t.submits, site.label(), ms(t.retiring));
                }
                line.push(')');
            }
            if totals.uploaded > 0 {
                let _ = write!(
                    line,
                    ", {:.1}MiB uploaded in {}",
                    totals.uploaded as f64 / (1 << 20) as f64,
                    ms(totals.staging_write)
                );
            }
            if totals.creates.0 > 0 {
                let _ = write!(
                    line,
                    ", {} created in {}",
                    totals.creates.0,
                    ms(totals.creates.1)
                );
            }
        } else if !totals.retiring.is_zero() {
            // A frame can pay a wait for work it did not submit: retiring a previous
            // frame's in-flight submit. Report it, so that time is never invisible.
            let _ = write!(line, ", waiting {} on earlier work", ms(totals.retiring));
        }
        if totals.bakes > 0 {
            let _ = write!(line, ", {} bakes in {}", totals.bakes, ms(totals.baking));
            // Name them. Capped, because a pathological frame should not push the
            // rest of the line out of a journal entry — the tail is the long pole
            // and the list is sorted by time, so the cap only ever drops the cheap
            // ones. It still says how many it dropped.
            if let Some((shown, rest)) = split_at_most(&totals.bake_sites, BAKE_SITES_SHOWN) {
                let names: Vec<String> = shown.iter().map(BakeSite::to_string).collect();
                let _ = write!(line, " ({}", names.join(", "));
                if rest > 0 {
                    let _ = write!(line, ", +{rest} more");
                }
                let _ = write!(line, ")");
            }
        }
        if totals.shapes > 0 {
            let _ = write!(
                line,
                ", {} shaped runs in {}",
                totals.shapes,
                ms(totals.shaping)
            );
        }
        if ctx.full_damage {
            line.push_str(", full damage");
        }
        if ctx.animating {
            line.push_str(", animating");
        }
        if let Some(state) = ctx.overview_state {
            let _ = write!(line, ", overview {state:.2}");
        }
        line
    }

    /// Record the moment a finished frame was handed to KMS, against the
    /// presentation time it was built for.
    ///
    /// The gap between the two is the frame's *headroom*: how much of the cycle
    /// was left for the kernel's commit worker and the display hardware once we
    /// were out of the way. A missed vblank with healthy headroom and one with
    /// none are different bugs — the first is downstream of us (a fence that
    /// signals late, a host compositor on its own clock), the second is ours.
    /// Without this the log can only say "late", which is true of both.
    pub fn queued(&mut self, output: &str, target: Duration, at: Duration) {
        if self.settings.is_none() {
            return;
        }

        self.queued.insert(output.to_owned(), (target, at));
    }

    /// Record a presented frame: how late it landed against the presentation time
    /// the compositor aimed for when it built it.
    ///
    /// **Not** the gap in the DRM vblank sequence, which is the obvious thing to
    /// measure and is wrong here. The hardware vblank counter advances every
    /// refresh cycle whether or not we flipped, and a damage-driven compositor
    /// deliberately does not flip when nothing changed — so on an idle desktop
    /// the sequence gap measures *idleness*. The first run of this logger
    /// reported "dropped 59 frames" once a second on a static screen, which was
    /// the clock ticking, not a stutter.
    ///
    /// What a user perceives as a stutter is a frame that was meant for a
    /// particular vblank and arrived at a later one. `target` is the deadline the
    /// frame was built against (`FrameClock::next_presentation_time`), `actual` is
    /// when it reached the screen; a difference of a whole refresh cycle or more
    /// is a missed deadline, whatever the compositor was doing before it.
    pub fn presented(
        &mut self,
        output: &str,
        target: Duration,
        actual: Duration,
        refresh: Option<Duration>,
    ) {
        let Some(settings) = self.settings else {
            return;
        };

        // A frame parked for its GPU samples is emitted by whatever happens next, and on an idle
        // output the next `end` can be a second away. The flip is the other thing that happens, and
        // it happens *after* the submit it waited on — so by here the samples have landed.
        self.flush_parked(settings);

        // Only the frame that was built for this target: on a miss the next
        // frame's queue overwrites the entry, and pairing a late presentation
        // with a *later* frame's headroom would read as healthy every time.
        let headroom = self
            .queued
            .get(output)
            .filter(|(queued_target, _)| *queued_target == target)
            .map(|(target, at)| target.as_micros() as i64 - at.as_micros() as i64);

        if let Some(headroom) = headroom {
            self.stats
                .entry(output.to_owned())
                .or_default()
                .headroom_us
                .push(headroom);
        }

        // No hardware clock (`DrmEventTime::Realtime`, or the debug knob that
        // emulates a zero presentation time): nothing to compare against.
        if actual.is_zero() || target.is_zero() {
            return;
        }

        // Without a refresh interval there is no cycle to be late by.
        let Some(refresh) = refresh.filter(|r| !r.is_zero()) else {
            return;
        };

        // What the screen did, independent of what any one frame aimed at: the gap
        // to the previous presentation, in whole cycles. Recorded before the
        // early-outs below, because a frame presented *early* still lands on some
        // vblank and still contributes an interval.
        // Also kept for the miss line below. A flip that immediately follows another and a flip
        // after a quiet stretch are different events on this stack: measured across ~18500 live
        // flips, back-to-back ones miss 1% of the time and *anything* with a cycle of quiet in
        // front of it misses 26-47%, flat from 2 cycles out to 5 seconds. Without this number a
        // miss line cannot tell the two apart, and the whole idle regime reads as one mystery.
        // See `docs/fork/present-misses.md` §10.
        let prev = self.last_presented.insert(output.to_owned(), actual);
        let since_last_flip = prev
            .and_then(|prev| actual.checked_sub(prev))
            .map(|gap| (gap.as_secs_f64() / refresh.as_secs_f64()).round() as usize);
        if let Some(cycles) = since_last_flip {
            self.stats.entry(output.to_owned()).or_default().cadence[cycles.min(CADENCE_MAX)] += 1;
        }

        // The same quantity measured against what the frame *aimed at* instead of where it landed.
        //
        // `since_last_flip` folds effect into cause and cannot be used to ask whether idleness
        // causes misses: a frame that misses lands a cycle later **by construction**, so the
        // 2-cycle bucket collects every continuation frame that missed, and only ever those. It
        // measured 41-84% for that reason as much as any.
        //
        // `target` is chosen when the frame is queued, before it is known whether it will make it,
        // so a miss cannot move this number. Same 1-based vocabulary as the direct gap on purpose —
        // 1 means "aimed at the cycle right after the last flip", so the two histograms line up row
        // for row and the difference between them is the thing under study. Idle cycles in front is
        // this minus one. See `docs/fork/present-misses.md` §12.3/§13.2.
        let aimed_after = prev
            .and_then(|prev| target.checked_sub(prev))
            .map(|gap| (gap.as_secs_f64() / refresh.as_secs_f64()).round() as usize);
        if let Some(cycles) = aimed_after {
            self.stats.entry(output.to_owned()).or_default().aim[cycles.min(CADENCE_MAX)] += 1;
        }

        let Some(late) = actual.checked_sub(target) else {
            // Presented early. That is the frame clock mispredicting downward,
            // not a drop.
            return;
        };

        // Round to whole cycles: landing a hair after the target is the normal
        // scheduling jitter of hitting the same vblank, not a miss.
        let missed = (late.as_secs_f64() / refresh.as_secs_f64()).round() as u64;
        if missed == 0 {
            return;
        }

        self.stats.entry(output.to_owned()).or_default().dropped += missed;

        let queued = match headroom {
            Some(us) if us >= 0 => format!(", queued {} early", ms_us(us)),
            Some(us) => format!(", queued {} LATE", ms_us(-us)),
            None => String::new(),
        };

        let cadence = cadence_clause(since_last_flip);
        let aim = aim_clause(aimed_after);

        tracing::warn!(
            "missed {missed} vblank(s) on {output}: presented {} late, \
             refresh {}{queued}{cadence}{aim}",
            ms(late),
            ms(refresh),
        );
    }

    fn maybe_summarize(&mut self, now: Instant) {
        let Some(every) = self.settings.and_then(|s| s.summary_every) else {
            return;
        };
        let elapsed = now - self.last_summary;
        if elapsed < every {
            return;
        }
        self.last_summary = now;

        // Before the per-output lines, so a hole is visible above the numbers it puts in doubt.
        if let Some(dropped) = take_dropped_lines() {
            tracing::warn!(
                "{dropped} log lines were DROPPED since the last summary — the non-blocking log \
                 writer's buffer overflowed, so this log has holes in it. Set NIRI_LOG_BLOCKING=1 \
                 to make the writer apply backpressure instead (at the cost of stalling whatever \
                 thread is logging)."
            );
        }

        let ring_cap = self.settings.and_then(|s| s.ring);
        let mut banked: Vec<String> = Vec::new();
        for (output, stats) in &mut self.stats {
            if stats.frames == 0 {
                // Still report drops on an output that rendered nothing: that is
                // exactly the shape of a compositor that has stopped keeping up.
                if stats.dropped > 0 {
                    tracing::info!("{output}: no frames, {} dropped", stats.dropped);
                    stats.dropped = 0;
                }
                continue;
            }

            let mut sorted = std::mem::take(&mut stats.totals);
            sorted.sort_unstable();
            let p50 = Stats::percentile(&sorted, 50.);
            let p95 = Stats::percentile(&sorted, 95.);
            let fps = stats.frames as f64 / elapsed.as_secs_f64();

            let gpu = match (stats.gpu_total.is_zero(), stats.gpu_lost) {
                (true, 0) => String::new(),
                (true, lost) => format!(", gpu unmeasured ({lost} lost)"),
                (false, 0) => format!(", gpu avg {}", ms(stats.gpu_total / stats.frames as u32)),
                // The average is over every frame either way, so with samples
                // missing it is a floor — say so rather than let it read as the
                // GPU getting cheaper.
                (false, lost) => format!(
                    ", gpu avg ≥{} ({lost} lost)",
                    ms(stats.gpu_total / stats.frames as u32)
                ),
            };

            // Headroom's tail of interest is the *low* one: the median says
            // whether the cadence has slack at all, p5 says how close the worst
            // frames came to handing over past their own deadline.
            let headroom = if stats.headroom_us.is_empty() {
                String::new()
            } else {
                let mut sorted = std::mem::take(&mut stats.headroom_us);
                sorted.sort_unstable();
                let at = |p: f64| {
                    let rank =
                        ((p / 100. * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
                    ms_us(sorted[rank - 1])
                };
                format!(
                    ", headroom p50 {}, p5 {}, min {}",
                    at(50.),
                    at(5.),
                    ms_us(sorted[0])
                )
            };

            // Only the intervals that say something: an idle desktop's gaps are
            // all "4+" and would drown the run of 1s and 2s that a stutter is.
            let cadence = histogram_clause("cadence", &stats.cadence);
            // The denominator for the miss line's `aimed` tag: without it a session can say how
            // many frames aimed into quiet and missed, but not how many aimed into quiet at all.
            let aim = histogram_clause("aim", &stats.aim);

            let line = format!(
                "{output}: {:.1} fps over {}, p50 {}, p95 {}, worst {}, {} over budget, \
                 {} dropped{gpu}{headroom}{cadence}{aim}",
                fps,
                ms(elapsed),
                ms(p50),
                ms(p95),
                ms(stats.worst),
                stats.over_budget,
                stats.dropped,
            );
            // One line per summary period, so formatting it eagerly costs nothing
            // even in ring mode — and banking it keeps the dump self-sufficient:
            // `correlate-frame-log.py` needs the summaries to close its windows,
            // and they cannot be reconstructed from the frame records alone.
            if ring_cap.is_some() {
                banked.push(line.clone());
            }
            tracing::info!("{line}");

            *stats = Stats::default();
        }

        if let Some(cap) = ring_cap {
            for line in banked {
                while self.ring.len() >= cap {
                    self.ring.pop_front();
                }
                self.ring.push_back(Entry::Line(line));
            }
        }
    }

    /// Format and write every banked entry, oldest first, then clear the ring — so
    /// successive dumps give successive windows rather than re-reporting the same
    /// frames.
    ///
    /// A file rather than the journal on purpose: a dump is tens of thousands of
    /// lines at once, and journald's rate limiter would *drop* some of them, which
    /// is the one failure this whole mechanism cannot tolerate — a log with
    /// invisible holes reads exactly like a log of a session that rendered fewer
    /// frames. The text is byte-identical to the journal's, so
    /// `scripts/correlate-frame-log.py` reads a dump directly.
    pub fn dump(&mut self) -> std::io::Result<(std::path::PathBuf, usize)> {
        use std::io::Write as _;

        let path = dump_path(self.dumps);
        let entries = self.ring.len();

        // Write beside the target and rename into place. A dump is tens of MB onto a
        // tmpfs, so a mid-write ENOSPC is a real outcome — and a half-written file at
        // the expected path is indistinguishable from a complete dump of a shorter
        // window, which is precisely the invisible-hole failure this mechanism cannot
        // tolerate. `rename` is atomic, so the reader sees the whole file or none.
        let tmp = path.with_extension("txt.partial");
        {
            let file = std::fs::File::create(&tmp)?;
            let mut out = std::io::BufWriter::new(file);
            // By reference, not `drain`: dropping a `Drain` discards the whole drained
            // range, so an error partway through used to destroy the entries it had
            // not written yet. The ring is cleared below, once the bytes are safe.
            for entry in self.ring.iter() {
                match entry {
                    Entry::Frame {
                        frame,
                        total,
                        totals,
                        budget,
                    } => writeln!(
                        out,
                        "{}",
                        Self::format_frame(frame, *total, totals, *budget)
                    )?,
                    Entry::Line(line) => writeln!(out, "{line}")?,
                }
            }
            out.flush()?;
        }
        std::fs::rename(&tmp, &path)?;

        self.ring.clear();
        self.dumps += 1;
        Ok((path, entries))
    }
}

/// Where [`FrameLog::dump`] writes. `NIRI_FRAME_LOG_DUMP` overrides it; otherwise
/// the session's runtime dir, which is where the rest of our per-session files live
/// and is writable without any assumption about the cwd.
fn dump_path(nth: u64) -> std::path::PathBuf {
    // An explicit path is taken verbatim, including on a second dump: the caller
    // asked for that name. Everything else is numbered, because the natural way to
    // scope a measurement is dump-to-clear, run, dump-to-collect — and with one
    // fixed name per PID the second signal silently destroys the first window.
    if let Ok(path) = std::env::var("NIRI_FRAME_LOG_DUMP") {
        return std::path::PathBuf::from(path);
    }
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_owned());
    std::path::PathBuf::from(dir).join(format!("niri-frame-log.{}.{nth}.txt", std::process::id()))
}

/// How many bake sites a frame line names before it starts counting the rest.
const BAKE_SITES_SHOWN: usize = 3;

/// How long the display had been quiet in front of a missed flip, in whole refresh cycles.
///
/// Its own function so a test can pin the wording without going through a page flip. The
/// distinction it draws is the one the live data says matters: a flip that immediately follows
/// another misses ~1% of the time on this stack, and one with even a single idle cycle in front of
/// it misses 26–47% — flat from 2 cycles out to 5 seconds. See `docs/fork/present-misses.md` §10.
fn cadence_clause(since_last_flip: Option<usize>) -> String {
    match since_last_flip {
        None => ", first flip".to_owned(),
        Some(1) => ", back-to-back".to_owned(),
        Some(n) => format!(", {n} cycles since the last flip"),
    }
}

/// `, <name> 1×12 2×3 4+×5` — the non-empty buckets of a cycle histogram, or nothing at all.
///
/// Bucket 0 is skipped (two flips inside one cycle should not happen) and the last is `N+`, since
/// it saturates. Shared by the two histograms so they cannot drift into different formats, which
/// matters more than it looks: they are meant to be read side by side.
fn histogram_clause(name: &str, buckets: &[u64; CADENCE_MAX + 1]) -> String {
    let body: String = (1..=CADENCE_MAX)
        .filter(|i| buckets[*i] > 0)
        .map(|i| {
            let label = if i == CADENCE_MAX {
                format!("{i}+")
            } else {
                i.to_string()
            };
            format!(" {label}×{}", buckets[i])
        })
        .collect();

    if body.is_empty() {
        String::new()
    } else {
        format!(", {name}{body}")
    }
}

/// How much quiet was in front of the frame's *target*, which is the same question
/// [`cadence_clause`] asks about its landing — except this one cannot be caused by the miss it is
/// printed on.
///
/// A frame that misses lands a cycle later than it aimed, so the direct gap files every missed
/// continuation frame under "2 cycles" no matter how busy the display was. The target is fixed at
/// queue time, so it separates "this frame was launched into quiet" from "this frame missed". Same
/// 1-based counting as the direct gap so the two read against each other; `n` here is `n - 1` idle
/// cycles in front. See `docs/fork/present-misses.md` §12.3.
fn aim_clause(aimed_after: Option<usize>) -> String {
    match aimed_after {
        None => String::new(),
        Some(1) => ", aimed at the next cycle".to_owned(),
        Some(n) => format!(", aimed {n} cycles after the last flip"),
    }
}

/// How much of a phase has to go unexplained before the line says so.
///
/// Not zero: the union counter reads the clock twice per scope, and a frame with a few hundred
/// scopes carries enough of that overhead to show a residual on a phase that is genuinely fully
/// accounted for. Half a millisecond is comfortably above that and comfortably below anything
/// worth chasing against a 16.67 ms budget.
const UNATTRIBUTED_FLOOR: Duration = Duration::from_micros(500);

/// `(the first `n`, how many were left over)`, or `None` when there is nothing to
/// show. Split out so the "+N more" arithmetic is pinned by a test rather than done
/// inline in a format string.
fn split_at_most<T>(all: &[T], n: usize) -> Option<(&[T], usize)> {
    if all.is_empty() {
        return None;
    }
    let shown = all.len().min(n);
    Some((&all[..shown], all.len() - shown))
}

/// Milliseconds with two decimals — the resolution that matters against a 16.7ms
/// budget, without the noise of `Duration`'s own formatting.
fn ms(duration: Duration) -> String {
    format!("{:.2}ms", duration.as_secs_f64() * 1000.)
}

/// The same, for a signed microsecond count — headroom can be negative, which is
/// the whole point of measuring it.
fn ms_us(us: i64) -> String {
    format!("{:.2}ms", us as f64 / 1000.)
}

#[cfg(test)]
mod tests {
    use niri_vk::stats::SubmitSite;

    use super::*;

    /// The env-var grammar, since it is the only interface this has.
    #[test]
    fn parses_the_documented_forms() {
        assert!(FrameLog::parse("").is_none());
        assert!(FrameLog::parse("0").is_none());
        assert!(FrameLog::parse("off").is_none());

        let on = FrameLog::parse("1").unwrap();
        assert_eq!(on.threshold, None);
        assert!(!on.log_all);
        assert_eq!(on.summary_every, Some(Duration::from_secs(10)));

        let all = FrameLog::parse("all").unwrap();
        assert!(all.log_all);

        let explicit = FrameLog::parse("8").unwrap();
        assert_eq!(explicit.threshold, Some(Duration::from_millis(8)));
        assert_eq!(
            FrameLog::parse("8ms").unwrap().threshold,
            explicit.threshold
        );
        assert_eq!(
            FrameLog::parse("2.5").unwrap().threshold,
            Some(Duration::from_micros(2500))
        );

        let combined = FrameLog::parse("8,summary=5").unwrap();
        assert_eq!(combined.threshold, Some(Duration::from_millis(8)));
        assert_eq!(combined.summary_every, Some(Duration::from_secs(5)));

        assert_eq!(FrameLog::parse("1,summary=0").unwrap().summary_every, None);

        // An unknown option is ignored, not fatal, and does not turn logging on
        // by itself.
        assert!(FrameLog::parse("nonsense").is_none());
        assert!(FrameLog::parse("1,nonsense").is_some());

        // An explicit off anywhere wins, so a session file can disable an
        // inherited setting by appending to it.
        assert!(FrameLog::parse("all,off").is_none());

        // `gpu` turns logging on by itself, but it does *not* carry the GPU-timing
        // flag — see `wants_gpu_timing` and the test below for why that is split.
        assert!(FrameLog::parse("gpu").is_some());
    }

    /// GPU timing is decided by reading `NIRI_FRAME_LOG` directly, not by
    /// `FrameLog::parse` setting a flag, because the renderer asks before the frame
    /// log is built. The whole option was silently dead for a session over exactly
    /// that ordering: the tty backend brings up the device — and its renderer, which
    /// allocates the query pool or does not — while constructing the backend, and
    /// `FrameLog::from_env` runs after. So the query pool was never allocated,
    /// nothing was ever collected, and the log showed no GPU time *and* no losses,
    /// which is indistinguishable from a device that cannot timestamp at all.
    #[test]
    fn gpu_timing_is_read_from_the_variable_not_pushed_by_the_parser() {
        assert!(wants_gpu_timing("gpu"));
        assert!(wants_gpu_timing("1,gpu"));
        assert!(wants_gpu_timing("8ms, gpu ,summary=5"));
        assert!(wants_gpu_timing("1,GPU"));

        assert!(!wants_gpu_timing(""));
        assert!(!wants_gpu_timing("1"));
        assert!(!wants_gpu_timing("all,summary=5"));
        // Substrings must not count: these are whole tokens.
        assert!(!wants_gpu_timing("1,gpuish"));
    }

    /// Per-site totals from `(site, submits, waited)` triples, for the log-line tests. The
    /// submit-enqueue time is left at zero: the line reports the *wait* per site, since that is
    /// where a frame's budget goes.
    fn sites(
        entries: &[(SubmitSite, u64, Duration)],
    ) -> [niri_vk::stats::SiteTotals; SubmitSite::ALL.len()] {
        let mut out = [niri_vk::stats::SiteTotals::default(); SubmitSite::ALL.len()];
        for (site, submits, retiring) in entries {
            let i = SubmitSite::ALL.iter().position(|s| s == site).unwrap();
            out[i] = niri_vk::stats::SiteTotals {
                submits: *submits,
                submitting: Duration::ZERO,
                retiring: *retiring,
            };
        }
        out
    }

    /// A frame that submitted nothing but paid a wait, for the log line below.
    fn empty_frame() -> InFlight {
        let now = Instant::now();
        InFlight {
            seq: 1,
            output: "out".to_owned(),
            started: now,
            phase_started: now,
            phase_attributed: Duration::ZERO,
            phase: None,
            spans: Vec::new(),
            bakes_at_start: 0,
            shapes_at_start: 0,
            submits_at_start: 0,
            draws_at_start: 0,
            shaded_at_start: 0,
            context: FrameContext::default(),
        }
    }

    /// Enqueueing work and waiting for it are reported apart, and a wait is never
    /// dropped from the line just because this frame did not submit it.
    ///
    /// This is the property that lets the synchronous-submit work be measured at
    /// all (`docs/fork/renderer-synchronous-submits.md`). With one number, handing
    /// the scanout fence to KMS and deferring the same wait to the next frame look
    /// identical: both collapse "submits in 14ms" to nothing. Only a wait that
    /// leaves the line *and does not reappear on the next frame* is a real saving.
    #[test]
    fn a_wait_is_told_apart_from_the_submit_that_caused_it() {
        let frame = empty_frame();
        let totals = Totals {
            submits: 2,
            submitting: Duration::from_micros(90),
            retiring: Duration::from_millis(14),
            sites: sites(&[
                (SubmitSite::KmsFrame, 1, Duration::from_micros(12690)),
                (SubmitSite::Upload, 1, Duration::from_micros(1310)),
            ]),
            ..Totals::default()
        };
        let line = FrameLog::format_frame(&frame, Duration::from_millis(17), &totals, None);
        assert!(
            line.contains(
                "2 submits in 0.09ms, waiting 14.00ms (1 scanout in 12.69ms, 1 upload in 1.31ms)"
            ),
            "{line}"
        );

        // The frame's first wait leads the breakdown, because without it the rest cannot be read:
        // it carries whatever the previous frame left running, so the site that happens to go
        // first looks expensive whether or not it is.
        let totals = Totals {
            submits: 2,
            submitting: Duration::from_micros(90),
            retiring: Duration::from_millis(14),
            first_wait: Some((SubmitSite::Upload, Duration::from_micros(13420))),
            sites: sites(&[
                (SubmitSite::KmsFrame, 1, Duration::ZERO),
                (SubmitSite::Upload, 1, Duration::from_millis(14)),
            ]),
            uploaded: 3 << 20,
            creates: (7, Duration::from_micros(4300)),
            staging_write: Duration::from_micros(1200),
            ..Totals::default()
        };
        let line = FrameLog::format_frame(&frame, Duration::from_millis(17), &totals, None);
        assert!(
            line.contains("(first upload 13.42ms; 1 upload in 14.00ms, 1 scanout in 0.00ms)"),
            "{line}"
        );
        assert!(line.contains("3.0MiB uploaded in 1.20ms"), "{line}");
        // Resource creation is neither a submit nor a bake, so it has to say so itself or it is
        // invisible — which is exactly how ~50ms hid on the seat's worst frames.
        assert!(line.contains("7 created in 4.30ms"), "{line}");

        // The shape the fix is aiming for: the submit stays, its wait is gone. The site still
        // reports — with a zero — because "the scanout submit waited for nothing" is the result,
        // and a site that vanished from the line would be indistinguishable from one that never
        // submitted.
        let totals = Totals {
            submits: 1,
            submitting: Duration::from_micros(90),
            sites: sites(&[(SubmitSite::KmsFrame, 1, Duration::ZERO)]),
            ..Totals::default()
        };
        let line = FrameLog::format_frame(&frame, Duration::from_millis(4), &totals, None);
        assert!(
            line.contains("1 submits in 0.09ms (1 scanout in 0.00ms)"),
            "{line}"
        );
        assert!(!line.contains("waiting"), "{line}");

        // And the shape that would mean the wait merely moved: a frame paying for
        // work it never submitted. It must still show up.
        let totals = Totals {
            retiring: Duration::from_millis(8),
            ..Totals::default()
        };
        let line = FrameLog::format_frame(&frame, Duration::from_millis(9), &totals, None);
        assert!(line.contains("waiting 8.00ms on earlier work"), "{line}");
    }

    /// A GPU time with samples missing is a floor, and must not read as a total.
    ///
    /// The dev VM's stack is expected to come back writing *some* of its
    /// timestamps. Averaging a lost pair in as zero would then move the number in
    /// the direction of "the GPU got cheaper" — the exact conclusion the log
    /// exists to support — so the loss has to be visible next to it.
    #[test]
    fn a_lost_timestamp_is_reported_next_to_the_gpu_time() {
        let frame = empty_frame();

        let totals = Totals {
            gpu: Duration::from_micros(2500),
            ..Totals::default()
        };
        let line = FrameLog::format_frame(&frame, Duration::from_millis(9), &totals, None);
        assert!(line.contains("(gpu 2.50ms)"), "{line}");

        let totals = Totals {
            gpu: Duration::from_micros(2500),
            gpu_lost: 3,
            ..Totals::default()
        };
        let line = FrameLog::format_frame(&frame, Duration::from_millis(9), &totals, None);
        assert!(line.contains("(gpu 2.50ms, 3 lost)"), "{line}");

        // And a frame whose every pair was lost must still say something: an
        // absent `gpu` reads as "GPU timing is off", which is a different fact.
        let totals = Totals {
            gpu_lost: 2,
            ..Totals::default()
        };
        let line = FrameLog::format_frame(&frame, Duration::from_millis(9), &totals, None);
        assert!(line.contains("(gpu unmeasured, 2 lost)"), "{line}");

        let line =
            FrameLog::format_frame(&frame, Duration::from_millis(9), &Totals::default(), None);
        assert!(!line.contains("gpu"), "{line}");
    }

    /// A bake count says a frame paid for a re-rasterization; the line has to say
    /// *whose*, because that is the only part that tells you what to fix. The panel
    /// baking on every frame of the overview animation was invisible for as long as
    /// the line only counted.
    #[test]
    fn bakes_are_named_and_the_long_pole_survives_the_cap() {
        let frame = empty_frame();
        let site = |file, line, bakes, us| BakeSite {
            file,
            line,
            bakes,
            time: Duration::from_micros(us),
        };

        let totals = Totals {
            bakes: 3,
            baking: Duration::from_micros(4200),
            bake_sites: vec![site("src/ui/panel.rs", 1540, 3, 4200)],
            ..Totals::default()
        };
        let line = FrameLog::format_frame(&frame, Duration::from_millis(9), &totals, None);
        assert!(
            line.contains("3 bakes in 4.20ms (ui/panel.rs:1540 ×3 4.20ms)"),
            "{line}"
        );

        // Over the cap: the expensive ones are named (the list arrives sorted) and
        // the rest are counted, never silently dropped.
        let totals = Totals {
            bakes: 5,
            baking: Duration::from_micros(5000),
            bake_sites: vec![
                site("src/ui/panel.rs", 10, 1, 3000),
                site("src/ui/dash.rs", 20, 1, 1000),
                site("src/ui/calendar.rs", 30, 1, 600),
                site("src/ui/mru.rs", 40, 1, 300),
                site("src/ui/app_grid.rs", 50, 1, 100),
            ],
            ..Totals::default()
        };
        let line = FrameLog::format_frame(&frame, Duration::from_millis(9), &totals, None);
        assert!(line.contains("ui/panel.rs:10 ×1 3.00ms"), "{line}");
        assert!(line.contains("ui/calendar.rs:30 ×1 0.60ms"), "{line}");
        assert!(line.contains("+2 more"), "{line}");
        assert!(
            !line.contains("app_grid"),
            "the cheapest sites are the ones to drop: {line}"
        );

        // No bakes, no parenthetical.
        let line =
            FrameLog::format_frame(&frame, Duration::from_millis(9), &Totals::default(), None);
        assert!(!line.contains("bakes"), "{line}");
    }

    /// `split_at_most` is the "+N more" arithmetic, where an off-by-one turns a
    /// truncated list into one that reads complete.
    #[test]
    fn split_at_most_counts_what_it_drops() {
        assert_eq!(split_at_most::<u8>(&[], 3), None);
        let all = [1, 2, 3, 4, 5];
        assert_eq!(split_at_most(&all, 3), Some((&all[..3], 2)));
        assert_eq!(split_at_most(&all, 5), Some((&all[..], 0)));
        assert_eq!(split_at_most(&all, 9), Some((&all[..], 0)));
    }

    /// The bake counter must not see another thread's bakes, or a test asserting
    /// "this repaint re-baked nothing" fails on a neighbour's work — which it did,
    /// rarely, while the counter was process-wide. See [`BAKES`].
    #[test]
    fn a_bake_on_another_thread_is_not_counted_here() {
        let before = bakes();
        std::thread::spawn(|| drop(time_bake())).join().unwrap();
        assert_eq!(
            bakes(),
            before,
            "another thread's bake landed in this thread's count"
        );

        let _bake = time_bake();
        assert_eq!(bakes(), before + 1, "this thread's own bake is counted");
    }

    fn test_log() -> FrameLog {
        FrameLog {
            parked: VecDeque::new(),
            ring: VecDeque::new(),
            dumps: 0,
            settings: Some(Settings::default()),
            in_flight: None,
            stats: HashMap::new(),
            queued: HashMap::new(),
            last_presented: HashMap::new(),
            last_summary: Instant::now(),
        }
    }

    /// Promise `n` samples for the frame the log is building.
    ///
    /// Writes the thread-local directly rather than calling [`expect_gpu_sample`],
    /// which reads the process-wide [`CURRENT_SEQ`]: libtest runs these in
    /// parallel, so another test's `end` can zero it between this test's `begin`
    /// and its promise, and the flake would look like the parking logic. The
    /// sequence a sample is *tagged* with is read from the frame itself, and
    /// [`FRAME_SEQ`] only ever hands out unique numbers, so nothing else here is
    /// shared.
    fn promise_gpu_samples(n: u64) {
        GPU_EXPECTED.with(|c| c.set(c.get() + n));
    }

    /// A sample belongs to the frame that *issued* it, not the one that happened
    /// to read it back.
    ///
    /// The distinction only exists because a deferred submit is measured after its
    /// frame has finished on the CPU. Summing into one counter — which is what
    /// this replaced — silently moves an overview frame's 11ms of GPU time onto
    /// the idle frame behind it, i.e. reports the expensive frame as cheap and the
    /// cheap one as expensive. That is worse than no number.
    #[test]
    fn a_sample_belongs_to_the_frame_that_issued_it() {
        let _ = take_gpu_samples();

        add_gpu_time(
            7,
            niri_vk::stats::SubmitSite::KmsFrame,
            Duration::from_millis(5),
        );
        add_gpu_time(
            9,
            niri_vk::stats::SubmitSite::KmsFrame,
            Duration::from_millis(2),
        );
        add_gpu_lost(7, niri_vk::stats::SubmitSite::KmsFrame);

        let seven = take_gpu_samples_for(7);
        assert_eq!(seven.time, Duration::from_millis(5), "wrong frame's time");
        assert_eq!(seven.lost, 1, "the lost pair went to the wrong frame");
        assert_eq!(seven.count, 2, "a promised sample was not accounted for");

        let nine = take_gpu_samples_for(9);
        assert_eq!(nine.time, Duration::from_millis(2));
        assert_eq!(nine.count, 1, "frame 9 took a sample that was not its own");

        assert_eq!(take_gpu_samples().count, 0, "samples outlived their frame");
    }

    /// A frame's GPU time splits by the submit that spent it, and the split
    /// survives the parked-frame top-up.
    ///
    /// This is the whole point of the per-site tag: a heavy transition frame is
    /// two submits — the effects offscreen and the scanout pass — and "gpu 23ms"
    /// cannot tell you which to attack. The failure mode it guards is silent,
    /// because summing is exactly what the total already does: the line keeps
    /// printing a correct total while the breakdown quietly lands on one site.
    #[test]
    fn gpu_time_splits_by_submit_site() {
        use niri_vk::stats::SubmitSite;
        let _ = take_gpu_samples();

        add_gpu_time(11, SubmitSite::OffscreenFrame, Duration::from_millis(5));
        add_gpu_time(11, SubmitSite::KmsFrame, Duration::from_millis(18));
        add_gpu_time(11, SubmitSite::OffscreenFrame, Duration::from_millis(2));

        let s = take_gpu_samples_for(11);
        assert_eq!(
            s.time,
            Duration::from_millis(25),
            "the total must still hold"
        );
        assert_eq!(
            s.by_site[SubmitSite::KmsFrame.index()],
            Duration::from_millis(18)
        );
        assert_eq!(
            s.by_site[SubmitSite::OffscreenFrame.index()],
            Duration::from_millis(7),
            "two offscreen submits must accumulate into one site"
        );
        assert!(
            s.by_site[SubmitSite::Blur.index()].is_zero(),
            "a site that never ran must stay zero"
        );
        // The split must sum to the total, or the line would attribute time twice.
        assert_eq!(s.by_site.iter().sum::<Duration>(), s.time);
    }

    /// Ring mode banks every frame with no formatting and no I/O, and the dump
    /// writes them out in order and empties the ring.
    ///
    /// The property under test is the one that makes ring mode worth having: with
    /// `all`, the compositor thread formats a ~600-char line and hands it to
    /// journald *per frame*, and because that happens after the frame's total is
    /// measured it inflates the miss rate while leaving the reported cost
    /// untouched — an observer effect invisible to its own instrument. So the
    /// assertions are "nothing was written while frames ran" and "everything is
    /// there afterwards".
    #[test]
    fn ring_mode_banks_frames_and_dumps_them() {
        let dir = std::env::temp_dir().join(format!("niri-ring-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dump.txt");
        std::env::set_var("NIRI_FRAME_LOG_DUMP", &path);

        let mut log = test_log();
        log.settings = Some(Settings {
            ring: Some(4),
            ..Settings::default()
        });

        for _ in 0..3 {
            log.begin("out");
            log.end(Some(Duration::from_millis(16)));
        }
        assert_eq!(log.ring.len(), 3, "frames must be banked, not dropped");

        // The ring is bounded: the oldest entries fall off rather than growing
        // without limit in a session left running for hours. Which four survive is
        // the part that matters and the part a length check cannot see — an
        // eviction that dropped the *newest* would keep the count right and hand
        // back the beginning of the session instead of the run you just measured.
        // The output name is per-frame here purely so the entries are telling apart.
        for i in 3..6 {
            log.begin(&format!("out{i}"));
            log.end(Some(Duration::from_millis(16)));
        }
        assert_eq!(log.ring.len(), 4, "the ring must stay at its cap");
        let banked: Vec<String> = log
            .ring
            .iter()
            .map(|e| match e {
                Entry::Frame { frame, .. } => frame.output.clone(),
                Entry::Line(l) => l.clone(),
            })
            .collect();
        assert_eq!(
            banked,
            vec!["out", "out3", "out4", "out5"],
            "the ring must evict the OLDEST and keep the newest, in order"
        );

        let (dumped, n) = log.dump().expect("dump");
        assert_eq!(dumped, path);
        assert_eq!(n, 4);
        assert!(log.ring.is_empty(), "a dump must empty the ring");

        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text.lines().count(),
            4,
            "every banked entry must reach the file"
        );
        assert!(
            text.lines()
                .all(|l| l.starts_with("frame on out") && l.contains(" took ")),
            "dumped lines must be byte-identical to the journal's, so \
             correlate-frame-log.py reads a dump directly: {text}"
        );

        // A second dump reports the window since the first, not the same frames again.
        log.begin("out");
        log.end(Some(Duration::from_millis(16)));
        let (_, n) = log.dump().expect("dump");
        assert_eq!(n, 1, "successive dumps must give successive windows");

        // A dump that cannot be written must leave the ring alone. It used to
        // `drain` as it wrote, and dropping a partly-consumed `Drain` discards the
        // whole range — so an ENOSPC partway through destroyed the frames it had not
        // written yet, on top of leaving a truncated file that reads like a complete
        // dump of a shorter window.
        log.begin("out");
        log.end(Some(Duration::from_millis(16)));
        std::env::set_var("NIRI_FRAME_LOG_DUMP", dir.join("no/such/dir/dump.txt"));
        assert!(
            log.dump().is_err(),
            "a dump into a missing directory must fail"
        );
        assert_eq!(
            log.ring.len(),
            1,
            "a failed dump must not consume the entries it could not write"
        );

        std::env::remove_var("NIRI_FRAME_LOG_DUMP");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Successive dumps must land in successive files. With one fixed name per PID
    /// the second `SIGUSR1` silently destroyed the first window — and the natural
    /// way to scope a measurement is exactly two dumps: one to clear, one to
    /// collect. An explicit `NIRI_FRAME_LOG_DUMP` is still taken verbatim, because
    /// the caller asked for that name.
    #[test]
    fn successive_dumps_do_not_overwrite_each_other() {
        std::env::remove_var("NIRI_FRAME_LOG_DUMP");
        assert_ne!(
            dump_path(0),
            dump_path(1),
            "each dump needs its own file, or the second destroys the first"
        );
    }

    /// `ring` is off unless asked for, and `ring=0` turns it back off.
    #[test]
    fn ring_is_parsed_from_the_environment() {
        assert_eq!(FrameLog::parse("1").unwrap().ring, None);
        assert_eq!(FrameLog::parse("ring").unwrap().ring, Some(DEFAULT_RING));
        assert_eq!(FrameLog::parse("ring=200").unwrap().ring, Some(200));
        // `ring=0` disables the ring without enabling logging on its own, so it
        // needs a companion to be a valid setting at all.
        assert!(FrameLog::parse("ring=0").is_none());
        assert_eq!(FrameLog::parse("1,ring=0").unwrap().ring, None);
        // And `ring` turns logging on by itself, like `all` and `gpu`.
        assert!(FrameLog::parse("ring").is_some());
    }

    /// A frame whose submit was deferred holds its line until the measurement
    /// lands — and the frames behind it wait their turn rather than overtaking.
    #[test]
    fn a_frame_waits_for_the_gpu_samples_it_was_promised() {
        let _ = take_gpu_samples();
        let mut log = test_log();

        log.begin("out");
        promise_gpu_samples(1);
        let deferred = log.in_flight.as_ref().unwrap().seq;
        log.end(Some(Duration::from_millis(16)));
        assert!(
            !log.stats.contains_key("out"),
            "the line was emitted with no GPU time, which reads as a frame that used none"
        );

        // A later frame that promised nothing is still held: lines come out in the
        // order the frames ran, or a run of them stops being a timeline.
        log.begin("out");
        log.end(Some(Duration::from_millis(16)));
        assert_eq!(
            log.parked.len(),
            2,
            "a complete frame overtook a parked one"
        );

        add_gpu_time(
            deferred,
            niri_vk::stats::SubmitSite::KmsFrame,
            Duration::from_millis(9),
        );
        log.flush_parked(log.settings.unwrap());

        assert!(log.parked.is_empty(), "the sample landed and nothing moved");
        let stats = &log.stats["out"];
        assert_eq!(stats.frames, 2);
        assert_eq!(
            stats.gpu_total,
            Duration::from_millis(9),
            "the deferred frame's GPU time never reached its line"
        );
    }

    /// A promise that is never kept must not park the log forever. A submit can
    /// fail after it stamped its command buffer, and the frame it belonged to
    /// still has everything else worth reporting.
    #[test]
    fn a_sample_that_never_arrives_releases_its_frame() {
        let _ = take_gpu_samples();
        let mut log = test_log();

        for _ in 0..MAX_PARKED_FRAMES + 1 {
            log.begin("out");
            promise_gpu_samples(1);
            log.end(Some(Duration::from_millis(16)));
        }

        assert_eq!(
            log.stats["out"].frames, 1,
            "the cap released the wrong number of frames — it must give up on the oldest only"
        );
        assert_eq!(log.parked.len(), MAX_PARKED_FRAMES);
    }

    /// The residual is what this whole mechanism exists to print, so pin the arithmetic and the
    /// wording. 16 ms of `collect` with 5 ms of buckets is 11 ms nobody has ever measured — the
    /// exact shape of the live seat's first overview open, which went unnoticed because reading it
    /// meant subtracting five numbers off a journal line by hand.
    #[test]
    fn a_phase_with_unexplained_time_says_how_much() {
        let mut frame = empty_frame();
        frame.spans.push(Span {
            phase: Phase::Collect,
            wall: Duration::from_millis(16),
            attributed: Duration::from_millis(5),
        });

        let line =
            FrameLog::format_frame(&frame, Duration::from_millis(16), &Totals::default(), None);
        assert!(
            line.contains("collect 16.00ms (11.00ms unattributed)"),
            "{line}"
        );
    }

    /// A phase with no buckets at all — `queue`, `callbacks` — must NOT claim its whole self as
    /// unexplained. Nothing ever promised to explain it, so "100% unattributed" is noise on every
    /// line rather than a finding, and noise on every line is how a real residual gets ignored.
    #[test]
    fn a_phase_nothing_promised_to_explain_stays_quiet() {
        let mut frame = empty_frame();
        frame.spans.push(Span {
            phase: Phase::Queue,
            wall: Duration::from_millis(16),
            attributed: Duration::ZERO,
        });

        let line =
            FrameLog::format_frame(&frame, Duration::from_millis(16), &Totals::default(), None);
        assert!(line.contains("queue 16.00ms"), "{line}");
        assert!(!line.contains("unattributed"), "{line}");
    }

    /// Below the floor the line stays quiet: the union counter reads the clock twice per scope,
    /// and a fully-accounted phase still shows a few microseconds of that.
    #[test]
    fn a_fully_accounted_phase_stays_quiet() {
        let mut frame = empty_frame();
        frame.spans.push(Span {
            phase: Phase::Collect,
            wall: Duration::from_micros(5100),
            attributed: Duration::from_micros(5000),
        });

        let line =
            FrameLog::format_frame(&frame, Duration::from_millis(6), &Totals::default(), None);
        assert!(!line.contains("unattributed"), "{line}");
    }

    /// Phase marks name the work that *follows* them, and the totals add up.
    #[test]
    fn phases_attribute_the_span_after_the_mark() {
        let mut log = test_log();

        log.begin("test-output");
        log.phase(Phase::Elements);
        log.phase(Phase::Submit);
        let frame = log.in_flight.as_ref().unwrap();
        assert_eq!(frame.spans.len(), 1, "the mark closes exactly one span");
        assert_eq!(frame.spans[0].phase, Phase::Elements);

        log.end(Some(Duration::from_millis(16)));
        assert!(log.in_flight.is_none());
        let stats = &log.stats["test-output"];
        assert_eq!(stats.frames, 1);
    }

    /// Each summary reports only the drops that are new. The failure is quiet in both directions:
    /// reporting the running total makes every summary after one overflow scream about drops that
    /// already happened, and forgetting to advance the mark means drops are never mentioned at all
    /// — which is the outcome the counter exists to prevent.
    #[test]
    fn only_new_dropped_lines_are_reported() {
        let mut reported = 0;
        assert_eq!(dropped_delta(0, &mut reported), None, "no drops, no report");
        assert_eq!(dropped_delta(7, &mut reported), Some(7));
        assert_eq!(
            dropped_delta(7, &mut reported),
            None,
            "the same drops were reported twice"
        );
        assert_eq!(
            dropped_delta(10, &mut reported),
            Some(3),
            "only the new ones"
        );
    }

    /// A miss line has to say how long the display had been quiet in front of it. Measured across
    /// ~18 500 live flips, a back-to-back flip misses 1% of the time and one with even a single
    /// idle cycle in front of it misses 26–47% — so the gap, not the lateness, is what sorts the
    /// two populations. The cadence figure is what the VM/VMM side asked for
    /// (`docs/fork/present-misses.md` §9.3/§10), so pin the three shapes it can take.
    #[test]
    fn a_miss_line_says_how_long_the_display_had_been_quiet() {
        assert_eq!(cadence_clause(None), ", first flip");
        assert_eq!(cadence_clause(Some(1)), ", back-to-back");
        assert_eq!(cadence_clause(Some(4)), ", 4 cycles since the last flip");

        // The aim clause sits beside it and says the same thing about the frame's *target*. Silent
        // on the first flip, where there is no previous flip to be quiet since — the direct clause
        // says "first flip" and repeating it would be noise.
        assert_eq!(aim_clause(None), "");
        assert_eq!(aim_clause(Some(1)), ", aimed at the next cycle");
        assert_eq!(aim_clause(Some(4)), ", aimed 4 cycles after the last flip");

        // Both histograms print through one formatter, so they cannot drift apart in a way that
        // makes a side-by-side reading wrong. Bucket 0 is never shown; the top bucket saturates.
        let mut buckets = [0u64; CADENCE_MAX + 1];
        buckets[0] = 7;
        buckets[1] = 12;
        buckets[3] = 1;
        buckets[CADENCE_MAX] = 5;
        assert_eq!(
            histogram_clause("aim", &buckets),
            format!(", aim 1×12 3×1 {CADENCE_MAX}+×5")
        );
        assert_eq!(histogram_clause("cadence", &[0; CADENCE_MAX + 1]), "");

        // And the gap really is measured from the previous *presentation*: two flips four cycles
        // apart must land in the cadence histogram's bucket 4, not 1.
        let refresh = Duration::from_micros(16667);
        let mut log = test_log();
        let first = Duration::from_secs(100);
        log.presented("out", first, first, Some(refresh));
        let second = first + refresh * 4;
        log.presented("out", second, second, Some(refresh));

        assert_eq!(
            log.stats["out"].cadence[4], 1,
            "a flip four cycles after the previous one was not recorded as such: {:?}",
            log.stats["out"].cadence
        );
    }

    /// A frame is missed when it lands a whole refresh cycle or more after the
    /// deadline it was built for — and, crucially, **not** merely because time
    /// passed since the previous frame.
    #[test]
    fn only_a_late_presentation_counts_as_missed() {
        let refresh = Duration::from_micros(16667);
        let mut log = FrameLog {
            parked: VecDeque::new(),
            ring: VecDeque::new(),
            dumps: 0,
            settings: Some(Settings::default()),
            in_flight: None,
            stats: HashMap::new(),
            queued: HashMap::new(),
            last_presented: HashMap::new(),
            last_summary: Instant::now(),
        };
        let dropped = |log: &FrameLog| log.stats.get("out").map_or(0, |s| s.dropped);

        // On time: presented essentially at the target.
        let target = Duration::from_secs(100);
        log.presented(
            "out",
            target,
            target + Duration::from_micros(200),
            Some(refresh),
        );
        assert_eq!(dropped(&log), 0, "hitting the target is not a miss");

        // One cycle late.
        log.presented("out", target, target + refresh, Some(refresh));
        assert_eq!(dropped(&log), 1);

        // Three cycles late.
        log.presented("out", target, target + refresh * 3, Some(refresh));
        assert_eq!(dropped(&log), 4);

        // Early — the frame clock mispredicting downward, not a drop.
        log.presented(
            "out",
            target,
            target - Duration::from_millis(5),
            Some(refresh),
        );
        assert_eq!(dropped(&log), 4);

        // THE case that made the first version useless: an idle desktop redrawing
        // once a second, each frame hitting its own target exactly. A metric based
        // on the gap between presentations would call this 59 dropped frames every
        // second; it is a compositor with nothing to draw.
        let mut log = FrameLog {
            parked: VecDeque::new(),
            ring: VecDeque::new(),
            dumps: 0,
            settings: Some(Settings::default()),
            in_flight: None,
            stats: HashMap::new(),
            queued: HashMap::new(),
            last_presented: HashMap::new(),
            last_summary: Instant::now(),
        };
        for i in 0..5 {
            let target = Duration::from_secs(200) + Duration::from_secs(i);
            log.presented("out", target, target, Some(refresh));
        }
        assert_eq!(dropped(&log), 0, "idle redraws are not dropped frames");

        // No hardware clock, or no refresh interval: nothing to compare against.
        log.presented("out", target, Duration::ZERO, Some(refresh));
        log.presented("out", target, target + refresh * 5, None);
        assert_eq!(dropped(&log), 0);
    }

    /// The cadence histogram counts gaps *between presentations*, which is the one
    /// thing here measured on the screen rather than on a frame — and the only one
    /// that distinguishes a smooth 30fps (every gap 2) from 60fps with holes in it
    /// (mostly 1, occasionally 3), which feel completely different and read as the
    /// same "N dropped".
    #[test]
    fn cadence_counts_the_gaps_between_presentations() {
        let refresh = Duration::from_micros(16667);
        let mut log = FrameLog {
            parked: VecDeque::new(),
            ring: VecDeque::new(),
            dumps: 0,
            settings: Some(Settings::default()),
            in_flight: None,
            stats: HashMap::new(),
            queued: HashMap::new(),
            last_presented: HashMap::new(),
            last_summary: Instant::now(),
        };

        let base = Duration::from_secs(100);
        // Gaps of 1, 1, 2 then 6 cycles. The first presentation opens the interval
        // and is not itself a gap.
        for cycles in [0, 1, 2, 4, 10] {
            let at = base + refresh * cycles;
            log.presented("out", at, at, Some(refresh));
        }

        let cadence = log.stats["out"].cadence;
        assert_eq!(cadence[1], 2, "two consecutive-cycle presentations");
        assert_eq!(cadence[2], 1, "one two-cycle gap");
        assert_eq!(
            cadence[CADENCE_MAX], 1,
            "a six-cycle gap saturates into the last bucket rather than being lost"
        );
        assert_eq!(cadence[0], 0, "nothing landed inside a single cycle");
    }

    /// The `aim` histogram must be blind to whether the frame it describes missed — that is the
    /// entire reason it exists next to `cadence`.
    ///
    /// The confound it removes: a frame that misses lands a cycle later than it aimed, so under
    /// the direct gap *every* missed continuation frame files itself under "2 cycles" and the
    /// 2-cycle bucket cannot be read as "idleness causes misses" — the misses put themselves
    /// there. Here two frames aim at the very next cycle after the previous flip and one of them
    /// lands a cycle late: `cadence` must disagree about them, `aim` must not.
    #[test]
    fn a_miss_moves_the_landing_bucket_but_never_the_aim_bucket() {
        let refresh = Duration::from_micros(16667);
        let mut log = FrameLog {
            parked: VecDeque::new(),
            ring: VecDeque::new(),
            dumps: 0,
            settings: Some(Settings::default()),
            in_flight: None,
            stats: HashMap::new(),
            queued: HashMap::new(),
            last_presented: HashMap::new(),
            last_summary: Instant::now(),
        };

        let base = Duration::from_secs(100);
        // Opens the interval.
        log.presented("out", base, base, Some(refresh));
        // Aimed at the next cycle and made it.
        log.presented("out", base + refresh, base + refresh, Some(refresh));
        // Aimed at the next cycle after *that* and landed one cycle late.
        log.presented("out", base + refresh * 2, base + refresh * 3, Some(refresh));

        let stats = &log.stats["out"];
        assert_eq!(
            stats.aim[1], 2,
            "both frames aimed at the cycle right after the previous flip"
        );
        assert_eq!(stats.aim[2], 0, "no frame aimed into a quiet cycle");
        assert_eq!(
            stats.dropped, 1,
            "the late frame must still be counted as a miss"
        );

        // The landing histogram sees them differently — which is the confound, stated as an
        // assertion so that removing `aim` cannot silently take the distinction with it.
        assert_eq!(stats.cadence[1], 1, "only one frame *landed* back-to-back");
        assert_eq!(
            stats.cadence[2], 1,
            "the missed frame landed 2 cycles after the previous flip purely because it missed"
        );
    }

    /// Headroom is only meaningful paired with the frame it was measured on. The
    /// queue slot holds *one* entry per output, so a miss followed by another
    /// frame leaves the later frame's handover sitting there — reading it against
    /// the earlier target would report the healthy headroom of a frame that is
    /// not the one that missed, i.e. exactly the wrong answer, every time.
    #[test]
    fn headroom_belongs_to_the_frame_that_was_queued() {
        let refresh = Duration::from_micros(16667);
        let mut log = FrameLog {
            parked: VecDeque::new(),
            ring: VecDeque::new(),
            dumps: 0,
            settings: Some(Settings::default()),
            in_flight: None,
            stats: HashMap::new(),
            queued: HashMap::new(),
            last_presented: HashMap::new(),
            last_summary: Instant::now(),
        };
        let headroom = |log: &FrameLog| {
            log.stats
                .get("out")
                .map_or(Vec::new(), |s| s.headroom_us.clone())
        };

        let target = Duration::from_secs(100);
        log.queued("out", target, target - Duration::from_millis(4));
        log.presented("out", target, target, Some(refresh));
        assert_eq!(headroom(&log), [4000], "queued 4ms before its own deadline");

        // The next frame's handover, while the previous target is presented late.
        let next = target + refresh;
        log.queued("out", next, next - Duration::from_millis(9));
        log.presented("out", target, target + refresh, Some(refresh));
        assert_eq!(
            headroom(&log),
            [4000],
            "a mismatched target contributes no headroom"
        );

        // Handed over past its own deadline: negative, and it must stay signed.
        let late = next + refresh;
        log.queued("out", late, late + Duration::from_millis(2));
        log.presented("out", late, late + refresh, Some(refresh));
        assert_eq!(headroom(&log), [4000, -2000]);
    }

    /// A frame that walked away from its GPU work still cost that work, and the budget verdict has
    /// to say so — while a frame that *waited* must not be charged for it twice.
    ///
    /// Under async scanout the compositor thread does not park on the scanout fence, so `took`
    /// stops containing GPU execution and every frame reads as comfortable. The live seat reported
    /// "0 over budget" across a session where 19% of deep-overview frames needed more than a
    /// refresh interval of CPU + GPU. Under a synchronous finish the same `took` already contains
    /// the GPU, so the naive fix double-counts and flags healthy frames.
    #[test]
    fn a_frame_is_charged_for_gpu_work_it_did_not_wait_for() {
        let gpu = Duration::from_millis(9);
        let deferred = Totals {
            gpu,
            retiring: Duration::ZERO,
            ..Totals::default()
        };
        let waited = Totals {
            gpu,
            retiring: Duration::from_millis(10),
            ..Totals::default()
        };

        let cpu = Duration::from_millis(4);
        assert_eq!(
            frame_cost(cpu, &deferred),
            Duration::from_millis(13),
            "a deferred frame's GPU time is not on its thread, but it is still on its deadline"
        );

        // The synchronous shape: `took` is the CPU work *plus* the park, and the park covers the
        // GPU. Charging it again would call a 14ms frame a 23ms one.
        let took = cpu + Duration::from_millis(10);
        assert_eq!(
            frame_cost(took, &waited),
            took,
            "a frame that parked on its fence was charged for the GPU twice"
        );

        // Mixed, which is the real seat: the scanout defers while an offscreen still waits.
        let partly = Totals {
            gpu,
            retiring: Duration::from_millis(3),
            ..Totals::default()
        };
        assert_eq!(frame_cost(cpu, &partly), Duration::from_millis(10));
    }

    /// The verdict and the percentiles must be the same quantity, or a summary can report a p50
    /// well under budget beside a nonzero over-budget count and mean both.
    #[test]
    fn the_budget_verdict_and_the_summary_agree() {
        let _ = take_gpu_samples();
        let mut log = test_log();
        let budget = Duration::from_millis(16);

        log.begin("out");
        promise_gpu_samples(1);
        let seq = log.in_flight.as_ref().unwrap().seq;
        log.end(Some(budget));
        // A frame whose CPU time is trivial but whose GPU work blows the budget on its own.
        add_gpu_time(
            seq,
            niri_vk::stats::SubmitSite::KmsFrame,
            Duration::from_millis(20),
        );
        log.flush_parked(log.settings.unwrap());

        let stats = &log.stats["out"];
        assert_eq!(stats.frames, 1);
        assert_eq!(
            stats.over_budget, 1,
            "a frame needing 20ms of GPU against a 16ms budget was reported as comfortable"
        );
        assert!(
            stats.worst >= Duration::from_millis(20),
            "the summary's worst frame ignores the GPU time the verdict counted: {:?}",
            stats.worst
        );
    }
}
