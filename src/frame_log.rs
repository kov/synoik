// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

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
//! `SYNOIK_FRAME_LOG` takes a comma-separated list:
//!
//! | value | meaning |
//! |---|---|
//! | unset, `0`, `off` | disabled (the default) |
//! | `1`, `on` | log frames over the output's refresh interval |
//! | `<number>` | log frames over that many milliseconds |
//! | `all` | log every frame (very noisy — for short captures) |
//! | `summary=<secs>` | how often to emit the rolling summary; `0` turns it off |
//! | `gpu` | also time the GPU passes (see [`gpu_timing`]) |
//! | `ring[=N]` | bank raw frame records in a bounded ring; dump on `SIGUSR1` |
//! | `autodump[=cycles]` | dump the ring's tail by itself on a miss of `cycles` or more (default 2); implies `ring` |
//!
//! So `SYNOIK_FRAME_LOG=1` for everyday use, `SYNOIK_FRAME_LOG=8,summary=5,gpu` to
//! chase something specific, `SYNOIK_FRAME_LOG=all` to capture a few seconds in
//! full, and **`SYNOIK_FRAME_LOG=ring,gpu,autodump`** to leave running on a session
//! you actually use — that combination costs no per-frame I/O and still leaves a
//! file behind for a hitch nobody was ready to catch.
//!
//! # What it measures
//!
//! Two independent things, because they fail independently:
//!
//! **The `gpu` option works on the dev VM as of 2026-07-26.** It used to produce
//! nothing, for two independent reasons that looked identical from the log. The
//! virtio-gpu/Venus stack advertised timestamp queries and resolved every one to
//! zero — fixed host-side in the host Vulkan driver (`docs/fork/foundation.md`
//! §5), and this VMM now measures 100% usable pairs. And
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
//! - **Main-loop stalls**, from the wall and CPU time of one turn of the event loop
//!   ([`LoopWatch`]). The other two only ever measure a *frame*, so a stutter caused by anything
//!   else on the compositor thread — a D-Bus callback, a burst of client messages, a blocking read
//!   — reports every frame under budget and leaves no trace but a `cadence` bucket. "It stuttered
//!   and the log is clean" is what this answers.
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
    /// is in front of you; it surfaced under `SYNOIK_VK_VALIDATION=1`, which
    /// perturbs timing enough to lose the race about one run in five.
    static BAKES: Cell<u64> = const { Cell::new(0) };

    /// Nanoseconds this thread spent baking during the frame being built.
    ///
    /// This lives *inside* the `collect` phase, which is where the live seat put
    /// 22ms of a 31ms frame with only 18 elements on screen. A phase total says a
    /// frame was slow; this says which part of the widget path it was in. Shaping
    /// — the other half — is counted by [`synoik_vk::stats`], because it happens on
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
    static GPU_SAMPLES: RefCell<Vec<(u64, synoik_vk::stats::SubmitSite, Option<Duration>)>> =
        const { RefCell::new(Vec::new()) };

    /// A frame's GPU time subdivided by where in its command buffer it went.
    /// Separate from [`GPU_SAMPLES`] because these are a *subdivision* of samples
    /// already promised and counted, not promises of their own: they must never
    /// move `count` or `lost`, or a frame would park waiting for a sample that
    /// was only ever a breakdown of another one.
    static GPU_PHASE_SAMPLES: RefCell<Vec<(u64, synoik_vk::stats::GpuPhase, Duration)>> =
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
            synoik_vk::stats::leave_attributed();
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
        // `synoik_vk::stats::enter_attributed` for why the residual is a union and not a sum.
        synoik_vk::stats::enter_attributed();
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
/// `SYNOIK_FRAME_LOG` carries the `gpu` option. See [`FrameLog::from_env`].
///
/// Reads the environment itself instead of waiting to be told, because the first
/// caller is the renderer's constructor and on the tty backend that runs *before*
/// the frame log exists (the device, and its renderer, come up while the backend
/// is being built). Pushing the flag from `from_env` left the query pool
/// unallocated for the whole session, so `gpu` logged nothing — no samples and no
/// losses, which reads exactly like a device that cannot timestamp.
pub fn gpu_timing() -> bool {
    *GPU_TIMING
        .get_or_init(|| std::env::var("SYNOIK_FRAME_LOG").is_ok_and(|raw| wants_gpu_timing(&raw)))
}

/// Does this `SYNOIK_FRAME_LOG` value ask for GPU timing? Split out from
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
pub fn add_gpu_time(seq: u64, site: synoik_vk::stats::SubmitSite, duration: Duration) {
    push_gpu_sample(seq, site, Some(duration));
}

/// Report a timestamp pair that came back unusable. Counted, not silently
/// dropped: a stack that writes only some of its timestamps would otherwise make
/// the reported total a sum over an unknown subset of the frame's passes, i.e. a
/// number that reads like a total and is a floor. With the loss count beside it
/// the reader can tell the two apart.
pub fn add_gpu_lost(seq: u64, site: synoik_vk::stats::SubmitSite) {
    push_gpu_sample(seq, site, None);
}

/// Report one phase's share of a submit already reported through
/// [`add_gpu_time`]. Dropped silently outside a frame, like the sample it
/// subdivides.
pub fn add_gpu_phase(seq: u64, phase: synoik_vk::stats::GpuPhase, duration: Duration) {
    GPU_PHASE_SAMPLES.with(|s| {
        let mut s = s.borrow_mut();
        if s.len() >= MAX_PENDING_SAMPLES {
            s.remove(0);
        }
        s.push((seq, phase, duration));
    });
}

fn push_gpu_sample(seq: u64, site: synoik_vk::stats::SubmitSite, sample: Option<Duration>) {
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
    pub by_site: [Duration; synoik_vk::stats::SubmitSite::ALL.len()],
    /// The same time again, split by *where inside* the command buffer it went.
    /// Orthogonal to `by_site`, not a refinement of it: `by_site` says which
    /// submit, `by_phase` says prepass / render pass / present. Once the submit
    /// discipline folded everything into the frame's own buffer, `by_site` alone
    /// could only ever answer "scanout", so this is the split that can name what
    /// to attack. Indexed by `GpuPhase::index`.
    pub by_phase: [Duration; synoik_vk::stats::GpuPhase::ALL.len()],
    /// Pairs that came back unusable.
    pub lost: u64,
    /// Pairs of either kind, i.e. how many of the promised samples have landed.
    pub count: u64,
}

impl GpuSamples {
    fn add(&mut self, site: synoik_vk::stats::SubmitSite, sample: Option<Duration>) {
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
    fn add_phase(&mut self, phase: synoik_vk::stats::GpuPhase, duration: Duration) {
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

bitflags::bitflags! {
    /// Which animations were running when a frame was built.
    ///
    /// This used to be a single `animating: bool`, which meant a workspace switch, a
    /// window opening and a panel button's fill fade were indistinguishable in the
    /// log — so a report of "switching workspaces stuttered" could not be matched
    /// against the frames that did it. The bits are the *same* predicates the redraw
    /// loop already evaluates to decide whether to queue another frame
    /// (`Synoik::redraw`); naming them costs nothing beyond an OR, and there is one
    /// source of truth because `animating` is derived from this set rather than
    /// accumulated alongside it.
    ///
    /// Bits are deliberately fine-grained where the cost classes differ: a workspace
    /// switch composites *two* workspaces with a crop on the join axis, which is a
    /// different frame shape from any other layout animation.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct AnimCauses: u32 {
        /// A workspace switch — a keyboard/programmatic animation, or a touchpad
        /// gesture being dragged. The gesture case is labelling only: a drag does not
        /// make the compositor queue frames (the input events do), so this bit can be
        /// set on a frame that reports no *ongoing animation*.
        const WORKSPACE_SWITCH = 1 << 0;
        /// The app grid expanding or collapsing within the monitor.
        const APP_GRID_EXPAND = 1 << 1;
        /// A workspace thumbnail sliding shut after a close.
        const THUMB_CLOSE_SLIDE = 1 << 2;
        /// Window-level animations inside a workspace: open, close, move, resize.
        const WINDOWS = 1 << 3;
        /// Any other layout animation this set does not name individually. A frame
        /// carrying only this bit means the layout reported work the breakdown missed
        /// — treat it as a gap in this enum, not as a mystery.
        const LAYOUT_OTHER = 1 << 4;
        /// The exit-confirm or end-session dialog.
        const DIALOG = 1 << 5;
        /// The polkit authentication dialog.
        const POLKIT = 1 << 6;
        /// The screenshot flash.
        const FLASHSPOT = 1 << 7;
        /// The hot-corner ripple.
        const RIPPLE = 1 << 8;
        /// The dock showing, hiding or poking.
        const DOCK = 1 << 9;
        /// The screenshot UI.
        const SCREENSHOT_UI = 1 << 10;
        /// A panel popover (quick settings, calendar, …).
        const PANEL_POPOVER = 1 << 11;
        /// A notification banner.
        const NOTIFICATION = 1 << 12;
        /// An OSD (volume, brightness, …).
        const OSD = 1 << 13;
        /// The alt-tab switcher, including its sub-list fade.
        const SWITCHER = 1 << 14;
        /// The panel itself — button fill fades, the workspace dot morph.
        const PANEL = 1 << 15;
        /// The dash, including its drop gap easing shut.
        const DASH = 1 << 16;
        /// The app grid.
        const APP_GRID = 1 << 17;
        /// An app-folder dialog.
        const FOLDER_DIALOG = 1 << 18;
        /// The overview search cross-fade or entry expand.
        const OVERVIEW_SEARCH = 1 << 19;
        /// A screen transition (the crossfade over a mode set).
        const SCREEN_TRANSITION = 1 << 20;
        /// The lock screen: shield, page crossfade, slide, fade, caps hint or wiggle.
        const LOCK_SCREEN = 1 << 21;
        /// An animated cursor theme.
        const CURSOR = 1 << 22;
        /// A layer-shell surface animating.
        const LAYER_SURFACE = 1 << 23;
        /// The overview opening, closing or moving along its state axis.
        const OVERVIEW = 1 << 24;
        /// A drag-and-drop in progress, which keeps frames coming so the view can scroll.
        const DND = 1 << 25;
        /// An interactive window move.
        const INTERACTIVE_MOVE = 1 << 26;
        /// The workspace row's phantom slot easing back shut after a drag left it.
        const THUMB_PHANTOM = 1 << 27;
        /// The workspace row easing to where it belongs after a drop or a reorder.
        const THUMB_ROW_SLIDE = 1 << 28;
        /// The workspace peek sliding the thumbnail strip onto the live desktop.
        const WORKSPACE_PEEK = 1 << 29;
        /// The workspace row's scroll held past a click on one of its thumbnails, running out.
        const THUMB_SCROLL_FREEZE = 1 << 30;
    }
}

impl AnimCauses {
    /// Causes that are an ongoing **state** rather than a transition with an end.
    ///
    /// An animated cursor redraws for as long as it is on screen, and the lock screen keeps its
    /// clock ticking for as long as it is up. Each is a perfectly good reason to draw another frame
    /// — which is why they are in this set at all — but neither is ever going to *finish*, so
    /// anything waiting for the compositor to come to rest must wait for the other causes only.
    ///
    /// `INTERACTIVE_MOVE` is deliberately **not** here even though a held drag does not end on its
    /// own. The transitions a drag drives — a preview shrinking to its drag size, the dash
    /// reordering under it — are reported under that same bit, so masking it would report a drag
    /// as settled before its animation had run at all.
    pub const ONGOING: Self = Self::CURSOR.union(Self::LOCK_SCREEN);

    /// Lowercase, hyphenated names of the set bits, for the log line. Allocates, so
    /// this is for formatting a frame that is already being written out — never on
    /// the banking path, which stores the raw bits.
    pub fn names(self) -> Vec<&'static str> {
        // `bitflags`' own `Debug` prints `WORKSPACE_SWITCH | PANEL`, which is fine
        // for a dump but reads badly in a line that is otherwise prose. The explicit
        // table also keeps the log's vocabulary stable if a constant is ever renamed.
        const TABLE: &[(AnimCauses, &str)] = &[
            (AnimCauses::WORKSPACE_SWITCH, "workspace-switch"),
            (AnimCauses::APP_GRID_EXPAND, "app-grid-expand"),
            (AnimCauses::THUMB_CLOSE_SLIDE, "thumb-close-slide"),
            (AnimCauses::WINDOWS, "windows"),
            (AnimCauses::LAYOUT_OTHER, "layout-other"),
            (AnimCauses::DIALOG, "dialog"),
            (AnimCauses::POLKIT, "polkit"),
            (AnimCauses::FLASHSPOT, "flashspot"),
            (AnimCauses::RIPPLE, "ripple"),
            (AnimCauses::DOCK, "dock"),
            (AnimCauses::SCREENSHOT_UI, "screenshot-ui"),
            (AnimCauses::PANEL_POPOVER, "panel-popover"),
            (AnimCauses::NOTIFICATION, "notification"),
            (AnimCauses::OSD, "osd"),
            (AnimCauses::SWITCHER, "switcher"),
            (AnimCauses::PANEL, "panel"),
            (AnimCauses::DASH, "dash"),
            (AnimCauses::APP_GRID, "app-grid"),
            (AnimCauses::FOLDER_DIALOG, "folder-dialog"),
            (AnimCauses::OVERVIEW_SEARCH, "overview-search"),
            (AnimCauses::SCREEN_TRANSITION, "screen-transition"),
            (AnimCauses::LOCK_SCREEN, "lock-screen"),
            (AnimCauses::CURSOR, "cursor"),
            (AnimCauses::LAYER_SURFACE, "layer-surface"),
            (AnimCauses::OVERVIEW, "overview"),
            (AnimCauses::WORKSPACE_PEEK, "workspace-peek"),
            (AnimCauses::DND, "dnd"),
            (AnimCauses::INTERACTIVE_MOVE, "interactive-move"),
            (AnimCauses::THUMB_PHANTOM, "thumb-phantom"),
            (AnimCauses::THUMB_ROW_SLIDE, "thumb-row-slide"),
            (AnimCauses::THUMB_SCROLL_FREEZE, "thumb-scroll-freeze"),
        ];
        TABLE
            .iter()
            .filter(|(bit, _)| self.contains(*bit))
            .map(|(_, name)| *name)
            .collect()
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
    /// Which animations were running, so another frame is already due. Empty means
    /// the frame was not animating.
    pub animating: AnimCauses,
    /// Where the overview sits on its 0..2 state axis, if it is open at all.
    pub overview_state: Option<f64>,
    /// The workspace peek's progress, when the strip is on the live desktop rather than in the
    /// overview. Separate from `overview_state` because the two are different scenes with
    /// different costs, and a settled peek stamps nothing else.
    pub peek_state: Option<f64>,
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
    /// Dump the tail of the ring by itself when a presentation misses at least this
    /// many refresh cycles. `None` = off; implies [`Self::ring`].
    ///
    /// This is what makes the recorder useful for a stutter you cannot reproduce.
    /// The manual path requires noticing the hitch and getting `SIGUSR1` in before
    /// the ring rolls over, which is exactly the thing that fails for a one-off.
    ///
    /// **The threshold is at least 2 for a reason.** On this stack a *single* missed
    /// cycle is routine — `docs/fork/foundation.md` measures ~12% of presented
    /// frames landing one cycle late, unresolved and largely host-side. Triggering
    /// on that would dump continuously and tell you nothing. Two cycles (33ms at
    /// 60Hz) is past the point where a person sees a hitch rather than a statistic.
    autodump: Option<u64>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            threshold: None,
            log_all: false,
            summary_every: Some(Duration::from_secs(10)),
            ring: None,
            autodump: None,
        }
    }
}

