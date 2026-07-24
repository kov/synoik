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
//! - **Frame cost**, phase by phase ([`Phase`]), measured on the compositor thread. Note the render
//!   phase *includes* GPU execution: the Vulkan renderer submits and fence-waits synchronously, so
//!   `finish` does not return until the GPU is done. A slow `submit` is therefore ambiguous between
//!   CPU and GPU until the `gpu` option splits it out.
//! - **Presented frames**, from the DRM vblank sequence. A gap in the sequence is a frame the user
//!   did not get, which is the thing actually perceived as a stutter — and it can happen with every
//!   frame cost looking healthy (a late commit, a missed deadline elsewhere, the scanout engine).
//!
//! Neither is much use without knowing what the frame was *doing*, so a logged
//! frame carries its [`FrameContext`]: element count, whether the damage was
//! forced full, how many widget bakes ran (an uncached bake is a full GPU
//! round-trip), and the overview/animation state.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::utils::get_monotonic_time;

/// Number of widget bakes that have run since the process started. A bake is an
/// uncached rasterization into its own GPU texture — a render pass, a submit and
/// a fence wait each — so a frame doing several is a prime stutter suspect.
///
/// A free-standing counter rather than a field because bakes happen deep inside
/// widget code that has no reason to know about frame logging; the frame log
/// samples the delta across a frame.
static BAKES: AtomicU64 = AtomicU64::new(0);

/// Whether GPU timing was requested, sampled once. The renderer reads this at
/// construction to decide whether to allocate a timestamp query pool, so it must
/// answer the same way for the whole process.
static GPU_TIMING: AtomicBool = AtomicBool::new(false);

/// Accumulated GPU time reported by the renderer for the frame being built, in
/// nanoseconds. The renderer adds each submit's measured duration; the frame log
/// takes and clears it when the frame ends.
///
/// Shared through a static for the same reason as [`BAKES`]: a `VulkanFrame` is
/// created in a dozen places that would otherwise all have to carry a handle to
/// the log, and this is debug instrumentation, not a data path.
static GPU_NANOS: AtomicU64 = AtomicU64::new(0);

