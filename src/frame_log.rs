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
//! **Known gap on the dev VM:** the `gpu` option produces nothing here. The
//! virtio-gpu/Venus stack advertises timestamp queries and resolves every one to
//! zero, which the renderer detects and reports once before going quiet. The
//! evidence and the handoff for fixing it host-side are in
//! `docs/fork/venus-timestamp-gap.md`.
//!
//! Because the host-side fix is expected to land as a *partial* hit rate rather
//! than all-or-nothing, a pair that comes back unusable is counted (`N lost`)
//! instead of quietly averaged in as zero: a GPU time with samples missing is a
//! floor, and the line says so.
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
//!   compositor.
//!
//! Neither is much use without knowing what the frame was *doing*, so a logged
//! frame carries its [`FrameContext`]: element count, whether the damage was
//! forced full, how many widget bakes ran (an uncached bake is a full GPU
//! round-trip), and the overview/animation state.

use std::cell::Cell;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
}

/// Whether GPU timing was requested, sampled once. The renderer reads this at
/// construction to decide whether to allocate a timestamp query pool, so it must
/// answer the same way for the whole process.
static GPU_TIMING: AtomicBool = AtomicBool::new(false);

/// Accumulated GPU time reported by the renderer for the frame being built, in
/// nanoseconds. The renderer adds each submit's measured duration; the frame log
/// takes and clears it when the frame ends.
///
/// A free-standing counter for the same reason as [`BAKES`]: a `VulkanFrame` is
/// created in a dozen places that would otherwise all have to carry a handle to
/// the log, and this is debug instrumentation, not a data path. Process-wide
/// rather than per-thread because nothing asserts on it.
static GPU_NANOS: AtomicU64 = AtomicU64::new(0);

/// Timestamp pairs the renderer read back and could not use, for the frame being
/// built. Counted, not silently dropped: a stack that writes only some of its
/// timestamps would otherwise make [`GPU_NANOS`] a sum over an unknown subset of
/// the frame's passes, i.e. a number that reads like a total and is a floor.
/// With the loss count beside it the reader can tell the two apart.
static GPU_LOST: AtomicU64 = AtomicU64::new(0);

/// Whether anything is listening. Sampled by the scoped timers so an unlogged
/// session does not pay two `Instant::now()` calls per bake and per shaped run.
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Accumulates its lifetime into this thread's bake time when dropped. Inert when
/// frame logging is off, so call sites can be unconditional.
pub struct Timed(Option<Instant>);

impl Drop for Timed {
    fn drop(&mut self) {
        if let Some(started) = self.0 {
            let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
            BAKE_NANOS.with(|c| c.set(c.get().saturating_add(nanos)));
        }
    }
}

/// Time a widget bake — an uncached rasterization into its own GPU texture. Hold
/// the returned guard for the operation.
pub fn time_bake() -> Timed {
    BAKES.with(|c| c.set(c.get().saturating_add(1)));
    Timed(ENABLED.load(Ordering::Relaxed).then(Instant::now))
}

/// Widget bakes **this thread** has run since it started. Exposed so a test can
/// assert that a repaint did **not** re-bake — a bake is a GPU round trip, so "did
/// this stay cached?" is a correctness question about frame cost that pixels
/// cannot answer. A test owns its renderer, so its own thread's count is exactly
/// the bakes it caused; see [`BAKES`].
pub fn bakes() -> u64 {
    BAKES.with(Cell::get)
}

/// Whether the renderer should measure GPU pass durations. See [`FrameLog::from_env`].
pub fn gpu_timing() -> bool {
    GPU_TIMING.load(Ordering::Relaxed)
}

/// Report a submit's measured GPU duration. Called by the renderer after it
/// reads back its timestamp queries.
pub fn add_gpu_time(duration: Duration) {
    GPU_NANOS.fetch_add(
        u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );
}

/// Report a timestamp pair that came back unusable. See [`GPU_LOST`].
pub fn add_gpu_lost() {
    GPU_LOST.fetch_add(1, Ordering::Relaxed);
}

/// Take the GPU time banked since the last call. The frame log calls this when a
/// frame ends; tests use it to check that the renderer's timestamps arrive at all.
pub fn take_gpu_time() -> Duration {
    Duration::from_nanos(GPU_NANOS.swap(0, Ordering::Relaxed))
}

/// Take the count of unusable timestamp pairs banked since the last call.
pub fn take_gpu_lost() -> u64 {
    GPU_LOST.swap(0, Ordering::Relaxed)
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
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            threshold: None,
            log_all: false,
            summary_every: Some(Duration::from_secs(10)),
        }
    }
}

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