/// How many banked entries an automatic dump keeps: the run-up to the miss, not the
/// whole ring. ~10 s at 60 Hz on one output, and the interesting part of a stutter is
/// the second or two before it.
const AUTODUMP_TAIL: usize = 600;

/// Shortest gap between two automatic dumps. A bad stretch misses many times over,
/// and without this the first hitch's dump would be immediately overwritten in spirit
/// by a hundred more — each one costing synchronous I/O on the compositor thread
/// while the session is already struggling.
const AUTODUMP_COOLDOWN: Duration = Duration::from_secs(60);

/// Most automatic dumps in one session. A session that misses all day would otherwise
/// fill the state directory unattended; the first few carry the same information.
const AUTODUMP_MAX: u64 = 20;

/// Default trigger, in missed refresh cycles. See [`Settings::autodump`] for why this
/// is not 1.
const AUTODUMP_DEFAULT_CYCLES: u64 = 2;

/// CPU time on the compositor thread, outside the frame path, that counts as a stall.
///
/// A quarter of a 60Hz budget. Anything at or above this is work that has to come out
/// of the frame the loop owes next, and it is invisible to every other instrument
/// here — the frame log times a frame from `begin` to `end`, so cost incurred in some
/// other event source's callback simply is not in it.
const STALL_CPU: Duration = Duration::from_millis(4);

/// Wall time the compositor thread spent **not running** — no CPU accrued — with a
/// redraw already queued.
///
/// This is the other stall class and the one nothing else can see. A handler that
/// makes a synchronous D-Bus round trip, reads a cold file, or waits on a lock burns
/// no CPU while it does so, so it looks exactly like an idle poll. What separates
/// them is that a frame was already due: an idle loop with nothing pending is
/// supposed to sit in poll, and an idle loop with a frame queued is not.
///
/// Two refresh intervals at 60Hz. One would fire on the ordinary wait for the vblank
/// timer that schedules the redraw in the first place.
const STALL_BLOCKED: Duration = Duration::from_millis(32);

/// Shortest gap between two stall warnings. A pathological stretch stalls every
/// iteration, and the warning is a formatted journal line on the very thread that is
/// already behind.
const STALL_COOLDOWN: Duration = Duration::from_secs(1);

/// This thread's CPU time, which advances only while the thread is actually running.
///
/// The whole loop watch rests on that: a window's wall time covers the poll *and*
/// every event source's callback, and CPU time is what separates "we were blocked
/// waiting for something" from "we were busy". No portable Rust API exposes it, hence
/// the raw `clock_gettime`.
/// Whether one turn of the event loop counts as a stall, and what to call it.
///
/// Split out of [`FrameLog::loop_turn_end`] so the thresholds can be pinned by tests:
/// the live path reads two real clocks, and nothing about a wall-clock reading is
/// reproducible. This is the whole decision — the caller only formats it.
///
/// `other_cpu` is CPU burned in the window that was not the frame; `blocked` is wall
/// time in which the thread ran nothing at all.
fn stall_verdict(
    other_cpu: Duration,
    blocked: Duration,
    redraw_was_pending: bool,
) -> Option<&'static str> {
    let busy = other_cpu >= STALL_CPU;
    // Blocked time is only a fault if a frame was owed. An idle loop with nothing
    // pending is *supposed* to sit in poll, and reporting that would bury the real
    // signal under one line per idle second.
    let stuck = redraw_was_pending && blocked >= STALL_BLOCKED;

    match (busy, stuck) {
        (true, true) => Some("busy and blocked"),
        (true, false) => Some("busy"),
        (false, true) => Some("blocked with a frame due"),
        (false, false) => None,
    }
}

fn thread_cpu() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // Safe: `ts` is a valid, correctly-sized output for a clock the kernel always
    // provides. A failure leaves it zeroed, which reads as "no CPU accrued" — the
    // conservative direction, since it can only *suppress* a CPU-stall report.
    unsafe {
        libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts);
    }
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

/// Watches the compositor thread for time spent *outside* the frame path.
///
/// The gap this closes: every other instrument in this module measures a frame, from
/// [`FrameLog::begin`] to [`FrameLog::end`]. A stutter caused by something else on the
/// thread — a D-Bus callback, a burst of client messages, a blocking read — produces
/// no slow frame at all. The frame log reports every frame under budget and the only
/// surviving trace is a `cadence` bucket, which says a gap happened and nothing about
/// why. That signature ("it stuttered but the log is clean") had no owner.
///
/// The window measured is *between* consecutive runs of the post-dispatch callback,
/// which is exactly one turn of `calloop`'s loop: the poll, every source callback it
/// dispatched, and the callback body itself. Redraw's own CPU is subtracted, because
/// redraw runs inside a source callback and the frame log already reports it in far
/// more detail.
#[derive(Debug, Default)]
struct LoopWatch {
    /// Wall and thread-CPU at the end of the previous callback — the start of the
    /// window being measured. `None` before the first one; that iteration is skipped
    /// rather than reported as an enormous stall from process start.
    last_end: Option<(Instant, Duration)>,
    /// Whether a redraw was already queued when the previous callback ended. Sampled
    /// *then*, not now: the question is whether the loop owed a frame during the
    /// window, and by the end of it the redraw may have happened.
    redraw_was_pending: bool,
    /// Thread CPU spent inside frames during the window.
    redraw_cpu: Duration,
    /// When the last stall was warned about, for [`STALL_COOLDOWN`].
    last_warned: Option<Instant>,
    /// Total stalls seen this session, reported by the IPC perf request.
    stalls: u64,
}

/// Upper edges, in microseconds, of the [`DispatchLateness`] buckets; the last bucket
/// collects everything beyond the final edge.
///
/// Spaced to answer one question: is a released frame late by *scheduling* amounts
/// (tens of microseconds — a timerfd wakeup) or by *milliseconds* (the loop was busy,
/// or the vCPU was not running). The margin sweeps needed 6-8ms of slack to reach
/// parity, and the difference between those two explanations is the difference between
/// a 50-line timer source and a new event loop.
const LATENESS_EDGES_US: [u64; 7] = [100, 250, 500, 1_000, 2_000, 4_000, 8_000];

/// How late a deadline-dispatched frame actually started, against the moment it was
/// armed for.
///
/// Measured on *every* held frame, not only the ones that then missed: "queued N ms
/// LATE" prints on a miss, and a sample conditioned on missing is exactly the sample
/// that cannot tell you how often release is punctual.
#[derive(Debug, Default)]
struct DispatchLateness {
    /// Counts per bucket, edges from [`LATENESS_EDGES_US`], plus one overflow bucket.
    buckets: [u64; LATENESS_EDGES_US.len() + 1],
    count: u64,
    total: Duration,
    worst: Duration,
}

impl DispatchLateness {
    fn record(&mut self, lateness: Duration) {
        let us = lateness.as_micros() as u64;
        let bucket = LATENESS_EDGES_US
            .iter()
            .position(|edge| us < *edge)
            .unwrap_or(LATENESS_EDGES_US.len());
        self.buckets[bucket] += 1;
        self.count += 1;
        self.total += lateness;
        self.worst = self.worst.max(lateness);
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
    /// This thread's CPU time when the frame started, so [`LoopWatch`] can subtract
    /// the frame's own work from the loop iteration that contained it.
    cpu_started: Duration,
    /// When the current phase began — every mark closes one span and opens the next.
    phase_started: Instant,
    /// The attributed-scope union at that same moment, so a span can report how much of itself the
    /// frame line's other clauses account for. See [`synoik_vk::stats::enter_attributed`].
    phase_attributed: Duration,
    phase: Option<Phase>,
    spans: Vec<Span>,
    bakes_at_start: u64,
    shapes_at_start: u64,
    submits_at_start: u64,
    draws_at_start: u64,
    shaded_at_start: u64,
    shaded_by_site_at_start: [u64; synoik_vk::stats::DrawSite::ALL.len()],
    blitted_by_site_at_start: [u64; synoik_vk::stats::BlitSite::ALL.len()],
    context: FrameContext,
}

/// What a finished frame cost, beyond its per-phase wall clock: the counters and
/// timers that live in process-wide statics because the code that feeds them sits
/// too deep to carry a log handle.
#[derive(Debug, Default, Clone)]
struct Totals {
    gpu: Duration,
    /// `gpu`, split by the submit it was spent in. Indexed by `SubmitSite::index`.
    gpu_sites: [Duration; synoik_vk::stats::SubmitSite::ALL.len()],
    /// `gpu`, split by where inside the command buffer it went. Indexed by
    /// `GpuPhase::index`.
    gpu_phases: [Duration; synoik_vk::stats::GpuPhase::ALL.len()],
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
    sites: [synoik_vk::stats::SiteTotals; synoik_vk::stats::SubmitSite::ALL.len()],
    /// The frame's *first* wait and where it was paid. Not comparable to the others: every submit
    /// is chained on the queue timeline, so the first one cannot begin until the previous frame —
    /// including the scanout submit the CPU walked away from — has finished on the GPU. Whichever
    /// site goes first absorbs that tail and reads as expensive.
    first_wait: Option<(synoik_vk::stats::SubmitSite, Duration)>,
    /// Bytes staged into GPU images. Separates a frame that made many small round trips from one
    /// that moved a wallpaper — different costs, different fixes.
    uploaded: u64,
    /// GPU resources created, and the wall time it took. Not a submit and not free: on a
    /// virtualized driver a `vkCreateImage` round-trips to the host whenever venus misses its
    /// image-requirements cache, so this is collect time that the submit breakdown structurally
    /// cannot see. Added because the seat's worst frames had ~50ms that was neither a fence wait
    /// nor a bake.
    creates: (u64, Duration),
    /// The same creations, split by constructor. Which resource is being made every frame is the
    /// actionable half — a blur chain, an offscreen and a staging buffer are three different bugs
    /// with three different fixes — and the bare count cannot say. See
    /// [`synoik_vk::stats::take_create_sites`].
    create_sites: Vec<synoik_vk::stats::CreateSite>,
    /// Wall time memcpying host bytes into mapped staging. Separate from `creates` because it is a
    /// different cost with a different fix — first-touch page faults on a freshly mapped buffer,
    /// scaling with payload (`docs/fork/foundation.md` §5; the mapping itself is cached and does
    /// ~58 GB/s once warm) — and folding it in made a wallpaper frame read as 9.96ms of
    /// "creation".
    staging_write: Duration,
    /// `(barriers, descriptor writes, descriptor allocations)` this frame recorded. Counted, not
    /// timed: each is negligible on its own, and what costs is that venus forwards every one to
    /// the host, so a frame issuing many pays inside its fence wait — where this log reads the
    /// time as `waiting … first scanout` rather than as GPU execution. `creates` explains that gap
    /// when something was allocated; these are for when nothing was. The first frame of an
    /// overview open is the open case: ~11ms of wait beyond its GPU time, with zero creates.
    host_calls: (u64, u64, u64),
    /// Render passes this frame began. On a tile-based host a pass boundary resolves and reloads
    /// tile memory for the whole target — tens of megabytes at 4K, spent without shading a
    /// fragment or blitting a pixel, so no other counter here can see it.
    render_passes: u64,
    /// Destination pixels blitted this frame, split by [`synoik_vk::stats::BlitSite`].
    ///
    /// Blits are **not draws**, so `shaded` cannot see them, yet they are recorded inside the GPU
    /// timestamp bracket — which is how a frame reports GPU time its coverage cannot explain.
    /// `perf_probe`'s sweep 7 named this blind spot and then had to guess at its size.
    blitted_by_site: [u64; synoik_vk::stats::BlitSite::ALL.len()],
    draws: u64,
    /// Fragments shaded. The number that actually predicts a frame's cost: holding draws fixed
    /// and shrinking the damage rect collapses a frame to its bare submit overhead.
    shaded: u64,
    /// [`Self::shaded`] split by what the draw was for. The total alone says a frame shades six
    /// screens and gives no clue which one to attack; the classes are unrelated levers (see
    /// [`synoik_vk::stats::DrawSite`]).
    shaded_by_site: [u64; synoik_vk::stats::DrawSite::ALL.len()],
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

/// Whether the `SYNOIK_DEBUG_DAMAGE` per-frame damage log is on.
///
/// The damage a **screen** is repainted with is not observable any other way. It lives inside
/// `DrmCompositor`, and every capture path re-renders the scene instead of reading the buffers, so
/// a screenshot of a stale screen comes back clean — which says the scene is right, and nothing at
/// all about what was repainted. This is the only view of the actual ask.
///
/// Read once. Off by default: on, it logs a line per frame per output.
pub fn damage_log_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("SYNOIK_DEBUG_DAMAGE").is_some())
}

/// One line per frame: what the screen was told to repaint, and into what.
///
/// `age` names the buffer the rects answer for — 1 is "since the previous frame", which is what a
/// single-buffered screen would need, and 2 is what a double-buffered one needs, because the
/// buffer it hands back last held the frame two ago. A region that changed and appears in neither
/// is a region the screen keeps showing stale.
///
/// `plane` says whether the frame was composited into a swapchain buffer or handed to the scanout
/// plane as a client's own buffer: a region that stops being on a plane has to be repainted by
/// whoever takes it over, and that is a different failure from a missing rect.
pub fn log_frame_damage(
    output: &str,
    plane: &str,
    elements: usize,
    ages: [(
        usize,
        &[smithay::utils::Rectangle<i32, smithay::utils::Physical>],
    ); 2],
) {
    tracing::info!("{}", frame_damage_line(output, plane, elements, ages));
}

/// The log line itself, so its shape is pinned without a subscriber.
fn frame_damage_line(
    output: &str,
    plane: &str,
    elements: usize,
    ages: [(
        usize,
        &[smithay::utils::Rectangle<i32, smithay::utils::Physical>],
    ); 2],
) -> String {
    /// Past this many rects the tail says how many there were rather than printing them.
    const SHOW: usize = 12;

    let rects = |damage: &[smithay::utils::Rectangle<i32, smithay::utils::Physical>]| {
        if damage.is_empty() {
            return String::from("none");
        }
        let mut out = damage
            .iter()
            .take(SHOW)
            .map(|r| format!("{}x{}+{}+{}", r.size.w, r.size.h, r.loc.x, r.loc.y))
            .collect::<Vec<_>>()
            .join(" ");
        if damage.len() > SHOW {
            let _ = write!(out, " (+{} more)", damage.len() - SHOW);
        }
        out
    };

    let [(age_a, damage_a), (age_b, damage_b)] = ages;
    format!(
        "damage {output} plane={plane} elements={elements} age{age_a}=[{}] age{age_b}=[{}]",
        rects(damage_a),
        rects(damage_b),
    )
}

/// Whether the session asked to be told when an element drops one of its instances, via
/// `SYNOIK_DEBUG_INSTANCES=1`.
///
/// Read once. Off by default: on, it keeps a per-output map of every element's instances and
/// diffs it each frame.
pub fn instance_watch_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("SYNOIK_DEBUG_INSTANCES").is_some())
}