/// Count one widget bake. Called from the single uncached-bake path.
pub fn count_bake() {
    // Relaxed throughout: these counters are read on the same thread that writes
    // them in the common case, and a torn count in a debug log is not a problem
    // worth an ordering for.
    BAKES.fetch_add(1, Ordering::Relaxed);
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

/// Take the GPU time banked since the last call. The frame log calls this when a
/// frame ends; tests use it to check that the renderer's timestamps arrive at all.
pub fn take_gpu_time() -> Duration {
    Duration::from_nanos(GPU_NANOS.swap(0, Ordering::Relaxed))
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
    /// Screencast, recording and screencopy captures riding on this redraw.
    Captures,
    /// Waking clients for the next frame.
    Callbacks,
}

impl Phase {
    const ALL: [Phase; 6] = [
        Phase::Elements,
        Phase::Collect,
        Phase::Submit,
        Phase::Queue,
        Phase::Captures,
        Phase::Callbacks,
    ];

    fn label(self) -> &'static str {
        match self {
            Phase::Elements => "elements",
            Phase::Collect => "collect",
            Phase::Submit => "submit",
            Phase::Queue => "queue",
            Phase::Captures => "captures",
            Phase::Callbacks => "callbacks",
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
}

impl Stats {
    fn record(&mut self, total: Duration, over: bool, gpu: Duration) {
        self.frames += 1;
        self.totals.push(total);
        self.worst = self.worst.max(total);
        self.over_budget += u64::from(over);
        self.gpu_total += gpu;
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

/// Per-output presentation tracking, for spotting frames that never reached the
/// display.
#[derive(Debug, Default)]
struct Presentation {
    last_sequence: Option<u32>,
    last_time: Option<Duration>,
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
    context: FrameContext,
}

/// See the [module docs](self).
#[derive(Debug)]
pub struct FrameLog {
    settings: Option<Settings>,
    in_flight: Option<InFlight>,
    stats: HashMap<String, Stats>,
    presentation: HashMap<String, Presentation>,
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
            presentation: HashMap::new(),
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

        let now = Instant::now();
        self.in_flight = Some(InFlight {
            output: output.to_owned(),
            started: now,
            phase_started: now,
            phase: None,
            spans: Vec::with_capacity(Phase::ALL.len()),
            bakes_at_start: BAKES.load(Ordering::Relaxed),
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
        let bakes = BAKES.load(Ordering::Relaxed) - frame.bakes_at_start;
        let gpu = take_gpu_time();

        // The budget: an explicit threshold if given, else the refresh interval.
        // With neither (a headless output with no refresh) nothing is "too long",
        // so only `all` logs.
        let budget = settings.threshold.or(refresh);
        let over = budget.is_some_and(|budget| total > budget);

        if over || settings.log_all {
            let line = Self::format_frame(&frame, total, gpu, bakes, budget);
            if over {
                tracing::warn!("{line}");
            } else {
                tracing::debug!("{line}");
            }
        }

        self.stats
            .entry(frame.output)
            .or_default()
            .record(total, over, gpu);

        self.maybe_summarize(now);
    }

    fn format_frame(
        frame: &InFlight,
        total: Duration,
        gpu: Duration,
        bakes: u64,
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
        if !gpu.is_zero() {
            let _ = write!(line, " (gpu {})", ms(gpu));
        }

        let ctx = &frame.context;
        let _ = write!(line, "; {} elements", ctx.elements);
        if bakes > 0 {
            let _ = write!(line, ", {bakes} bakes");
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

    /// Record a presented frame from the DRM vblank. A jump in `sequence` means
    /// the display went through vblanks we never filled — the frames a user
    /// actually sees as a stutter.
    pub fn presented(&mut self, output: &str, sequence: u32, refresh: Option<Duration>) {
        if self.settings.is_none() {
            return;
        }

        let now = get_monotonic_time();
        let entry = self.presentation.entry(output.to_owned()).or_default();
        let previous_time = entry.last_time.replace(now);
        let previous = entry.last_sequence.replace(sequence);

        let Some(previous) = previous else {
            return;
        };

        // Sequence wrap and backwards jumps (a mode set, a VT switch) are not
        // drops; `wrapping_sub` handles the wrap, and anything absurd is treated
        // as a discontinuity rather than thousands of dropped frames.
        let missed = sequence.wrapping_sub(previous).saturating_sub(1);
        if missed == 0 || missed > 60 {
            return;
        }

        self.stats.entry(output.to_owned()).or_default().dropped += u64::from(missed);

        let gap = previous_time.map(|previous| now.saturating_sub(previous));
        match (gap, refresh) {
            (Some(gap), Some(refresh)) => tracing::warn!(
                "dropped {missed} frame(s) on {output}: {} between presentations, refresh {}",
                ms(gap),
                ms(refresh),
            ),
            _ => tracing::warn!("dropped {missed} frame(s) on {output}"),
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

            let gpu = if stats.gpu_total.is_zero() {
                String::new()
            } else {
                format!(", gpu avg {}", ms(stats.gpu_total / stats.frames as u32))
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

    /// Phase marks name the work that *follows* them, and the totals add up.
    #[test]
    fn phases_attribute_the_span_after_the_mark() {
        let mut log = FrameLog {
            settings: Some(Settings::default()),
            in_flight: None,
            stats: HashMap::new(),
            presentation: HashMap::new(),
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

    /// A gap in the vblank sequence is dropped frames; a wrap is not, and neither
    /// is the first frame seen.
    #[test]
    fn sequence_gaps_count_as_dropped_frames() {
        let mut log = FrameLog {
            settings: Some(Settings::default()),
            in_flight: None,
            stats: HashMap::new(),
            presentation: HashMap::new(),
            last_summary: Instant::now(),
        };
        let refresh = Some(Duration::from_micros(16667));

        log.presented("out", 10, refresh);
        assert!(
            !log.stats.contains_key("out"),
            "the first frame is a baseline"
        );

        log.presented("out", 11, refresh);
        assert_eq!(log.stats.get("out").map_or(0, |s| s.dropped), 0);

        log.presented("out", 14, refresh);
        assert_eq!(log.stats["out"].dropped, 2);

        // Wrapping the counter is not a drop storm.
        log.presented("out", u32::MAX, refresh);
        let after_jump = log.stats["out"].dropped;
        log.presented("out", 0, refresh);
        assert_eq!(log.stats["out"].dropped, after_jump);
    }
}
