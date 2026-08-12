// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! How late a wakeup asked for at an exact moment actually arrives.
//!
//! synoik holds a continuous frame until `vblank − (render estimate + margin)` and releases it on
//! an event-loop timer. On this VM that release measured ~1 ms late on average and 2.8 ms at the
//! tail, which is most of what the deadline margin turned out to be paying for. That number alone
//! cannot say *why*, and the three candidates want three different projects:
//!
//! 1. calloop's timer machinery,
//! 2. our event loop being busy with something else when the deadline came,
//! 3. the vCPU not running at all, which no userspace loop can fix.
//!
//! This probe removes (2) by construction — its loop has nothing else to do — and separates (1)
//! from (3) by asking for the same wakeups two ways: through a calloop `Timer`, and through a raw
//! absolute `clock_nanosleep`, which is as close to the kernel as a thread can ask. If both are
//! late by the same amount, the timer is innocent and the host is the floor. If calloop is late
//! and the raw sleep is punctual, the loop is the problem and a better timer source fixes it.
//!
//! Run it on the seat *under the same load* the compositor sees; an idle machine measures nothing.

use std::time::{Duration, Instant};

/// Upper edges, in microseconds, matching `FrameLog`'s release-lateness buckets so the two
/// histograms can be read side by side.
const EDGES_US: [u64; 7] = [100, 250, 500, 1_000, 2_000, 4_000, 8_000];

#[derive(Default)]
struct Histogram {
    buckets: [u64; EDGES_US.len() + 1],
    count: u64,
    total: Duration,
    worst: Duration,
}

impl Histogram {
    fn record(&mut self, lateness: Duration) {
        let us = lateness.as_micros() as u64;
        let bucket = EDGES_US
            .iter()
            .position(|edge| us < *edge)
            .unwrap_or(EDGES_US.len());
        self.buckets[bucket] += 1;
        self.count += 1;
        self.total += lateness;
        self.worst = self.worst.max(lateness);
    }

    fn report(&self, name: &str) {
        if self.count == 0 {
            println!("{name}: no samples");
            return;
        }
        let mean = self.total.as_secs_f64() * 1000. / self.count as f64;
        println!(
            "{name}: mean {:.3}ms, worst {:.3}ms over {} wakeups",
            mean,
            self.worst.as_secs_f64() * 1000.,
            self.count,
        );
        let mut parts = Vec::new();
        for (i, count) in self.buckets.iter().enumerate() {
            if *count == 0 {
                continue;
            }
            match EDGES_US.get(i) {
                Some(edge) => parts.push(format!("<{edge}us x{count}")),
                None => parts.push(format!(">={}us x{count}", EDGES_US[EDGES_US.len() - 1])),
            }
        }
        println!("  {}", parts.join("  "));
    }
}

/// Ask for `cycles` wakeups, `interval` apart, through a calloop timer.
fn calloop_arm(cycles: u64, interval: Duration) -> Histogram {
    let mut event_loop: calloop::EventLoop<(Histogram, u64, Instant)> =
        calloop::EventLoop::try_new().expect("event loop");
    let handle = event_loop.handle();

    let start = Instant::now() + interval;
    handle
        .insert_source(
            calloop::timer::Timer::from_deadline(start),
            move |deadline, _, (hist, fired, _): &mut (Histogram, u64, Instant)| {
                // `deadline` is the instant the timer was armed for, so this is exactly the
                // question: how long after that moment did the callback actually run.
                hist.record(Instant::now().saturating_duration_since(deadline));
                *fired += 1;
                calloop::timer::TimeoutAction::ToInstant(deadline + interval)
            },
        )
        .expect("insert timer");

    let mut data = (Histogram::default(), 0u64, Instant::now());
    while data.1 < cycles {
        event_loop
            .dispatch(Some(Duration::from_millis(100)), &mut data)
            .expect("dispatch");
    }
    data.0
}

/// The same wakeups, asked for with an absolute `clock_nanosleep`. No event loop, no readiness
/// machinery — whatever lateness survives here is the kernel and the host.
fn raw_arm(cycles: u64, interval: Duration) -> Histogram {
    use rustix::thread::clock_nanosleep_absolute;
    use rustix::time::{clock_gettime, ClockId};

    let mut hist = Histogram::default();
    let mut deadline = clock_gettime(ClockId::Monotonic);
    for _ in 0..cycles {
        deadline = add(deadline, interval);
        let _ = clock_nanosleep_absolute(ClockId::Monotonic, &deadline);
        let now = clock_gettime(ClockId::Monotonic);
        hist.record(diff(now, deadline));
    }
    hist
}

fn add(ts: rustix::time::Timespec, d: Duration) -> rustix::time::Timespec {
    let mut nsec = ts.tv_nsec as i64 + d.subsec_nanos() as i64;
    let mut sec = ts.tv_sec + d.as_secs() as i64;
    if nsec >= 1_000_000_000 {
        nsec -= 1_000_000_000;
        sec += 1;
    }
    rustix::time::Timespec {
        tv_sec: sec,
        tv_nsec: nsec as _,
    }
}

fn diff(now: rustix::time::Timespec, then: rustix::time::Timespec) -> Duration {
    let secs = now.tv_sec - then.tv_sec;
    let nanos = now.tv_nsec as i64 - then.tv_nsec as i64;
    let total = secs * 1_000_000_000 + nanos;
    Duration::from_nanos(total.max(0) as u64)
}

fn main() {
    let cycles: u64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(3_600);
    // 60Hz, the cadence the frame clock arms its deadlines at.
    let interval = Duration::from_micros(16_667);

    println!("{cycles} wakeups at {:?} each, two ways.", interval);
    println!("Run this under the load the compositor sees; an idle machine measures nothing.\n");

    // Raw first: if the machine is already unable to wake a sleeping thread on time, the calloop
    // number that follows means nothing on its own.
    raw_arm(cycles, interval).report("clock_nanosleep");
    calloop_arm(cycles, interval).report("calloop timer");
}