/// One instance of one element, as far as the damage tracker's `instance_matches` cares.
///
/// The z-index here is the element's position in the frame's list, where the tracker's is its
/// position among the elements it actually *renders* — it skips ones hidden behind opaque
/// regions. The two agree except where something became fully occluded, and disagreeing makes
/// this report a shrink the tracker would have handled, never miss one it would not: a z-index
/// that moved makes the survivor mismatch, which is the branch that damages everything.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Instance {
    pub geometry: smithay::utils::Rectangle<i32, smithay::utils::Physical>,
    pub alpha: f32,
    pub z_index: usize,
}

/// Every element of `elements` by id, ready to be diffed against the next frame's.
pub fn instances_of<E>(
    elements: &[E],
    scale: smithay::utils::Scale<f64>,
) -> std::collections::HashMap<String, Vec<Instance>>
where
    E: smithay::backend::renderer::element::Element,
{
    let mut by_id: std::collections::HashMap<String, Vec<Instance>> =
        std::collections::HashMap::new();
    for (z_index, elem) in elements.iter().enumerate() {
        by_id
            .entry(format!("{:?}", elem.id()))
            .or_default()
            .push(Instance {
                geometry: elem.geometry(scale),
                alpha: elem.alpha(),
                z_index,
            });
    }
    by_id
}

/// Report every element that dropped an instance while a sibling stayed put — the one way the
/// damage tracker under-reports.
///
/// It allows an id to appear many times in a frame and decides per instance, matching each against
/// *any* remembered one. An instance that moves matches none, takes the branch that damages its new
/// geometry and every remembered instance, and so heals the rect it left. An instance that simply
/// goes away heals nothing: the survivor matches, so the cheap branch runs, and `elements_gone`
/// only fires for an id that left the frame altogether. The rect the departed instance covered is
/// then asked for by nobody, in that frame or any after it — it sits in whichever screen buffer
/// missed the repaint and resurfaces every time that buffer comes round. Pinned by
/// `tests::damage_instances`.
///
/// We rely on multiple instances by design: one cached texture draws a window in the workspace and
/// again in every thumbnail showing it. So the answer is not to stop sharing ids, and this says
/// which element to look at when a stale rectangle appears.
pub fn log_instance_shrinks(
    name: &str,
    geometry: smithay::utils::Rectangle<i32, smithay::utils::Physical>,
    before: &std::collections::HashMap<String, Vec<Instance>>,
    after: &std::collections::HashMap<String, Vec<Instance>>,
) {
    for line in instance_shrinks(before, after, geometry) {
        tracing::info!("instances {name} {line}");
    }
}

/// The offenders themselves, one line each, so the same rule serves the log and the corpus.
///
/// Three conditions, all of them the tracker's own:
///
/// - the count shrank, so an instance left;
/// - **every** surviving instance matches a remembered one. One that does not takes the branch that
///   damages this id's whole remembered instance list — which heals the departed rects too, so a
///   frame where anything of this id moved is a frame that repairs itself. Measured on the seat:
///   without this, four fifths of the report is elements mid-relayout that are fine;
/// - at least one vacated rect lands on the output, since the tracker clips its damage there and an
///   instance that left from off-screen never owed any.
pub fn instance_shrinks(
    before: &std::collections::HashMap<String, Vec<Instance>>,
    after: &std::collections::HashMap<String, Vec<Instance>>,
    output: smithay::utils::Rectangle<i32, smithay::utils::Physical>,
) -> Vec<String> {
    let mut lines = Vec::new();
    for (id, was) in before {
        let Some(is) = after.get(id) else {
            // The id left the frame entirely, which `elements_gone` damages in full.
            continue;
        };
        if is.len() >= was.len() || !is.iter().all(|i| was.contains(i)) {
            continue;
        }
        let vacated = was
            .iter()
            .filter(|i| !is.contains(i))
            .filter_map(|i| i.geometry.intersection(output))
            .map(|geo| format!("{}x{}+{}+{}", geo.size.w, geo.size.h, geo.loc.x, geo.loc.y))
            .collect::<Vec<_>>();
        if vacated.is_empty() {
            continue;
        }
        lines.push(format!(
            "{id} {} -> {} vacated=[{}]",
            was.len(),
            is.len(),
            vacated.join(" ")
        ));
    }
    lines.sort();
    lines
}

/// Log which *elements* a frame's `scene` overdraw went to, when `SYNOIK_SCENE_BREAKDOWN` is set.
///
/// The frame line splits coverage by [`DrawSite`](synoik_vk::stats::DrawSite) — scene vs blur vs
/// text — which is enough to say *which class* to attack and no more. Once the answer is "the
/// scene", the next question is which of the ninety-odd elements in an overview frame are paying
/// for it, and that needs the element list, which only the render path has.
///
/// An element's geometry **is** its shaded area: measured headlessly on a settled overview frame,
/// the sum of `geometry ∩ output` over every element came to 1.68x the output against a `scene`
/// counter reading 1.68x. So this needs no new counter, just the arithmetic.
///
/// Off by default and one atomic load when off. On, it logs one frame in [`EVERY`]: the breakdown
/// of a settled scene does not change, and a per-frame log of ninety elements would push
/// everything else out of the journal.
pub fn log_scene_breakdown<E>(
    elements: &[E],
    scale: smithay::utils::Scale<f64>,
    output: smithay::utils::Rectangle<i32, smithay::utils::Physical>,
) where
    E: smithay::backend::renderer::element::Element + std::fmt::Debug,
{
    /// Log one frame in this many.
    const EVERY: u64 = 240;

    static MODE: OnceLock<Option<bool>> = OnceLock::new();
    let verbose = match MODE.get_or_init(|| match std::env::var("SYNOIK_SCENE_BREAKDOWN") {
        Ok(v) => Some(v == "verbose"),
        Err(_) => None,
    }) {
        Some(verbose) => *verbose,
        None => return,
    };
    static SEEN: AtomicU64 = AtomicU64::new(0);
    if !SEEN.fetch_add(1, Ordering::Relaxed).is_multiple_of(EVERY) {
        return;
    }
    if let Some(line) = scene_breakdown(elements, scale, output) {
        tracing::info!("{line}");
    }
    if verbose {
        for line in scene_elements(elements, scale, output) {
            tracing::info!("{line}");
        }
    }
}

/// Walk `elements` front-to-back the way the renderer does, yielding the ones it would actually
/// shade, each with the area it costs.
///
/// smithay's rule, matched here exactly: an element whose output-clipped geometry is *entirely*
/// covered by the opaque regions declared above it is skipped and costs nothing; every other one
/// is drawn over its whole geometry, so a partially covered element still pays in full. A hidden
/// element does not contribute its own opaque regions, because it never gets that far.
///
/// Summing geometry without the skip is how this instrument came to report a full-output backdrop
/// that was already culled — a whole output of cost attributed to an element that costs nothing,
/// which is the most expensive kind of wrong an attribution tool can be.
fn shaded_elements<E>(
    elements: &[E],
    scale: smithay::utils::Scale<f64>,
    output: smithay::utils::Rectangle<i32, smithay::utils::Physical>,
) -> Vec<(usize, &E, f64)>
where
    E: smithay::backend::renderer::element::Element,
{
    use smithay::utils::Rectangle;

    let mut opaque: Vec<Rectangle<i32, smithay::utils::Physical>> = Vec::new();
    let mut shaded = Vec::with_capacity(elements.len());
    for (i, e) in elements.iter().enumerate() {
        let geo = e.geometry(scale);
        let Some(clipped) = geo.intersection(output) else {
            continue;
        };
        let visible = Rectangle::subtract_rects_many([clipped], opaque.iter().copied());
        let visible_area: i64 = visible
            .iter()
            .map(|r| i64::from(r.size.w.max(0)) * i64::from(r.size.h.max(0)))
            .sum();
        if visible_area == 0 {
            continue;
        }
        opaque.extend(
            e.opaque_regions(scale)
                .iter()
                .map(|r| {
                    let mut r = *r;
                    r.loc += geo.loc;
                    r
                })
                .filter_map(|r| r.intersection(output)),
        );
        let area = f64::from(clipped.size.w.max(0)) * f64::from(clipped.size.h.max(0));
        shaded.push((i, e, area));
    }
    shaded
}

/// One line per element, for when the per-kind [`scene_breakdown`] has named the expensive class
/// and the question becomes *which* element that is and *why it is not culled*.
///
/// Both of those are answered by the same three numbers: an element covering the whole output that
/// is fully opaque hides everything below it, and one that is not is a full-screen blend that
/// nothing can remove — it can only be merged into its neighbour or not drawn. So each line carries
/// the clipped area, the element's `alpha`, and how much of that area it declares opaque. Verbose
/// only (`SYNOIK_SCENE_BREAKDOWN=verbose`): a settled overview frame is ninety-odd lines.
fn scene_elements<E>(
    elements: &[E],
    scale: smithay::utils::Scale<f64>,
    output: smithay::utils::Rectangle<i32, smithay::utils::Physical>,
) -> Vec<String>
where
    E: smithay::backend::renderer::element::Element + std::fmt::Debug,
{
    let output_px = u64::from(output.size.w.max(0) as u32) * u64::from(output.size.h.max(0) as u32);
    if output_px == 0 {
        return Vec::new();
    }
    let px = output_px as f64;
    shaded_elements(elements, scale, output)
        .into_iter()
        .filter_map(|(i, e, area)| {
            let g = e.geometry(scale);
            // Below a hundredth of the output an element cannot be the answer to "what is drawing
            // 3.87x", and there are dozens of them.
            if area / px < 0.01 {
                return None;
            }
            // `Element::opaque_regions` answers relative to the element, so the rects have to be
            // moved to where the element sits before they can be clipped to the output. Without
            // the offset every element away from the origin reads as declaring nothing opaque —
            // which is exactly the answer this column exists to distinguish from the real thing.
            let declared = e.opaque_regions(scale);
            let opaque: f64 = declared
                .iter()
                .map(|r| {
                    let mut r = *r;
                    r.loc += g.loc;
                    r
                })
                .filter_map(|r| r.intersection(output))
                .map(|r| f64::from(r.size.w.max(0)) * f64::from(r.size.h.max(0)))
                .sum();
            // "declared nothing" and "declared something that clipped away to nothing" are
            // different answers and the difference is the whole point of this column, so say
            // which. They are otherwise told apart only by a *negative* zero — `Sum for f64`
            // folds from `-0.0`, so an empty region list prints `-0.00` and a zero-area one
            // prints `0.00`. Nobody reads a log that way.
            let opaque = if declared.is_empty() {
                "none".to_owned()
            } else {
                format!("{:.2}x", opaque / px)
            };
            let name = format!("{e:?}");
            let kind = name.split(['(', ' ', '{']).next().unwrap_or("?");
            Some(format!(
                "  scene element #{i} {kind} {:.2}x alpha={:.2} opaque={} {}x{}+{}+{} {:?}",
                area / px,
                e.alpha(),
                opaque,
                g.size.w,
                g.size.h,
                g.loc.x,
                g.loc.y,
                e.id(),
            ))
        })
        .collect()
}

/// The line [`log_scene_breakdown`] logs, split out so it can be asserted on — the arithmetic is
/// the whole point of it, and a `tracing` call proves nothing in a test.
pub fn scene_breakdown<E>(
    elements: &[E],
    scale: smithay::utils::Scale<f64>,
    output: smithay::utils::Rectangle<i32, smithay::utils::Physical>,
) -> Option<String>
where
    E: smithay::backend::renderer::element::Element + std::fmt::Debug,
{
    let output_px = u64::from(output.size.w.max(0) as u32) * u64::from(output.size.h.max(0) as u32);
    if output_px == 0 {
        return None;
    }

    let mut by_kind: HashMap<String, (usize, f64)> = HashMap::new();
    let mut total = 0.0;
    // Clipped to the output and occlusion-aware, because that is what gets shaded: an element
    // hanging off the edge — every overview element does, mid-animation — pays only for the part
    // inside, and one entirely behind opaque regions pays nothing at all. A sum that ignores
    // either reads high and stays plausible, which is the worst way for an instrument to be wrong;
    // `the_scene_breakdown_totals_what_was_actually_shaded` is the guard.
    for (_, e, area) in shaded_elements(elements, scale, output) {
        total += area;
        // The `Debug` prefix is the element enum's variant name — the granularity that answers
        // "what is this" without adding a trait method to every element type.
        let name = format!("{e:?}");
        let kind = name.split(['(', ' ', '{']).next().unwrap_or("?").to_owned();
        let entry = by_kind.entry(kind).or_insert((0, 0.0));
        entry.0 += 1;
        entry.1 += area;
    }
    let mut ranked: Vec<(String, (usize, f64))> = by_kind.into_iter().collect();
    ranked.sort_by(|a, b| b.1 .1.total_cmp(&a.1 .1));
    let px = output_px as f64;
    let shares: Vec<String> = ranked
        .iter()
        .take(8)
        .filter(|(_, (_, a))| a / px >= 0.01)
        .map(|(kind, (n, a))| format!("{kind} {:.2}x n={n}", a / px))
        .collect();
    Some(format!(
        "scene breakdown: {:.2}x the output over {} elements — {}",
        total / px,
        elements.len(),
        shares.join(", "),
    ))
}

/// Why a frame's damage happened, per element — the question no other instrument here answers.
///
/// The damage log says *what* a frame asked to repaint; the scene breakdown says *what it cost*.
/// Neither says **which element made the ask**, so a full-output repaint over a stable element set
/// reads as unattributable: nothing in the list changed size, count or kind, yet the tracker asks
/// for everything.
///
/// This mirrors the six axes smithay's `OutputDamageTracker` actually compares
/// (`ElementInstanceState::matches`: src, geometry, transform, alpha, z-index, framebuffer-effect
/// flag) plus the commit counter, and names the elements that failed to match. Mirroring a subset
/// would be worse than nothing: on a frame whose culprit moved along an axis we do not watch, the
/// instrument would report "nobody changed" — an omission that reads as a clean bill of health.
///
/// It deliberately does **not** try to predict the composed damage rect. The tracker unions our
/// per-element rects with the previous frames' damage for the buffer's age and then runs them
/// through `DamageShaper`, which merges and re-splits; a predicted rect could never match, and
/// chasing that difference would only measure the shaper. What it predicts instead is the union
/// *area* of the damage its own inputs justify, so the honest comparison is available: inputs quiet
/// but the composed frame full-output means the amplifier is inside the tracker, not in our
/// elements — and that is a different investigation with a different fix.
///
/// Z-index is counted over the **shaded** elements only, the same way the tracker counts it: an
/// element culled by the opaque regions above it never reaches `render_element_z_index`, so
/// counting raw list positions would report a z-shift on every frame where a hidden element came
/// or went.
#[derive(Debug, Default)]
pub struct DamageAttribution {
    prev: HashMap<smithay::backend::renderer::element::Id, ElementSnapshot>,
    /// The last culprit set reported, so a steady state logs once and then counts.
    last_signature: Option<String>,
    /// Frames elided since [`Self::last_signature`] was logged.
    suppressed: u64,
}