/// The frame being timed right now.
#[derive(Debug)]
struct InFlight {
    output: String,
    started: Instant,
    /// When the current phase began — every mark closes one span and opens the next.
    phase_started: Instant,
    phase: Option<Phase>,
    spans: Vec<(Phase, Duration)>,
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
#[derive(Debug, Default)]
struct Totals {
    gpu: Duration,
    /// Timestamp pairs the renderer could not use. Nonzero means `gpu` is a
    /// floor, not a total. See [`GPU_LOST`].
    gpu_lost: u64,
    bakes: u64,
    baking: Duration,
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

/// See the [module docs](self).
#[derive(Debug)]
pub struct FrameLog {
    settings: Option<Settings>,
    in_flight: Option<InFlight>,
    stats: HashMap<String, Stats>,
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

        Self {
            settings,
            in_flight: None,
            stats: HashMap::new(),
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
                ("gpu", None) => {
                    enabled = true;
                    GPU_TIMING.store(true, Ordering::Relaxed);
                }
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

        let now = Instant::now();
        self.in_flight = Some(InFlight {
            output: output.to_owned(),
            started: now,
            phase_started: now,
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
        if let Some(previous) = frame.phase.replace(phase) {
            frame.spans.push((previous, now - frame.phase_started));
        }
        frame.phase_started = now;
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

        let now = Instant::now();
        if let Some(last) = frame.phase.take() {
            frame.spans.push((last, now - frame.phase_started));
        }
        let total = now - frame.started;
        let totals = Totals {
            gpu: take_gpu_time(),
            gpu_lost: take_gpu_lost(),
            bakes: bakes() - frame.bakes_at_start,
            baking: Duration::from_nanos(BAKE_NANOS.with(|c| c.replace(0))),
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
        let over = budget.is_some_and(|budget| total > budget);

        if over || settings.log_all {
            let line = Self::format_frame(&frame, total, &totals, budget);
            if over {
                tracing::warn!("{line}");
            } else {
                tracing::debug!("{line}");
            }
        }

        self.stats.entry(frame.output).or_default().record(
            total,
            over,
            totals.gpu,
            totals.gpu_lost,
        );

        self.maybe_summarize(now);
    }

    fn format_frame(
        frame: &InFlight,
        total: Duration,
        totals: &Totals,
        budget: Option<Duration>,
    ) -> String {
        let mut line = format!("frame on {} took {}", frame.output, ms(total));
        if let Some(budget) = budget {
            let _ = write!(line, " (budget {})", ms(budget));
        }

        // Phases in a fixed order rather than the order recorded, so successive
        // lines line up when you read a run of them.
        line.push_str(" —");
        for phase in Phase::ALL {
            let spent: Duration = frame
                .spans
                .iter()
                .filter(|(p, _)| *p == phase)
                .map(|(_, d)| *d)
                .sum();
            if !spent.is_zero() {
                let _ = write!(line, " {} {}", phase.label(), ms(spent));
            }
        }
        if !totals.gpu.is_zero() {
            let _ = write!(line, " (gpu {}", ms(totals.gpu));
            if totals.gpu_lost > 0 {
                let _ = write!(line, ", {} lost", totals.gpu_lost);
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
        if self.settings.is_none() {
            return;
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

        tracing::warn!(
            "missed {missed} vblank(s) on {output}: presented {} late, refresh {}",
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

            tracing::info!(
                "{output}: {:.1} fps over {}, p50 {}, p95 {}, worst {}, {} over budget, \
                 {} dropped{gpu}",
                fps,
                ms(elapsed),
                ms(p50),
                ms(p95),
                ms(stats.worst),
                stats.over_budget,
                stats.dropped,
            );

            *stats = Stats::default();
        }
    }
}

/// Milliseconds with two decimals — the resolution that matters against a 16.7ms
/// budget, without the noise of `Duration`'s own formatting.
fn ms(duration: Duration) -> String {
    format!("{:.2}ms", duration.as_secs_f64() * 1000.)
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
            output: "out".to_owned(),
            started: now,
            phase_started: now,
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

    /// Phase marks name the work that *follows* them, and the totals add up.
    #[test]
    fn phases_attribute_the_span_after_the_mark() {
        let mut log = FrameLog {
            settings: Some(Settings::default()),
            in_flight: None,
            stats: HashMap::new(),
            last_summary: Instant::now(),
        };

        log.begin("test-output");
        log.phase(Phase::Elements);
        log.phase(Phase::Submit);
        let frame = log.in_flight.as_ref().unwrap();
        assert_eq!(frame.spans.len(), 1, "the mark closes exactly one span");
        assert_eq!(frame.spans[0].0, Phase::Elements);

        log.end(Some(Duration::from_millis(16)));
        assert!(log.in_flight.is_none());
        let stats = &log.stats["test-output"];
        assert_eq!(stats.frames, 1);
    }

    /// A frame is missed when it lands a whole refresh cycle or more after the
    /// deadline it was built for — and, crucially, **not** merely because time
    /// passed since the previous frame.
    #[test]
    fn only_a_late_presentation_counts_as_missed() {
        let refresh = Duration::from_micros(16667);
        let mut log = FrameLog {
            settings: Some(Settings::default()),
            in_flight: None,
            stats: HashMap::new(),
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
            settings: Some(Settings::default()),
            in_flight: None,
            stats: HashMap::new(),
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
}