/// One element as the tracker last saw it. Instances are kept as a list because a single [`Id`]
/// may legally appear more than once in a frame, and the tracker matches an instance against *any*
/// of the previous ones.
///
/// [`Id`]: smithay::backend::renderer::element::Id
#[derive(Debug, Clone)]
struct ElementSnapshot {
    commit: smithay::backend::renderer::utils::CommitCounter,
    instances: Vec<TrackedInstance>,
    /// Kept for the report only — an id alone cannot be read by a human.
    kind: String,
}

#[derive(Debug, Clone, PartialEq)]
struct TrackedInstance {
    src: smithay::utils::Rectangle<f64, smithay::utils::Buffer>,
    geometry: smithay::utils::Rectangle<i32, smithay::utils::Physical>,
    transform: smithay::utils::Transform,
    alpha: f32,
    z_index: usize,
    framebuffer_effect: bool,
}

/// Why one element contributed damage.
///
/// The order is the reporting priority for an [`Id`] appearing more than once in a frame, resolved
/// by `min`: a changed axis names a mechanism, a bare commit bump only says the client drew, so an
/// element with one instance that moved and another that merely committed is reported as *moved*.
/// `New` and `Gone` cannot mix with the rest — both are decided by whether the id was there at all.
///
/// [`Id`]: smithay::backend::renderer::element::Id
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Reason {
    /// Absent last frame: the tracker damages its whole geometry.
    New,
    /// Present last frame, gone now: the tracker damages where it *was*.
    Gone,
    Geometry,
    Src,
    Transform,
    Alpha,
    /// The element did not move, but something above it appeared or vanished, so every element
    /// below shifted down one. Cheap to cause and expensive to pay for: it invalidates the match
    /// for the whole tail of the list at once.
    ZIndex,
    FramebufferEffect,
    /// Matched on every axis; the element reported its own damage since our last commit.
    Commit,
}

impl Reason {
    fn as_str(self) -> &'static str {
        match self {
            Reason::New => "new",
            Reason::Gone => "gone",
            Reason::Geometry => "geometry",
            Reason::Src => "src",
            Reason::Transform => "transform",
            Reason::Alpha => "alpha",
            Reason::ZIndex => "z-index",
            Reason::FramebufferEffect => "fb-effect",
            Reason::Commit => "commit",
        }
    }
}

/// One element's contribution to a frame, as this instrument accounts for it.
#[derive(Debug, Clone)]
pub struct Culprit {
    pub kind: String,
    pub reason: &'static str,
    /// Damage rects this element justified, clipped to the output.
    pub rects: usize,
    /// Their union area as a multiple of the output's area.
    pub share: f64,
}

/// What one frame's inputs justify. Returned rather than logged so tests can assert on it —
/// a `tracing` call proves nothing.
#[derive(Debug, Clone, Default)]
pub struct FrameAttribution {
    /// Elements that survived the occlusion cull, i.e. the ones the tracker actually compared.
    pub shaded: usize,
    /// Of those, how many matched on every axis and reported no damage.
    pub quiet: usize,
    /// Union area of all justified damage, as a multiple of the output area.
    pub predicted: f64,
    /// Non-quiet elements, largest share first.
    pub culprits: Vec<Culprit>,
}

impl FrameAttribution {
    /// The culprit *set*, ignoring how much each one damaged. Two frames with the same signature
    /// are the same story, and logging the second one adds nothing — which is the whole reason
    /// this instrument can afford to run every frame.
    fn signature(&self) -> String {
        let mut parts: Vec<String> = self
            .culprits
            .iter()
            .map(|c| format!("{}:{}", c.kind, c.reason))
            .collect();
        parts.sort();
        parts.dedup();
        parts.join(",")
    }

    /// The human-readable line, without the suppression count.
    ///
    /// Only the largest few culprits are named, but the ones left out are **counted by reason**
    /// rather than dropped. A truncated list that does not say it is truncated reads as the whole
    /// story: the first live capture showed a pointer changing identity next to four renumbered
    /// windows and invited the conclusion that the pointer renumbered them, when the element that
    /// actually changed the count could have been sitting just past the cut.
    fn line(&self) -> String {
        const NAMED: usize = 6;

        let top: Vec<String> = self
            .culprits
            .iter()
            .take(NAMED)
            .map(|c| format!("{} {} {:.3}x n={}", c.kind, c.reason, c.share, c.rects))
            .collect();
        let mut line = format!(
            "predicted {:.3}x over {} shaded ({} quiet) — {}",
            self.predicted,
            self.shaded,
            self.quiet,
            if top.is_empty() {
                "nothing changed".to_owned()
            } else {
                top.join(", ")
            },
        );
        if self.culprits.len() > NAMED {
            let mut by_reason: Vec<(&str, usize)> = Vec::new();
            for c in &self.culprits[NAMED..] {
                match by_reason.iter_mut().find(|(r, _)| *r == c.reason) {
                    Some((_, n)) => *n += 1,
                    None => by_reason.push((c.reason, 1)),
                }
            }
            by_reason.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
            let tail: Vec<String> = by_reason.iter().map(|(r, n)| format!("{n}x {r}")).collect();
            let _ = write!(
                line,
                " (+{} smaller: {})",
                self.culprits.len() - NAMED,
                tail.join(", "),
            );
        }
        line
    }
}

/// Union area of `rects` as a fraction of `output_px`, computed by subtracting each rect against
/// the ones already counted. Overlapping damage is the norm — a moved element contributes both its
/// old and its new geometry — so summing areas would double-count exactly where the number matters.
fn union_share(
    rects: &[smithay::utils::Rectangle<i32, smithay::utils::Physical>],
    output_px: f64,
) -> f64 {
    use smithay::utils::Rectangle;

    let mut disjoint: Vec<Rectangle<i32, smithay::utils::Physical>> = Vec::new();
    let mut area = 0f64;
    for r in rects {
        for piece in Rectangle::subtract_rects_many([*r], disjoint.iter().copied()) {
            area += f64::from(piece.size.w.max(0)) * f64::from(piece.size.h.max(0));
            disjoint.push(piece);
        }
    }
    area / output_px
}

impl DamageAttribution {
    /// Account for one frame and roll the state forward.
    ///
    /// `elements` must be the list **as the damage tracker will see it** — in particular, before
    /// any debug overlay is spliced in. The overlay inserts a variable number of tint elements at
    /// index 0, which z-shifts everything below it; attributing that shift to the elements it
    /// happened to land on would be the instrument reporting its own presence.
    // smithay's element `Id` carries an `Arc<AtomicBool>` liveness flag, which is interior
    // mutability clippy cannot tell apart from a key that can change its own hash. It cannot: `Id`
    // hashes and compares on its identity alone, and the tracker we are mirroring keys its own
    // per-element state by `Id` for exactly that reason.
    #[allow(clippy::mutable_key_type)]
    pub fn frame<E>(
        &mut self,
        elements: &[E],
        scale: smithay::utils::Scale<f64>,
        output: smithay::utils::Rectangle<i32, smithay::utils::Physical>,
    ) -> FrameAttribution
    where
        E: smithay::backend::renderer::element::Element + std::fmt::Debug,
    {
        use smithay::backend::renderer::element::Id;

        let output_px = f64::from(output.size.w.max(0)) * f64::from(output.size.h.max(0));
        if output_px == 0. {
            return FrameAttribution::default();
        }

        let mut now: HashMap<Id, ElementSnapshot> = HashMap::new();
        // Per element, the rects it justified — kept apart from the frame total so a culprit can
        // report its own share without re-deriving it.
        let mut per_element: HashMap<
            Id,
            (
                Reason,
                Vec<smithay::utils::Rectangle<i32, smithay::utils::Physical>>,
            ),
        > = HashMap::new();
        let mut all_rects = Vec::new();
        let mut quiet = 0usize;

        let shaded = shaded_elements(elements, scale, output);
        for (z_index, (_, e, _)) in shaded.iter().enumerate() {
            let id = e.id().clone();
            let geometry = e.geometry(scale);
            let instance = TrackedInstance {
                src: e.src(),
                geometry,
                transform: e.transform(),
                alpha: e.alpha(),
                z_index,
                framebuffer_effect: e.is_framebuffer_effect(),
            };
            let name = format!("{e:?}");
            let kind = name.split(['(', ' ', '{']).next().unwrap_or("?").to_owned();

            let previous = self.prev.get(&id);
            let matched = previous.is_some_and(|p| p.instances.contains(&instance));

            let mut rects = Vec::new();
            let reason = if let Some(p) = previous {
                if matched {
                    // Same as the tracker: ask the element what changed since the commit we last
                    // saw. Element-local rects, so they need the geometry offset before they mean
                    // anything on the output.
                    let since: Vec<_> = e
                        .damage_since(scale, Some(p.commit))
                        .iter()
                        .map(|d| {
                            let mut d = *d;
                            d.loc += geometry.loc;
                            d
                        })
                        .filter_map(|d| d.intersection(output))
                        .collect();
                    if since.is_empty() {
                        quiet += 1;
                        None
                    } else {
                        rects.extend(since);
                        Some(Reason::Commit)
                    }
                } else {
                    // A mismatch damages both where it is and where it was, so both go in.
                    if let Some(clipped) = geometry.intersection(output) {
                        rects.push(clipped);
                    }
                    rects.extend(
                        p.instances
                            .iter()
                            .filter_map(|i| i.geometry.intersection(output)),
                    );
                    // Name the first axis that differs from the nearest previous instance. With
                    // one instance — the overwhelming case — this is exact.
                    let axis = p
                        .instances
                        .first()
                        .map(|o| {
                            if o.geometry != instance.geometry {
                                Reason::Geometry
                            } else if o.src != instance.src {
                                Reason::Src
                            } else if o.transform != instance.transform {
                                Reason::Transform
                            } else if o.alpha != instance.alpha {
                                Reason::Alpha
                            } else if o.framebuffer_effect != instance.framebuffer_effect {
                                Reason::FramebufferEffect
                            } else {
                                Reason::ZIndex
                            }
                        })
                        .unwrap_or(Reason::New);
                    Some(axis)
                }
            } else {
                if let Some(clipped) = geometry.intersection(output) {
                    rects.push(clipped);
                }
                Some(Reason::New)
            };

            if let Some(reason) = reason {
                all_rects.extend(rects.iter().copied());
                let entry = per_element
                    .entry(id.clone())
                    .or_insert((reason, Vec::new()));
                entry.0 = entry.0.min(reason);
                entry.1.extend(rects);
            }

            now.entry(id)
                .and_modify(|s| s.instances.push(instance.clone()))
                .or_insert_with(|| ElementSnapshot {
                    commit: e.current_commit(),
                    instances: vec![instance],
                    kind,
                });
        }

        // Elements that were here last frame and are not now — including ones that merely became
        // hidden, which is the same thing to the tracker. It damages where they were.
        for (id, p) in &self.prev {
            if now.contains_key(id) {
                continue;
            }
            let rects: Vec<_> = p
                .instances
                .iter()
                .filter_map(|i| i.geometry.intersection(output))
                .collect();
            all_rects.extend(rects.iter().copied());
            per_element.insert(id.clone(), (Reason::Gone, rects));
        }

        let mut culprits: Vec<Culprit> = per_element
            .iter()
            .map(|(id, (reason, rects))| Culprit {
                kind: now
                    .get(id)
                    .or_else(|| self.prev.get(id))
                    .map(|s| s.kind.clone())
                    .unwrap_or_else(|| "?".to_owned()),
                reason: reason.as_str(),
                rects: rects.len(),
                share: union_share(rects, output_px),
            })
            .collect();
        culprits.sort_by(|a, b| {
            b.share
                .total_cmp(&a.share)
                .then_with(|| a.kind.cmp(&b.kind))
        });

        self.prev = now;

        FrameAttribution {
            shaded: shaded.len(),
            quiet,
            predicted: union_share(&all_rects, output_px),
            culprits,
        }
    }
}

/// Whether [`log_damage_attribution`] does anything — `SYNOIK_DEBUG_DAMAGE_ATTRIB`.
///
/// Its own knob rather than a mode of `SYNOIK_DEBUG_DAMAGE`, because the damage overlay is a
/// damage *participant*: it splices a variable number of tint elements into the scene, and a
/// varying count z-shifts every element below it. Sharing one switch would mean the attribution
/// could only ever be read with its own confound turned on.
pub fn damage_attribution_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("SYNOIK_DEBUG_DAMAGE_ATTRIB").is_some())
}

/// Log one output's damage attribution, but only when the story changes.
///
/// A settled scene repeats the same culprit set every frame; printing it 60 times a second would
/// push the rest of the journal out and cost more than the thing it measures. So a repeat is
/// counted, and the count is reported when the set finally moves — a suppressed frame is visible
/// as a number rather than absent.
pub fn log_damage_attribution<E>(
    output_name: &str,
    elements: &[E],
    scale: smithay::utils::Scale<f64>,
    output: smithay::utils::Rectangle<i32, smithay::utils::Physical>,
) where
    E: smithay::backend::renderer::element::Element + std::fmt::Debug,
{
    if !damage_attribution_enabled() {
        return;
    }
    static STATE: OnceLock<Mutex<HashMap<String, DamageAttribution>>> = OnceLock::new();
    let mut map = STATE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap();
    let state = map.entry(output_name.to_owned()).or_default();

    let attribution = state.frame(elements, scale, output);
    let signature = attribution.signature();
    if state.last_signature.as_deref() == Some(signature.as_str()) {
        state.suppressed += 1;
        return;
    }
    let held = std::mem::take(&mut state.suppressed);
    state.last_signature = Some(signature);
    let line = attribution.line();
    if held > 0 {
        tracing::info!("damage-attrib {output_name}: {line} (previous held {held} frames)");
    } else {
        tracing::info!("damage-attrib {output_name}: {line}");
    }
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
    /// `SYNOIK_FRAME_LOG_DUMP`, captured **once at construction** rather than read on every
    /// [`dump`](Self::dump).
    ///
    /// Reading it at dump time made the dump path a function of process-global state that any
    /// other test could be mutating: two tests here set and cleared this var, and the pair raced
    /// often enough to fail a full run roughly one time in six (more under
    /// `SYNOIK_VK_VALIDATION=1`, whose slowdown widens the window). Capturing it at construction
    /// makes each `FrameLog` carry its own answer, so a test builds one with the path it wants and
    /// touches no environment at all. Same reasoning as [`dump_dir_from`] below, which was split
    /// out after the same class of flake.
    dump_override: Option<std::path::PathBuf>,
    stats: HashMap<String, Stats>,
    /// Per output, the last frame handed to KMS: what it aimed at and when we let
    /// go of it. Read back in [`FrameLog::presented`] to turn "late" into "late
    /// even though we were done in time" or "late because we were late".
    queued: HashMap<String, (Duration, Duration)>,
    /// Per output, when the last frame reached the screen — the other end of a
    /// presentation interval. See [`Stats::cadence`].
    last_presented: HashMap<String, Duration>,
    last_summary: Instant,
    /// When the last automatic dump ran, for [`AUTODUMP_COOLDOWN`]. `None` = none yet.
    last_autodump: Option<Instant>,
    /// How many automatic dumps this session has written, for [`AUTODUMP_MAX`].
    autodumps: u64,
    /// Watches for time the compositor thread spent outside the frame path.
    loop_watch: LoopWatch,
    /// How punctually deadline-dispatched frames were released. Empty unless deadline
    /// dispatch is on, since nothing else has a scheduled start to be late against.
    lateness: DispatchLateness,
    /// Per-output tallies since the session started, never reset.
    ///
    /// [`Stats`] is cleared on every summary, which makes it useless for the question
    /// a person actually asks after a hitch — "has this been happening?". Answering
    /// that from the journal means finding and adding up every summary line since
    /// login. These are the same events, kept.
    lifetime: HashMap<String, Lifetime>,
}

/// Per-output tallies for the whole session, behind the perf IPC request.
///
/// Deliberately counters and a histogram rather than percentiles: a session-lifetime
/// percentile would mean retaining every frame's total for the life of the session,
/// and the tail is what matters anyway. The cadence histogram carries the shape.
#[derive(Debug, Default, Clone)]
struct Lifetime {
    frames: u64,
    over_budget: u64,
    worst: Duration,
    /// Presentations that landed at least one refresh cycle late.
    misses: u64,
    /// Cycles lost across all of them — a single 4-cycle miss and four 1-cycle
    /// misses are the same number here and very different experiences, which is why
    /// `worst_miss` is kept too.
    missed_cycles: u64,
    worst_miss: u64,
    /// Gap to the previous presentation, in whole refresh cycles. See [`Stats::cadence`].
    cadence: [u64; CADENCE_MAX + 1],
}

impl FrameLog {
    /// Read `SYNOIK_FRAME_LOG`. Anything unparseable is reported and ignored rather
    /// than failing the session — this is a debugging aid, and a typo in a
    /// session file should not cost you a desktop.
    pub fn from_env() -> Self {
        let settings = std::env::var("SYNOIK_FRAME_LOG")
            .ok()
            .and_then(|raw| Self::parse(&raw));

        ENABLED.store(settings.is_some(), Ordering::Relaxed);
        synoik_vk::stats::set_enabled(settings.is_some());

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
            // Said separately and explicitly, because this line is what delimits a
            // session in the journal and in a dump: whether the recorder was armed
            // decides what a later reader is entitled to conclude from silence.
            match (settings.ring, settings.autodump) {
                (Some(cap), Some(cycles)) => tracing::info!(
                    "frame ring on: {cap} records, auto-dumping at {cycles}+ missed cycles"
                ),
                (Some(cap), None) => {
                    tracing::info!("frame ring on: {cap} records, dump with SIGUSR1")
                }
                (None, _) => tracing::info!("frame ring off: nothing is being banked"),
            }
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
            dump_override: std::env::var_os("SYNOIK_FRAME_LOG_DUMP")
                .filter(|s| !s.is_empty())
                .map(std::path::PathBuf::from),
            stats: HashMap::new(),
            queued: HashMap::new(),
            last_presented: HashMap::new(),
            last_summary: Instant::now(),
            last_autodump: None,
            autodumps: 0,
            loop_watch: LoopWatch::default(),
            lateness: DispatchLateness::default(),
            lifetime: HashMap::new(),
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
                    Err(_) => tracing::warn!("SYNOIK_FRAME_LOG: bad ring size {v:?}, ignoring"),
                },
                // Implies `ring`, rather than silently doing nothing without it:
                // there is nothing to dump otherwise, and a no-op instrument reads
                // exactly like a session that never stuttered.
                ("autodump", None) => {
                    enabled = true;
                    settings.autodump = Some(AUTODUMP_DEFAULT_CYCLES);
                    settings.ring.get_or_insert(DEFAULT_RING);
                }
                ("autodump", Some(v)) => match v.parse::<u64>() {
                    Ok(0) => settings.autodump = None,
                    Ok(cycles) => {
                        enabled = true;
                        settings.autodump = Some(cycles);
                        settings.ring.get_or_insert(DEFAULT_RING);
                    }
                    Err(_) => {
                        tracing::warn!("SYNOIK_FRAME_LOG: bad autodump threshold {v:?}, ignoring")
                    }
                },
                ("summary", Some(v)) => match v.parse::<u64>() {
                    Ok(0) => settings.summary_every = None,
                    Ok(secs) => settings.summary_every = Some(Duration::from_secs(secs)),
                    Err(_) => {
                        tracing::warn!("SYNOIK_FRAME_LOG: bad summary period {v:?}, ignoring")
                    }
                },
                _ => match key.trim_end_matches("ms").parse::<f64>() {
                    Ok(ms) if ms > 0. => {
                        enabled = true;
                        settings.threshold = Some(Duration::from_secs_f64(ms / 1000.));
                    }
                    _ => tracing::warn!("SYNOIK_FRAME_LOG: unknown option {part:?}, ignoring"),
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
        // frame starts. See `synoik_vk::stats::begin_frame`.
        synoik_vk::stats::begin_frame();

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
            cpu_started: thread_cpu(),
            phase_started: now,
            phase_attributed: synoik_vk::stats::attributed(),
            phase: None,
            spans: Vec::with_capacity(Phase::ALL.len()),
            bakes_at_start: bakes(),
            shapes_at_start: synoik_vk::stats::shapes(),
            submits_at_start: synoik_vk::stats::submits(),
            draws_at_start: synoik_vk::stats::draws(),
            shaded_at_start: synoik_vk::stats::shaded(),
            shaded_by_site_at_start: synoik_vk::stats::shaded_by_site(),
            blitted_by_site_at_start: synoik_vk::stats::blitted_by_site(),
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
        let attributed = synoik_vk::stats::attributed();
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

        // Charge this frame's CPU to the frame, so the loop watch does not report it
        // as unexplained work outside the frame path. Accumulated rather than
        // assigned: one loop iteration can redraw several outputs.
        self.loop_watch.redraw_cpu += thread_cpu().saturating_sub(frame.cpu_started);

        let now = Instant::now();
        if let Some(last) = frame.phase.take() {
            frame.spans.push(Span {
                phase: last,
                wall: now - frame.phase_started,
                attributed: synoik_vk::stats::attributed().saturating_sub(frame.phase_attributed),
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
            shapes: synoik_vk::stats::shapes() - frame.shapes_at_start,
            shaping: synoik_vk::stats::take_shape_time(),
            submits: synoik_vk::stats::submits() - frame.submits_at_start,
            submitting: synoik_vk::stats::take_submit_time(),
            retiring: synoik_vk::stats::take_retire_time(),
            sites: synoik_vk::stats::take_sites(),
            first_wait: synoik_vk::stats::take_first_wait(),
            uploaded: synoik_vk::stats::take_uploaded_bytes(),
            creates: synoik_vk::stats::take_creates(),
            host_calls: synoik_vk::stats::take_host_calls(),
            render_passes: synoik_vk::stats::take_render_passes(),
            blitted_by_site: {
                let now = synoik_vk::stats::blitted_by_site();
                let mut d = [0u64; synoik_vk::stats::BlitSite::ALL.len()];
                for (i, slot) in d.iter_mut().enumerate() {
                    *slot = now[i] - frame.blitted_by_site_at_start[i];
                }
                d
            },
            create_sites: synoik_vk::stats::take_create_sites(),
            staging_write: synoik_vk::stats::take_staging_write(),
            draws: synoik_vk::stats::draws() - frame.draws_at_start,
            shaded: synoik_vk::stats::shaded() - frame.shaded_at_start,
            shaded_by_site: {
                let now = synoik_vk::stats::shaded_by_site();
                let mut d = [0u64; synoik_vk::stats::DrawSite::ALL.len()];
                for (i, slot) in d.iter_mut().enumerate() {
                    *slot = now[i] - frame.shaded_by_site_at_start[i];
                }
                d
            },
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
        let life = self.lifetime.entry(frame.output.clone()).or_default();
        life.frames += 1;
        life.over_budget += u64::from(over);
        life.worst = life.worst.max(cost);

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
        let mut line = format!(
            "[{}] frame on {} took {}",
            wall_clock(frame.started),
            frame.output,
            ms(total)
        );
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
            let mut by_site: Vec<_> = synoik_vk::stats::SubmitSite::ALL
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
            let by_phase: Vec<_> = synoik_vk::stats::GpuPhase::ALL
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
            // Split by what the draws were *for*. A blur chain's share is a fixed multiple of its
            // intermediate's area and moves only with the pass count or the intermediate size; the
            // scene's is the compositor's own layering. Without this the coverage figure names no
            // lever. Sites contributing under a tenth of the output are left out — the line is
            // already long, and a 0.0x share is not a lever either.
            let shares: Vec<String> = synoik_vk::stats::DrawSite::ALL
                .iter()
                .filter(|site| totals.shaded_by_site[site.index()] * 10 > ctx.output_px)
                .map(|site| {
                    format!(
                        "{} {:.1}x",
                        site.label(),
                        totals.shaded_by_site[site.index()] as f64 / ctx.output_px as f64
                    )
                })
                .collect();
            if !shares.is_empty() {
                let _ = write!(line, " [{}]", shares.join(", "));
            }
        }
        // Blits, beside the draws, because they are GPU work inside the same timestamp bracket
        // that `covering` structurally cannot describe — a frame whose GPU time exceeds what its
        // coverage explains is asking exactly this question.
        if ctx.output_px > 0 {
            let blitted: u64 = totals.blitted_by_site.iter().sum();
            if blitted * 10 > ctx.output_px {
                let shares: Vec<String> = synoik_vk::stats::BlitSite::ALL
                    .iter()
                    .filter(|site| totals.blitted_by_site[site.index()] * 10 > ctx.output_px)
                    .map(|site| {
                        format!(
                            "{} {:.1}x",
                            site.label(),
                            totals.blitted_by_site[site.index()] as f64 / ctx.output_px as f64
                        )
                    })
                    .collect();
                let _ = write!(
                    line,
                    ", blitting {:.1}x the output",
                    blitted as f64 / ctx.output_px as f64
                );
                if !shares.is_empty() {
                    let _ = write!(line, " [{}]", shares.join(", "));
                }
            }
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
            let mut by_site: Vec<_> = synoik_vk::stats::SubmitSite::ALL
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
                // Name them, capped exactly like the bake sites above and for the same reason.
                if let Some((shown, rest)) = split_at_most(&totals.create_sites, CREATE_SITES_SHOWN)
                {
                    let names: Vec<String> = shown.iter().map(ToString::to_string).collect();
                    let _ = write!(line, " ({}", names.join(", "));
                    if rest > 0 {
                        let _ = write!(line, ", +{rest} more");
                    }
                    let _ = write!(line, ")");
                }
            }
            // Every frame, not only notable ones: this is a number you read by *comparing* a slow
            // frame against the steady one next to it, and a field that appears only when it is
            // large cannot be compared against its own absence.
            let (barriers, writes, allocs) = totals.host_calls;
            if barriers | writes | allocs > 0 {
                let _ = write!(line, ", host {barriers}b/{writes}w/{allocs}a");
            }
            if totals.render_passes > 0 {
                let _ = write!(line, ", {} passes", totals.render_passes);
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
        if !ctx.animating.is_empty() {
            // Naming the animations is the whole point: "animating" alone could not
            // distinguish a workspace switch from a button's fill fade, so a stutter
            // report had nothing to join against.
            let _ = write!(line, ", animating {}", ctx.animating.names().join("+"));
        }
        if let Some(state) = ctx.overview_state {
            let _ = write!(line, ", overview {state:.2}");
        }
        if let Some(state) = ctx.peek_state {
            let _ = write!(line, ", peek {state:.2}");
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
        // See `docs/fork/foundation.md` §3.
        let prev = self.last_presented.insert(output.to_owned(), actual);
        let since_last_flip = prev
            .and_then(|prev| actual.checked_sub(prev))
            .map(|gap| (gap.as_secs_f64() / refresh.as_secs_f64()).round() as usize);
        if let Some(cycles) = since_last_flip {
            self.stats.entry(output.to_owned()).or_default().cadence[cycles.min(CADENCE_MAX)] += 1;
            self.lifetime.entry(output.to_owned()).or_default().cadence[cycles.min(CADENCE_MAX)] +=
                1;
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
        // this minus one. See `docs/fork/foundation.md` §3.
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
        let life = self.lifetime.entry(output.to_owned()).or_default();
        life.missed_cycles += missed;
        life.misses += 1;
        life.worst_miss = life.worst_miss.max(missed);

        let queued = match headroom {
            Some(us) if us >= 0 => format!(", queued {} early", ms_us(us)),
            Some(us) => format!(", queued {} LATE", ms_us(-us)),
            None => String::new(),
        };

        let cadence = cadence_clause(since_last_flip);
        let aim = aim_clause(aimed_after);

        let line = format!(
            "missed {missed} vblank(s) on {output}: presented {} late, \
             refresh {}{queued}{cadence}{aim}",
            ms(late),
            ms(refresh),
        );

        // Bank it as well as warn it. A miss line is the ONLY record of a miss —
        // `correlate-frame-log.py` derives the whole rate by counting these — so a
        // dump without them is not a quieter measurement, it is a measurement that
        // reads as a flawless session no matter what happened. That is exactly how
        // a ring dump came to score 0.00% against the same workload's 13.99%, and
        // the conclusion drawn from it (that the frame log was causing the misses)
        // was wrong. Summaries are banked for the same reason; misses were missed.
        //
        // Formatting here is not the tail-selective cost that over-budget frames
        // were: this fires per *miss*, which is a property of the display, not of
        // how expensive the frame was to build.
        if self.settings.and_then(|s| s.ring).is_some() {
            let cap = self.settings.and_then(|s| s.ring).expect("just checked");
            while self.ring.len() >= cap {
                self.ring.pop_front();
            }
            self.ring.push_back(Entry::Line(line.clone()));
        }
        tracing::warn!("{line}");

        self.maybe_autodump(settings, missed, &line);
    }

    /// Close out one turn of the event loop, reporting any time the compositor thread
    /// spent outside the frame path.
    ///
    /// Call at the **end** of the post-dispatch callback, which is once per turn of
    /// `calloop`'s loop. `redraw_pending` is whether any output has a redraw queued
    /// *now* — it is stashed for the next window, because the question a stall asks
    /// is whether a frame was owed while the thread was busy or blocked.
    ///
    /// See [`LoopWatch`] for why this exists at all: it is the one instrument here
    /// that can see a stutter which produced no slow frame.
    pub fn loop_turn_end(&mut self, redraw_pending: bool) {
        if self.settings.is_none() {
            return;
        }

        let now = Instant::now();
        let cpu = thread_cpu();

        if let Some((last_wall, last_cpu)) = self.loop_watch.last_end {
            let wall = now.saturating_duration_since(last_wall);
            let window_cpu = cpu.saturating_sub(last_cpu);
            // Work that was not the frame. Redraw runs inside a source callback, so
            // it lands in this window, and the frame log already reports it in far
            // more detail than a stall line could.
            let other_cpu = window_cpu.saturating_sub(self.loop_watch.redraw_cpu);
            // Time the thread was not running at all: poll, or a handler blocked in a
            // syscall. Only the second is a bug, and `redraw_was_pending` is what
            // tells them apart.
            let blocked = wall.saturating_sub(window_cpu);

            if let Some(why) = stall_verdict(other_cpu, blocked, self.loop_watch.redraw_was_pending)
            {
                self.loop_watch.stalls += 1;
                let line = format!(
                    "main loop {why}: {} outside the frame path ({} of CPU, {} not running, \
                     frame {} of CPU), turn took {}",
                    ms(other_cpu + blocked),
                    ms(other_cpu),
                    ms(blocked),
                    ms(self.loop_watch.redraw_cpu),
                    ms(wall),
                );

                // Banked as well as warned, for the same reason miss lines are: an
                // event missing from a dump reads as a session in which it never
                // happened. This is the line that explains a clean frame log.
                if let Some(cap) = self.settings.and_then(|s| s.ring) {
                    while self.ring.len() >= cap {
                        self.ring.pop_front();
                    }
                    self.ring.push_back(Entry::Line(line.clone()));
                }

                // Rate-limited because a pathological stretch stalls every turn, and
                // formatting a journal line costs the very thread that is behind.
                // The ring entry above is not rate-limited — it is nearly free, and
                // thinning it would put holes in the record.
                let quiet = self
                    .loop_watch
                    .last_warned
                    .is_some_and(|last| now.duration_since(last) < STALL_COOLDOWN);
                if !quiet {
                    self.loop_watch.last_warned = Some(now);
                    tracing::warn!("{line}");
                }
            }
        }

        self.loop_watch.last_end = Some((now, cpu));
        self.loop_watch.redraw_was_pending = redraw_pending;
        self.loop_watch.redraw_cpu = Duration::ZERO;
    }

    /// How many main-loop stalls this session has seen.
    pub fn stalls(&self) -> u64 {
        self.loop_watch.stalls
    }

    /// Everything the session has tallied, for the perf IPC request.
    ///
    /// The point of exposing this at all: a one-off hitch is over before anyone can
    /// arm an instrument, and until now the only way to ask "has this session been
    /// stuttering" was to find every summary line in the journal since login and add
    /// them up. This answers it in one call, days later, from the running process.
    /// Record how late a deadline-dispatched frame was released, against the moment the
    /// deadline was armed for.
    ///
    /// This is the number that decides what the deadline margin is really paying for.
    /// Release lateness in the tens of microseconds is a punctual timer, and a margin
    /// wider than that is buying something else — loop occupancy, or a vCPU that was not
    /// running. Milliseconds here would mean the wakeup itself is the cost.
    pub fn record_dispatch_lateness(&mut self, lateness: Duration) {
        self.lateness.record(lateness);
    }

    pub fn perf_snapshot(&self) -> synoik_ipc::FramePerf {
        synoik_ipc::FramePerf {
            enabled: self.is_enabled(),
            ring_capacity: self.settings.and_then(|s| s.ring).unwrap_or(0),
            ring_len: self.ring.len(),
            autodump_cycles: self.settings.and_then(|s| s.autodump),
            autodumps: self.autodumps,
            dumps: self.dumps,
            stalls: self.loop_watch.stalls,
            held_frames: self.lateness.count,
            lateness_mean_ms: if self.lateness.count == 0 {
                0.
            } else {
                self.lateness.total.as_secs_f64() * 1000. / self.lateness.count as f64
            },
            lateness_worst_ms: self.lateness.worst.as_secs_f64() * 1000.,
            lateness_buckets: self.lateness.buckets.to_vec(),
            lateness_edges_us: LATENESS_EDGES_US.to_vec(),
            deadline_dispatch: crate::frame_clock::deadline_dispatch_enabled(),
            deadline_margin_ms: crate::frame_clock::render_time_margin_now().as_secs_f64() * 1000.,
            outputs: self
                .lifetime
                .iter()
                .map(|(name, life)| synoik_ipc::FramePerfOutput {
                    output: name.clone(),
                    frames: life.frames,
                    over_budget: life.over_budget,
                    worst_ms: life.worst.as_secs_f64() * 1000.,
                    misses: life.misses,
                    missed_cycles: life.missed_cycles,
                    worst_miss_cycles: life.worst_miss,
                    cadence: life.cadence.to_vec(),
                })
                .collect(),
        }
    }

    /// Write the run-up to a bad miss to a file by itself, so a stutter nobody was
    /// ready for still leaves evidence.
    ///
    /// Fires *after* the miss line is banked, so the dump ends with the miss that
    /// caused it — the last line of the file names the event, and everything above it
    /// is the run-up.
    fn maybe_autodump(&mut self, settings: Settings, missed: u64, cause: &str) {
        let Some(threshold) = settings.autodump else {
            return;
        };
        if missed < threshold {
            return;
        }
        if self.autodumps >= AUTODUMP_MAX {
            return;
        }
        let now = Instant::now();
        if self
            .last_autodump
            .is_some_and(|last| now.duration_since(last) < AUTODUMP_COOLDOWN)
        {
            return;
        }

        // Set before the write, not after: a dump that fails still consumed the
        // cooldown. Otherwise a persistent failure (a full disk, a read-only state
        // directory) retries on every single miss, which turns one broken instrument
        // into a per-frame syscall storm on the compositor thread.
        self.last_autodump = Some(now);
        self.autodumps += 1;

        match self.dump_tail(AUTODUMP_TAIL) {
            Ok((path, entries)) => tracing::warn!(
                "frame log: auto-dumped {entries} entries to {} ({cause})",
                path.display()
            ),
            Err(err) => tracing::warn!("frame log: auto-dump failed: {err}"),
        }
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
                 writer's buffer overflowed, so this log has holes in it. Set SYNOIK_LOG_BLOCKING=1 \
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
        let path = dump_path(self.dump_override.clone(), self.dumps);
        if self.ring.is_empty() {
            // No file at all, rather than an empty one. A 0-byte dump beside the others reads as
            // "the window was captured and there was nothing in it" — which is the opposite of
            // what it means, and indistinguishable at a glance from a session that rendered
            // nothing. The caller says "the ring is empty" instead. Reachable whenever the
            // recorder is off: the signal handler is installed regardless.
            return Ok((path, 0));
        }
        let entries = self.write_ring(&path, self.ring.len())?;

        self.ring.clear();
        self.dumps += 1;
        Ok((path, entries))
    }

    /// Write the newest `keep` banked entries **without clearing the ring**, for the
    /// automatic dump ([`Settings::autodump`]).
    ///
    /// Both halves of that are deliberate. *Newest only*, because the point of an
    /// automatic dump is the run-up to one bad moment, and writing the whole
    /// ~22-minute ring would be tens of MB of synchronous I/O on the compositor
    /// thread — a hitch caused by the instrument, fired precisely when the session
    /// is already struggling. *Without clearing*, because otherwise the automatic
    /// dump silently destroys the window a later `SIGUSR1` would have collected, and
    /// two triggers in a row would eat each other's evidence.
    fn dump_tail(&mut self, keep: usize) -> std::io::Result<(std::path::PathBuf, usize)> {
        let path = dump_path(self.dump_override.clone(), self.dumps);
        // Same as [`dump`]: nothing banked, no file.
        if self.ring.is_empty() {
            return Ok((path, 0));
        }
        let entries = self.write_ring(&path, keep)?;
        self.dumps += 1;
        Ok((path, entries))
    }

    /// Format and write the newest `keep` banked entries, oldest first.
    ///
    /// A file rather than the journal on purpose: a dump is tens of thousands of
    /// lines at once, and journald's rate limiter would *drop* some of them, which
    /// is the one failure this whole mechanism cannot tolerate — a log with
    /// invisible holes reads exactly like a log of a session that rendered fewer
    /// frames. The text is byte-identical to the journal's, so
    /// `scripts/correlate-frame-log.py` reads a dump directly.
    fn write_ring(&self, path: &std::path::Path, keep: usize) -> std::io::Result<usize> {
        use std::io::Write as _;

        let skip = self.ring.len().saturating_sub(keep);
        let entries = self.ring.len() - skip;

        // Write beside the target and rename into place. A dump is tens of MB onto a
        // tmpfs, so a mid-write ENOSPC is a real outcome — and a half-written file at
        // the expected path is indistinguishable from a complete dump of a shorter
        // window, which is precisely the invisible-hole failure this mechanism cannot
        // tolerate. `rename` is atomic, so the reader sees the whole file or none.
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("txt.partial");
        {
            let file = std::fs::File::create(&tmp)?;
            let mut out = std::io::BufWriter::new(file);
            // By reference, not `drain`: dropping a `Drain` discards the whole drained
            // range, so an error partway through used to destroy the entries it had
            // not written yet. The caller clears the ring afterwards, once the bytes
            // are safe.
            for entry in self.ring.iter().skip(skip) {
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
        std::fs::rename(&tmp, path)?;

        Ok(entries)
    }
}

/// Where [`FrameLog::dump`] writes, given the `SYNOIK_FRAME_LOG_DUMP` override the log captured at
/// construction ([`FrameLog::dump_override`]); otherwise a numbered file under [`dump_dir`].
///
/// The override is a parameter rather than an env read for a reason — see that field.
fn dump_path(over: Option<std::path::PathBuf>, nth: u64) -> std::path::PathBuf {
    // An explicit path is taken verbatim, including on a second dump: the caller
    // asked for that name. Everything else is numbered, because the natural way to
    // scope a measurement is dump-to-clear, run, dump-to-collect — and with one
    // fixed name per PID the second signal silently destroys the first window.
    if let Some(path) = over {
        return path;
    }
    std::path::PathBuf::from(dump_dir()).join(format!("frame-log.{}.{nth}.txt", std::process::id()))
}

/// Where dumps live by default: `$XDG_STATE_HOME/synoik`, i.e. `~/.local/state/synoik`.
///
/// **Not `$XDG_RUNTIME_DIR`**, which is where this used to write. That directory is a
/// tmpfs by design — it is for sockets and pid files, things that *should* die with the
/// boot — and every earlier arm of this investigation tolerated that only because the
/// frame log also wrote each line to journald, which is persistent. `ring` mode's whole
/// purpose is that it does not, so the runtime dir was the one place a measurement must
/// not go: a reboot between taking a run and reading it destroyed it silently. State is
/// the XDG category for exactly this — survives restarts, not precious enough to be data.
pub(crate) fn dump_dir() -> std::ffi::OsString {
    let var = |k| std::env::var_os(k).filter(|s: &std::ffi::OsString| !s.is_empty());
    dump_dir_from(var("XDG_STATE_HOME"), var("HOME"), var("XDG_RUNTIME_DIR"))
}

/// The choice itself, with the environment passed in.
///
/// Split out so it can be tested without `set_var`. That is not fastidiousness: these
/// tests run in one process alongside everything else, and the first version of the test
/// cleared `SYNOIK_FRAME_LOG_DUMP` and broke a *different* test that was mid-dump. Env
/// mutation in a parallel test binary is a flake generator, and this file already carries
/// scars from the same class in its counters.
fn dump_dir_from(
    state: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
    runtime: Option<std::ffi::OsString>,
) -> std::ffi::OsString {
    if let Some(state) = state {
        let mut path = std::path::PathBuf::from(state);
        path.push("synoik");
        return path.into_os_string();
    }
    if let Some(home) = home {
        let mut path = std::path::PathBuf::from(home);
        path.extend([".local", "state", "synoik"]);
        return path.into_os_string();
    }
    // No home to write to (a system service, a broken environment). The runtime dir is
    // still better than the cwd, which for a compositor is wherever it was launched.
    runtime.unwrap_or_else(|| "/tmp".into())
}

/// How many bake sites a frame line names before it starts counting the rest.
const BAKE_SITES_SHOWN: usize = 3;

/// The same, for creation sites. There are fewer distinct constructors than widgets, so three
/// names covers a frame that is allocating for more than one reason.
const CREATE_SITES_SHOWN: usize = 3;

/// How long the display had been quiet in front of a missed flip, in whole refresh cycles.
///
/// Its own function so a test can pin the wording without going through a page flip. The
/// distinction it draws is the one the live data says matters: a flip that immediately follows
/// another misses ~1% of the time on this stack, and one with even a single idle cycle in front of
/// it misses 26–47% — flat from 2 cycles out to 5 seconds. See `docs/fork/foundation.md` §3.
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
/// cycles in front. See `docs/fork/foundation.md` §3.
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

/// Ties the ring's monotonic [`Instant`]s to wall-clock time.
///
/// Sampled once and never again: the two clocks are read together, and every banked record is
/// placed by its offset from that pair. So a stamp costs nothing per frame — the work happens at
/// dump time, where a record already pays a `format!` — and the stamps stay mutually consistent
/// even if the system clock is stepped mid-session (they drift from the wall together rather than
/// jumping apart from each other).
fn wall_anchor() -> (Instant, std::time::SystemTime) {
    static ANCHOR: std::sync::OnceLock<(Instant, std::time::SystemTime)> =
        std::sync::OnceLock::new();
    *ANCHOR.get_or_init(|| (Instant::now(), std::time::SystemTime::now()))
}

/// Local wall-clock time of a banked record as `HH:MM:SS.mmm`, so a dump can be lined up against
/// the journal, a screen recording, or a user saying when they saw something.
///
/// The ring holds raw records and formats them only when a dump is written, so this reads
/// [`InFlight::started`] rather than sampling a clock on the frame path. Unrepresentable times
/// render as dashes: a stamp that cannot be trusted must not look like one that can.
fn wall_clock(at: Instant) -> String {
    const UNKNOWN: &str = "--:--:--.---";

    let (anchor_at, anchor_wall) = wall_anchor();
    let wall = if at >= anchor_at {
        anchor_wall.checked_add(at - anchor_at)
    } else {
        anchor_wall.checked_sub(anchor_at - at)
    };
    let Some(wall) = wall else {
        return UNKNOWN.to_owned();
    };
    let Ok(since_epoch) = wall.duration_since(std::time::UNIX_EPOCH) else {
        return UNKNOWN.to_owned();
    };
    let Ok(secs) = i64::try_from(since_epoch.as_secs()) else {
        return UNKNOWN.to_owned();
    };
    let Ok(ts) = jiff::Timestamp::from_second(secs) else {
        return UNKNOWN.to_owned();
    };
    format!(
        "{}.{:03}",
        ts.to_zoned(jiff::tz::TimeZone::system())
            .strftime("%H:%M:%S"),
        since_epoch.subsec_millis()
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use smithay::utils::{Point, Rectangle, Size};

    use super::Instance;

    fn at(x: i32, y: i32) -> Instance {
        Instance {
            geometry: Rectangle::new(Point::from((x, y)), Size::from((100, 100))),
            alpha: 1.,
            z_index: 0,
        }
    }

    /// The rule the seat's log and the corpus both run on, in all four of its cases — including
    /// the ones it must stay quiet for, because a report that fires on a healthy frame is a report
    /// nobody reads, and this one exists to be read once, during a reproduction.
    #[test]
    fn only_a_departure_past_a_sibling_that_stayed_is_reported() {
        let id = String::from("thumbnail");
        let one = |v: Vec<Instance>| HashMap::from([(id.clone(), v)]);

        let screen = Rectangle::new(Point::from((0, 0)), Size::from((800, 600)));
        let shrinks = |was: Vec<Instance>, is: HashMap<String, Vec<Instance>>| {
            super::instance_shrinks(&one(was), &is, screen)
        };

        // A departure past a sibling that stayed: the tracker heals nothing.
        assert_eq!(
            shrinks(vec![at(0, 0), at(400, 0)], one(vec![at(0, 0)])),
            ["thumbnail 2 -> 1 vacated=[100x100+400+0]"]
        );

        // A move: the arriving instance matches nothing, so it damages every remembered one.
        assert!(shrinks(vec![at(0, 0), at(400, 0)], one(vec![at(0, 0), at(430, 0)])).is_empty());

        // A departure where the survivor also moved: same branch, same healing.
        assert!(shrinks(vec![at(0, 0), at(400, 0)], one(vec![at(30, 0)])).is_empty());

        // A departure in a frame where *another* instance of the id moved. That one instance is
        // enough: its branch damages the whole remembered list, departed rects included. Four
        // fifths of the seat's first report was this, which is why the rule asks that *every*
        // survivor matched.
        assert!(shrinks(
            vec![at(0, 0), at(200, 0), at(400, 0)],
            one(vec![at(0, 0), at(230, 0)])
        )
        .is_empty());

        // A departure from off the output, which the tracker clips away and never owed.
        assert!(shrinks(vec![at(0, 0), at(900, 0)], one(vec![at(0, 0)])).is_empty());

        // The id gone altogether, which `elements_gone` damages in full.
        assert!(shrinks(vec![at(0, 0), at(400, 0)], HashMap::new()).is_empty());
    }

    /// The damage log is read by eye out of a journal, so its shape is part of it: an empty age
    /// says `none` rather than an empty bracket that reads as a truncated line, and a long list
    /// says how much it dropped.
    #[test]
    fn the_damage_line_says_what_it_left_out() {
        use smithay::utils::{Physical, Point, Rectangle, Size};

        let rect =
            |x, y, w, h| Rectangle::<i32, Physical>::new(Point::from((x, y)), Size::from((w, h)));
        let one = [rect(0, 32, 1920, 1)];
        assert_eq!(
            frame_damage_line("DP-1", "swapchain", 37, [(1, &one), (2, &[])]),
            "damage DP-1 plane=swapchain elements=37 age1=[1920x1+0+32] age2=[none]"
        );

        let many: Vec<_> = (0..15).map(|i| rect(i, 0, 10, 10)).collect();
        let line = frame_damage_line("DP-1", "direct-scanout", 1, [(1, &many), (2, &[])]);
        assert!(
            line.contains("(+3 more)"),
            "a long list must say how many it dropped: {line}"
        );
    }

    use synoik_vk::stats::SubmitSite;

    use super::*;

    /// A sample lands in the bucket for the first edge it is *under*, and the tail has
    /// somewhere to go — an overflowing sample silently dropped would make a loop that
    /// wakes up 20ms late look punctual.
    #[test]
    fn lateness_buckets_by_first_edge_above_it() {
        let mut l = DispatchLateness::default();
        l.record(Duration::from_micros(40)); // < 100
        l.record(Duration::from_micros(100)); // the edge itself is not "under" it
        l.record(Duration::from_micros(7_999)); // < 8000, the last edge
        l.record(Duration::from_millis(20)); // overflow

        assert_eq!(l.buckets[0], 1, "40us is under the first edge");
        assert_eq!(l.buckets[1], 1, "100us is not under 100us");
        assert_eq!(l.buckets[LATENESS_EDGES_US.len() - 1], 1);
        assert_eq!(l.buckets[LATENESS_EDGES_US.len()], 1, "the tail is kept");
        assert_eq!(l.count, 4);
        assert_eq!(l.worst, Duration::from_millis(20));
    }

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

    /// GPU timing is decided by reading `SYNOIK_FRAME_LOG` directly, not by
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
    ) -> [synoik_vk::stats::SiteTotals; SubmitSite::ALL.len()] {
        let mut out = [synoik_vk::stats::SiteTotals::default(); SubmitSite::ALL.len()];
        for (site, submits, retiring) in entries {
            let i = SubmitSite::ALL.iter().position(|s| s == site).unwrap();
            out[i] = synoik_vk::stats::SiteTotals {
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
            cpu_started: Duration::ZERO,
            phase_started: now,
            phase_attributed: Duration::ZERO,
            phase: None,
            spans: Vec::new(),
            bakes_at_start: 0,
            shapes_at_start: 0,
            submits_at_start: 0,
            draws_at_start: 0,
            shaded_at_start: 0,
            shaded_by_site_at_start: [0; synoik_vk::stats::DrawSite::ALL.len()],
            blitted_by_site_at_start: [0; synoik_vk::stats::BlitSite::ALL.len()],
            context: FrameContext::default(),
        }
    }

    /// A creating frame names *what* it created, not just how much.
    ///
    /// The count alone is a dead end: 15 creations a frame is equally consistent with a blur
    /// chain rebuilt because its size animates, a swapchain image per frame, and an upload that
    /// forgot its cache — three different bugs. The seat's overview transition sat at exactly
    /// that number with nowhere to take it until the line named the constructor.
    /// A pass boundary costs tile-memory traffic for the whole target and shades nothing, so it is
    /// invisible to `covering`, `blitting` and `host` alike. Two frames with the same draws and the
    /// same coverage can differ by it.
    #[test]
    fn a_frame_says_how_many_render_passes_it_opened() {
        let frame = empty_frame();
        let totals = Totals {
            submits: 1,
            render_passes: 9,
            ..Totals::default()
        };
        let line = FrameLog::format_frame(&frame, Duration::from_millis(17), &totals, None);
        assert!(line.contains("9 passes"), "{line}");
    }

    /// Blits are GPU work inside the frame's timestamp bracket that `covering` structurally cannot
    /// describe — they are not draws. A frame whose GPU time exceeds what its coverage explains is
    /// asking exactly this question, so the answer has to be on the same line.
    #[test]
    fn a_frame_says_how_much_it_blitted_beside_what_it_drew() {
        use synoik_vk::stats::BlitSite;

        let mut frame = empty_frame();
        frame.context.output_px = 1000;
        let mut blitted = [0u64; BlitSite::ALL.len()];
        blitted[BlitSite::Present.index()] = 1000;
        blitted[BlitSite::Capture.index()] = 500;
        let totals = Totals {
            submits: 1,
            blitted_by_site: blitted,
            ..Totals::default()
        };
        let line = FrameLog::format_frame(&frame, Duration::from_millis(17), &totals, None);
        assert!(
            line.contains("blitting 1.5x the output [present 1.0x, capture 0.5x]"),
            "{line}"
        );

        // Under a tenth of the output is not a lever, and the line is already long.
        let mut blitted = [0u64; BlitSite::ALL.len()];
        blitted[BlitSite::Present.index()] = 50;
        let totals = Totals {
            submits: 1,
            blitted_by_site: blitted,
            ..Totals::default()
        };
        let line = FrameLog::format_frame(&frame, Duration::from_millis(17), &totals, None);
        assert!(!line.contains("blitting"), "{line}");
    }

    /// Host calls ride every submitting frame, because the number is read by comparing a slow
    /// frame with the steady one beside it — a field that appears only when it is large cannot be
    /// compared against its own absence.
    #[test]
    fn a_frame_says_how_many_calls_it_made_the_host_forward() {
        let frame = empty_frame();
        let totals = Totals {
            submits: 1,
            host_calls: (37, 12, 4),
            ..Totals::default()
        };
        let line = FrameLog::format_frame(&frame, Duration::from_millis(17), &totals, None);
        assert!(line.contains("host 37b/12w/4a"), "{line}");

        // A frame that made none says nothing rather than printing zeros.
        let totals = Totals {
            submits: 1,
            ..Totals::default()
        };
        let line = FrameLog::format_frame(&frame, Duration::from_millis(17), &totals, None);
        assert!(!line.contains("host "), "{line}");
    }

    #[test]
    fn a_creating_frame_names_the_constructor() {
        use synoik_vk::stats::CreateSite;

        let frame = empty_frame();
        let site = |file, line, creates, micros| CreateSite {
            file,
            line,
            creates,
            time: Duration::from_micros(micros),
        };
        let totals = Totals {
            // The creation clause rides the submit breakdown, so the frame needs a submit.
            submits: 1,
            creates: (19, Duration::from_micros(4300)),
            create_sites: vec![
                site("synoik-vk/src/texture.rs", 496, 15, 3000),
                site("synoik-vk/src/blur.rs", 224, 3, 1000),
                site("synoik-vk/src/staging.rs", 211, 1, 300),
            ],
            ..Totals::default()
        };
        let line = FrameLog::format_frame(&frame, Duration::from_millis(17), &totals, None);
        assert!(
            line.contains(
                "19 created in 4.30ms (texture.rs:496 ×15 3.00ms, blur.rs:224 ×3 1.00ms, \
                 staging.rs:211 ×1 0.30ms)"
            ),
            "{line}"
        );

        // Capped like the bake sites, and the drop is stated rather than silent: a line that
        // quietly showed three of five would read as a frame with three creators.
        let totals = Totals {
            submits: 1,
            creates: (4, Duration::from_micros(400)),
            create_sites: vec![
                site("synoik-vk/src/texture.rs", 496, 1, 400),
                site("synoik-vk/src/texture.rs", 590, 1, 300),
                site("synoik-vk/src/blur.rs", 224, 1, 200),
                site("synoik-vk/src/staging.rs", 211, 1, 100),
            ],
            ..Totals::default()
        };
        let line = FrameLog::format_frame(&frame, Duration::from_millis(17), &totals, None);
        assert!(line.contains("+1 more)"), "{line}");
    }

    /// Enqueueing work and waiting for it are reported apart, and a wait is never
    /// dropped from the line just because this frame did not submit it.
    ///
    /// This is the property that lets the synchronous-submit work be measured at
    /// all (`docs/fork/foundation.md`). With one number, handing
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
            // No env read, and none of these tests may set one: `SYNOIK_FRAME_LOG_DUMP` is
            // process-global, and mutating it here raced the other dump test.
            dump_override: None,
            settings: Some(Settings::default()),
            in_flight: None,
            stats: HashMap::new(),
            queued: HashMap::new(),
            last_presented: HashMap::new(),
            last_summary: Instant::now(),
            last_autodump: None,
            autodumps: 0,
            loop_watch: LoopWatch::default(),
            lateness: DispatchLateness::default(),
            lifetime: HashMap::new(),
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
            synoik_vk::stats::SubmitSite::KmsFrame,
            Duration::from_millis(5),
        );
        add_gpu_time(
            9,
            synoik_vk::stats::SubmitSite::KmsFrame,
            Duration::from_millis(2),
        );
        add_gpu_lost(7, synoik_vk::stats::SubmitSite::KmsFrame);

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
        use synoik_vk::stats::SubmitSite;
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
        let dir = std::env::temp_dir().join(format!("synoik-ring-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dump.txt");

        let mut log = test_log();
        // Per-log, not per-process: this is the whole point of capturing the override at
        // construction. Setting the env var here used to race the ENOSPC test below.
        log.dump_override = Some(path.clone());
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
            text.lines().all(|l| {
                let (stamp, rest) = l.split_once("] ").unwrap_or(("", l));
                // `[HH:MM:SS.mmm`, so a dump lines up against the journal, a screen recording, or
                // a user saying when they saw something.
                stamp.len() == "[00:00:00.000".len()
                    && rest.starts_with("frame on out")
                    && rest.contains(" took ")
            }),
            "every dumped frame carries a wall-clock stamp, and the rest of the line stays \
             byte-identical to the journal's so correlate-frame-log.py reads a dump \
             directly: {text}"
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
        // A path whose parent is a regular *file*, so it fails at the directory
        // create — a merely missing directory is no longer an error, since the
        // state dir legitimately does not exist before the first dump.
        let blocker = dir.join("not-a-directory");
        std::fs::write(&blocker, b"").unwrap();
        log.dump_override = Some(blocker.join("dump.txt"));
        assert!(
            log.dump().is_err(),
            "a dump that cannot create its directory must fail"
        );
        assert_eq!(
            log.ring.len(),
            1,
            "a failed dump must not consume the entries it could not write"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `autodump` must turn the ring on by itself, and must not trip on the routine
    /// single-cycle miss.
    ///
    /// Both halves are correctness, not ergonomics. Without the implication,
    /// `SYNOIK_FRAME_LOG=autodump` is a silent no-op — there is no ring to dump — and
    /// a session that never writes a file is indistinguishable from a session that
    /// never stuttered. And a default of 1 would fire on the ~12% of presentations
    /// that land one cycle late on this stack (`docs/fork/foundation.md`), i.e.
    /// continuously, which is the same as not having a trigger.
    #[test]
    fn autodump_implies_ring_and_ignores_the_routine_miss() {
        let s = FrameLog::parse("autodump").expect("autodump alone must enable logging");
        assert_eq!(s.autodump, Some(AUTODUMP_DEFAULT_CYCLES));
        assert!(
            s.ring.is_some(),
            "autodump without a ring has nothing to dump"
        );
        const {
            assert!(
                AUTODUMP_DEFAULT_CYCLES >= 2,
                "a 1-cycle trigger fires on routine misses and is no trigger at all"
            )
        };

        let s = FrameLog::parse("ring=16,autodump=3").expect("enabled");
        assert_eq!(s.autodump, Some(3));
        assert_eq!(s.ring, Some(16), "an explicit ring size must survive");

        let s = FrameLog::parse("ring,autodump=0").expect("ring still enables logging");
        assert_eq!(s.autodump, None, "autodump=0 must turn it off");
    }

    /// An automatic dump writes the run-up to a bad miss *without* consuming the
    /// ring, and rate-limits itself.
    ///
    /// Keeping the ring is the load-bearing part. If the automatic dump cleared it
    /// the way `SIGUSR1` does, the instrument would quietly destroy the evidence it
    /// exists to preserve: the first hitch of a bad stretch would take the window
    /// with it, and a person who then reached for `SIGUSR1` would collect the
    /// handful of frames since — reading as a healthy session.
    #[test]
    fn an_automatic_dump_keeps_the_ring_and_rate_limits() {
        let dir = std::env::temp_dir().join(format!("synoik-autodump-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auto.txt");

        let mut log = test_log();
        log.dump_override = Some(path.clone());
        log.settings = Some(Settings {
            ring: Some(64),
            autodump: Some(2),
            summary_every: None,
            ..Settings::default()
        });

        for _ in 0..5 {
            log.begin("out");
            log.end(Some(Duration::from_millis(16)));
        }
        let banked = log.ring.len();
        assert_eq!(banked, 5, "frames must be banked before the miss");

        // One cycle late: under the threshold, so nothing is written.
        log.presented(
            "out",
            Duration::from_millis(100),
            Duration::from_millis(116),
            Some(Duration::from_millis(16)),
        );
        assert_eq!(log.autodumps, 0, "a 1-cycle miss must not trigger a dump");

        // Two cycles late: over the threshold.
        log.presented(
            "out",
            Duration::from_millis(200),
            Duration::from_millis(233),
            Some(Duration::from_millis(16)),
        );
        assert_eq!(log.autodumps, 1, "a 2-cycle miss must trigger a dump");
        assert!(
            log.ring.len() > banked,
            "the automatic dump must NOT clear the ring — it would destroy the \
             window a later SIGUSR1 would collect"
        );

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.lines()
                .last()
                .is_some_and(|l| l.starts_with("missed ")),
            "the dump must end with the miss that caused it, so the file names \
             its own trigger: {text}"
        );

        // A bad stretch misses many times over; each dump is synchronous I/O on the
        // compositor thread, so the second one inside the cooldown must be skipped.
        log.presented(
            "out",
            Duration::from_millis(300),
            Duration::from_millis(333),
            Some(Duration::from_millis(16)),
        );
        assert_eq!(
            log.autodumps, 1,
            "a second miss inside the cooldown must not dump again"
        );

        // And the manual dump still has the whole window, including both misses.
        let (_, n) = log.dump().expect("dump");
        assert_eq!(
            n, 8,
            "SIGUSR1 must still collect everything the automatic dump left alone: \
             5 frames plus 3 miss lines"
        );
        assert!(
            log.ring.is_empty(),
            "the manual dump still empties the ring"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The loop watch must separate a blocked thread from an idle one, and must not
    /// charge the frame's own cost to the loop.
    ///
    /// Three failure modes, each of which would make the instrument worse than
    /// nothing:
    ///
    /// 1. Reporting idle polls. A desktop sits in poll for seconds at a time; a stall line per idle
    ///    second buries the one that matters.
    /// 2. Reporting the frame. Redraw runs inside a source callback, so its CPU lands in the loop's
    ///    window. Left in, every animating frame would be a "stall", and the number would say
    ///    nothing the frame log does not say better.
    /// 3. Missing the blocked case. A handler waiting on a D-Bus round trip burns no CPU, so a
    ///    CPU-only test sees a healthy loop — and that is the class this exists to catch, since it
    ///    produces no slow frame either.
    #[test]
    fn a_stall_is_work_or_a_block_but_never_an_idle_poll() {
        let ms = Duration::from_millis;

        assert_eq!(
            stall_verdict(Duration::ZERO, ms(5000), false),
            None,
            "an idle poll with nothing pending is not a stall, however long"
        );
        assert_eq!(
            stall_verdict(ms(1), ms(1), true),
            None,
            "a healthy turn with a frame queued is not a stall"
        );

        // The blocked case: no CPU at all, so anything measuring work would miss it.
        assert_eq!(
            stall_verdict(Duration::ZERO, STALL_BLOCKED, true),
            Some("blocked with a frame due"),
            "a handler blocked in a syscall with a frame due is the case that \
             produces no slow frame, and must be caught"
        );
        assert_eq!(
            stall_verdict(Duration::ZERO, STALL_BLOCKED, false),
            None,
            "the same block with no frame owed is just an idle poll"
        );

        // The busy case is reported whether or not a frame was pending: CPU burned
        // outside the frame path comes out of the frame the loop owes next.
        assert_eq!(
            stall_verdict(STALL_CPU, Duration::ZERO, false),
            Some("busy")
        );
        assert_eq!(
            stall_verdict(STALL_CPU, STALL_BLOCKED, true),
            Some("busy and blocked")
        );

        // The thresholds are bounds, not decoration: just under must stay quiet.
        assert_eq!(
            stall_verdict(STALL_CPU - ms(1), STALL_BLOCKED - ms(1), true),
            None,
            "just under both thresholds must not report"
        );
    }

    /// A frame's own CPU is charged to the frame, not to the loop.
    ///
    /// Drives the real `begin`/`end` and `loop_turn_end` rather than the arithmetic:
    /// the subtraction is only correct if `end` actually accumulates, and a test of
    /// the formula would pass with the accumulation deleted.
    #[test]
    fn a_frames_cpu_does_not_read_as_a_loop_stall() {
        let mut log = test_log();
        log.settings = Some(Settings {
            summary_every: None,
            ..Settings::default()
        });

        // Open the window.
        log.loop_turn_end(false);
        assert_eq!(log.stalls(), 0);

        // A frame that burns real CPU. Busy-spin rather than sleep: sleeping accrues
        // wall time and no CPU, which is the *other* case entirely.
        //
        // The spin is bounded by CPU time, not wall time. libtest runs these in
        // parallel, so a wall-clock deadline can be spent descheduled — the thread
        // would accrue almost no CPU and the assertion below would fail on a busy
        // machine and pass on an idle one. Spinning on the same clock the assertion
        // reads makes it true by construction.
        log.begin("out");
        let spin_from = thread_cpu();
        while thread_cpu().saturating_sub(spin_from) < Duration::from_millis(6) {
            std::hint::spin_loop();
        }
        log.end(Some(Duration::from_millis(16)));

        assert!(
            log.loop_watch.redraw_cpu >= Duration::from_millis(5),
            "the frame must have been charged its own CPU, got {:?}",
            log.loop_watch.redraw_cpu
        );

        log.loop_turn_end(false);
        assert_eq!(
            log.stalls(),
            0,
            "a turn whose only cost was the frame is not a loop stall — the frame \
             log already reports it, in more detail than a stall line could"
        );
    }

    /// An empty ring writes no file at all.
    ///
    /// The signal handler is installed whether or not the recorder is on, so `SIGUSR1` with
    /// `SYNOIK_FRAME_LOG` unset used to leave a 0-byte `frame-log.<pid>.0.txt` in the state dir.
    /// Beside real dumps that reads as "the window was captured and there was nothing in it" —
    /// the opposite of what it means, and indistinguishable from a session that rendered nothing.
    #[test]
    fn dumping_an_empty_ring_writes_no_file() {
        let dir = std::env::temp_dir().join(format!(
            "synoik-empty-dump-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("dump.txt");

        let mut log = test_log();
        log.dump_override = Some(path.clone());

        let (reported, entries) = log.dump().expect("an empty dump is not an error");
        assert_eq!(entries, 0);
        assert!(
            !reported.exists(),
            "an empty ring must leave no file behind, found {reported:?}",
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A dump must contain everything the scorer needs to compute a miss rate.
    ///
    /// This is the test that was missing, and its absence cost a wrong conclusion.
    /// `correlate-frame-log.py` derives the rate purely by counting
    /// `missed N vblank(s)` lines against the summaries' aim histogram. Ring mode
    /// banked frames and summaries but only *warned* the miss lines, so every ring
    /// dump scored a flawless 0.00% — and the same workload's 13.99% under `all`
    /// got attributed to the instrument's cost rather than to the missing input.
    #[test]
    fn a_dump_carries_the_misses_the_scorer_counts() {
        let mut log = test_log();
        log.settings = Some(Settings {
            ring: Some(64),
            summary_every: Some(Duration::from_millis(1)),
            ..Settings::default()
        });

        log.begin("out");
        log.end(Some(Duration::from_millis(16)));

        // A presentation a whole refresh later than it aimed for is a miss.
        let refresh = Duration::from_micros(16667);
        log.presented(
            "out",
            Duration::from_secs(10),
            Duration::from_secs(10) + refresh,
            Some(refresh),
        );

        let banked: Vec<String> = log
            .ring
            .iter()
            .filter_map(|e| match e {
                Entry::Line(l) => Some(l.clone()),
                _ => None,
            })
            .collect();
        assert!(
            banked
                .iter()
                .any(|l| l.contains("missed 1 vblank(s) on out")),
            "the miss must be banked, not only warned — a dump without it scores \
             0.00% however badly the session actually missed. Banked: {banked:?}"
        );
    }

    /// The default dump location must survive a reboot.
    ///
    /// It used to be `$XDG_RUNTIME_DIR`, which is a tmpfs. That was survivable only
    /// while the frame log also wrote every line to journald — `ring` mode removes
    /// exactly that, so a runtime-dir dump is the only copy of a measurement in
    /// existence and a reboot takes it. This pins the category, not the string: any
    /// default under the runtime dir is a regression.
    #[test]
    fn dumps_land_somewhere_a_reboot_does_not_erase() {
        let os = |s: &str| Some(std::ffi::OsString::from(s));
        let run = os("/run/user/4242");

        let path = dump_dir_from(None, os("/home/someone"), run.clone());
        assert_eq!(
            std::path::PathBuf::from(&path),
            std::path::PathBuf::from("/home/someone/.local/state/synoik"),
            "with no XDG_STATE_HOME, dumps belong under the home state dir"
        );

        let path = dump_dir_from(os("/home/someone/.state"), os("/home/someone"), run.clone());
        assert_eq!(
            std::path::PathBuf::from(&path),
            std::path::PathBuf::from("/home/someone/.state/synoik"),
            "XDG_STATE_HOME must win over the home fallback"
        );

        // Only with nowhere durable to write does the tmpfs become acceptable.
        let path = dump_dir_from(None, None, run);
        assert_eq!(
            std::path::PathBuf::from(&path),
            std::path::PathBuf::from("/run/user/4242"),
            "the runtime dir is the last resort, never the default"
        );
    }

    /// Successive dumps must land in successive files. With one fixed name per PID
    /// the second `SIGUSR1` silently destroyed the first window — and the natural
    /// way to scope a measurement is exactly two dumps: one to clear, one to
    /// collect. An explicit override is still taken verbatim, because the caller
    /// asked for that name.
    ///
    /// This test used to `remove_var("SYNOIK_FRAME_LOG_DUMP")`, which is what broke a
    /// *different* test mid-dump: the override is now a parameter, so nothing here
    /// touches process-global state.
    #[test]
    fn successive_dumps_do_not_overwrite_each_other() {
        assert_ne!(
            dump_path(None, 0),
            dump_path(None, 1),
            "each dump needs its own file, or the second destroys the first"
        );
        // ...but an explicit path is that path, on every dump.
        let over = std::path::PathBuf::from("/tmp/explicit.txt");
        assert_eq!(dump_path(Some(over.clone()), 0), over);
        assert_eq!(dump_path(Some(over.clone()), 7), over);
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
            synoik_vk::stats::SubmitSite::KmsFrame,
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
    /// (`docs/fork/foundation.md` §3), so pin the three shapes it can take.
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
            dump_override: None,
            ring: VecDeque::new(),
            dumps: 0,
            settings: Some(Settings::default()),
            in_flight: None,
            stats: HashMap::new(),
            queued: HashMap::new(),
            last_presented: HashMap::new(),
            last_summary: Instant::now(),
            last_autodump: None,
            autodumps: 0,
            loop_watch: LoopWatch::default(),
            lateness: DispatchLateness::default(),
            lifetime: HashMap::new(),
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
            dump_override: None,
            ring: VecDeque::new(),
            dumps: 0,
            settings: Some(Settings::default()),
            in_flight: None,
            stats: HashMap::new(),
            queued: HashMap::new(),
            last_presented: HashMap::new(),
            last_summary: Instant::now(),
            last_autodump: None,
            autodumps: 0,
            loop_watch: LoopWatch::default(),
            lateness: DispatchLateness::default(),
            lifetime: HashMap::new(),
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
            dump_override: None,
            ring: VecDeque::new(),
            dumps: 0,
            settings: Some(Settings::default()),
            in_flight: None,
            stats: HashMap::new(),
            queued: HashMap::new(),
            last_presented: HashMap::new(),
            last_summary: Instant::now(),
            last_autodump: None,
            autodumps: 0,
            loop_watch: LoopWatch::default(),
            lateness: DispatchLateness::default(),
            lifetime: HashMap::new(),
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
            dump_override: None,
            ring: VecDeque::new(),
            dumps: 0,
            settings: Some(Settings::default()),
            in_flight: None,
            stats: HashMap::new(),
            queued: HashMap::new(),
            last_presented: HashMap::new(),
            last_summary: Instant::now(),
            last_autodump: None,
            autodumps: 0,
            loop_watch: LoopWatch::default(),
            lateness: DispatchLateness::default(),
            lifetime: HashMap::new(),
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
            dump_override: None,
            ring: VecDeque::new(),
            dumps: 0,
            settings: Some(Settings::default()),
            in_flight: None,
            stats: HashMap::new(),
            queued: HashMap::new(),
            last_presented: HashMap::new(),
            last_summary: Instant::now(),
            last_autodump: None,
            autodumps: 0,
            loop_watch: LoopWatch::default(),
            lateness: DispatchLateness::default(),
            lifetime: HashMap::new(),
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
            synoik_vk::stats::SubmitSite::KmsFrame,
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

    /// A full-output opaque element hides everything under it, and the renderer does not shade
    /// what it skips. An instrument that keeps counting those elements reports a cost nobody pays
    /// and sends the next optimisation at something already free — which is exactly what it did:
    /// a full-output `SolidColor` backdrop under an opaque wallpaper read as a whole extra output
    /// of scene on kov's seat, against a fill-rate counter that never saw it.
    #[test]
    fn a_hidden_element_costs_nothing_in_the_breakdown() {
        use smithay::backend::renderer::element::{Id, Kind};
        use smithay::backend::renderer::utils::CommitCounter;
        use smithay::backend::renderer::Color32F;
        use smithay::utils::{Point, Rectangle, Size};

        use crate::render_helpers::solid_color::SolidColorRenderElement;

        let output = Rectangle::from_size(Size::from((100, 100)));
        let full = Rectangle::new(Point::from((0., 0.)), Size::from((100., 100.)));
        let solid = |color| {
            SolidColorRenderElement::new(
                Id::new(),
                full,
                CommitCounter::default(),
                color,
                Kind::Unspecified,
            )
        };
        let opaque = Color32F::from([1., 0., 0., 1.]);
        let translucent = Color32F::from([0., 1., 0., 0.5]);

        // Opaque on top, backdrop beneath: the backdrop is never shaded.
        let line = scene_breakdown(&[solid(opaque), solid(opaque)], 1.0.into(), output)
            .expect("a non-empty output has a breakdown");
        assert!(
            line.contains("1.00x the output"),
            "an element hidden behind an opaque one must not be counted: {line}"
        );

        // The same two with a translucent element on top: nothing is hidden, both are shaded.
        let line = scene_breakdown(&[solid(translucent), solid(opaque)], 1.0.into(), output)
            .expect("a non-empty output has a breakdown");
        assert!(
            line.contains("2.00x the output"),
            "a translucent element hides nothing, so both are shaded: {line}"
        );
    }

    /// A truncated culprit list must say what it left out.
    ///
    /// The line names only the largest few, which is what keeps it readable — but an unnamed
    /// culprit is exactly where the cause hides: the element that changed the scene's element
    /// count is often tiny, and it is the *count* that renumbers everything below it.
    #[test]
    fn the_attribution_line_counts_the_culprits_it_does_not_name() {
        let culprit = |kind: &str, reason: &'static str, share: f64| super::Culprit {
            kind: kind.to_owned(),
            reason,
            rects: 1,
            share,
        };
        // Eight culprits, descending, so two fall past the six the line names.
        let a = super::FrameAttribution {
            shaded: 20,
            quiet: 12,
            predicted: 0.5,
            culprits: vec![
                culprit("Monitor", "z-index", 0.4),
                culprit("Monitor", "z-index", 0.3),
                culprit("Monitor", "z-index", 0.2),
                culprit("Monitor", "z-index", 0.1),
                culprit("Panel", "commit", 0.05),
                culprit("Panel", "commit", 0.04),
                culprit("Pointer", "new", 0.0),
                culprit("Pointer", "gone", 0.0),
            ],
        };
        let line = a.line();
        assert!(
            line.contains("+2 smaller"),
            "the line must admit it truncated: {line}"
        );
        assert!(
            line.contains("1x gone") && line.contains("1x new"),
            "the omitted culprits must still be counted by reason: {line}"
        );

        // Nothing omitted, nothing claimed.
        let short = super::FrameAttribution {
            shaded: 3,
            quiet: 2,
            predicted: 0.1,
            culprits: vec![culprit("Monitor", "commit", 0.1)],
        };
        assert!(
            !short.line().contains("smaller"),
            "a complete list must not claim a tail: {}",
            short.line()
        );
    }
}
