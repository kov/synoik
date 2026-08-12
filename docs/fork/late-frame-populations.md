# Late frames come in three populations

A session's `missed N vblank(s)` lines are not one phenomenon. Read from a 6-hour live seat run on
2026-08-12 (pid 3079, 196 458 frames, 7 854 late presentations), they separate cleanly into three
populations with three different causes, three different owners, and three different fixes. Sorting
a late-frame report into these buckets *before* theorising is the point of this document — two of
the three are not renderer bugs at all, and one of them is not even ours.

The instruments: `synoik msg frame-perf` for the tallies, `SIGUSR1` for a ring dump
(`src/utils/signals.rs`, non-terminating — it is the only way to get the ring out of a live session,
see [the instrument gap](#the-instrument-gap) below), and the journal for the `main loop busy` lines.

## How to tell them apart

The miss line carries the discriminator. `N cycles since the last flip` says what the screen was
doing, and `queued X ms LATE` says how badly we were beaten:

| population | signature | share | cause | owner |
|---|---|---|---|---|
| **1. cold bake** | `3600 cycles`, 3–4 vblanks, `queued 30–50 ms LATE` | 236 events (1/min) | the full-bar panel bake, cold | ours, fixable |
| **2. sporadic submit** | `3–10 cycles`, 1 vblank, `queued 0.3–4 ms LATE` | 7 242 of 7 778 | our submit round trip | ours, cost not scheduling |
| **3. descheduled** | any, wide, correlated with `main loop busy` | 1 432 stalls | host memory pressure | environmental |

## Population 1 — the minute clock tick re-bakes the whole panel bar

Bake sites over the whole ring (65 536 entries, ~4 h):

| site | bakes | total |
|---|---|---|
| `ui/panel.rs` bar chrome | 236 | **12 426 ms** |
| `ui/switcher/ui.rs` | 4 | 8.9 ms |
| `ui/panel.rs` (other) | 2 | 2.9 ms |

Per bake: **min 0.9 ms, p50 60.7 ms, p90 91.8 ms, p99 311 ms, max 469 ms.** One per minute, forever,
on an otherwise idle desktop, to change a clock digit. A representative idle-minute frame:

```
frame took 38.36ms — collect 34.50ms submit 3.74ms ... 30 draws covering 0.1x the output,
waiting 37.54ms (first offscreen 33.94ms), 1 bakes in 34.08ms (ui/panel.rs bar chrome)
missed 3 vblank(s): presented 53.70ms late, 3599 cycles since the last flip
```

Three facts make this diagnosable rather than mysterious:

- **`min 0.9 ms` is the same bake, warm.** A p50 sixty times the minimum is not a slow bake, it is a
  *cold* one. The once-a-minute cadence guarantees the path is cold at every single use.
- **`0.1x the output` in 30 draws.** The work is trivial. The cost is the round trip, and the buffer
  allocation, and whatever the driver had paged out.
- **`collect 34.50ms` with `submit 3.74ms`.** The time is inside `collect`, i.e. inside the bake,
  not in the frame's own submit. `widget::bake_uncached_sized` says so itself: "each one is a render
  pass, a submit and a fence wait".

This is the [cold-cost class](./first-frame-costs.md): invisible to any steady-state instrument,
because by construction it never happens twice in a row.

**Fix (landed):** the bar bake was a full-output-width texture holding three text labels — the
clock, the recording `M:SS`, and the keyboard layout — so *any* of them changing re-rasterized the
entire bar. Each label is now its own small texture, cached on its own content, exactly as the
background, the pills and the workspace dots already were. The clock tick re-bakes a clock-sized
rect. `a_clock_tick_rebakes_only_the_clock` is the guard, counted on bakes rather than pixels
because a needless re-bake renders identically to a cached one.

Note the neighbour, which had the same shape and was worse: the screen-recording label is `M:SS`, so
an active recording re-baked the full bar **every second**. Splitting one label and leaving the
others would have left a latent version of the same bug.

## Population 2 — sporadic updates lose to our own submit cost

7 242 of 7 778 misses are exactly one vblank, after a 3–10 cycle gap, `queued` only 0.3–4 ms late
(mode 1–2 ms). The frames immediately preceding them:

```
n=5530   mean took 5.64ms — elements 0.03  collect 0.18  submit 5.29
```

**94% of it is `submit`.** Not our CPU work — the venus round trip. The median drawing frame is
1–4 ms; the ones that miss average 5.6 ms. Damage arrives at a random phase in the refresh cycle,
we render immediately, and whenever less than ~5.6 ms remained we land one vblank later.

### What mutter does here, and what it does not

It is tempting to call this a scheduling bug and reach for mutter's `max_render_time`. **That is
wrong, and mutter's own source says so.** `clutter_frame_clock_estimate_max_update_time_us` and
`calculate_next_update_time_us` (`clutter/clutter/clutter-frame-clock.c:857-1090`, 50.3) do compute
a render-time estimate and dispatch at `next_presentation − max_update_time` — but only when the
clock is in the *continuous* case. `should_update_now` (`:976-1023`) short-circuits all of it:

> There was an idle period since the last presentation, so there seems be no constantly updating
> actor. In this case it's best to start working on the next update ASAP, this results in lowest
> average latency for sporadic user input.

A 3–10 cycle gap **is** that idle period. Mutter dispatches immediately, exactly as we do, and
accepts the same one-vblank landing. Waiting for a deadline would guarantee the later vblank *and*
add latency. Our behaviour in this population already matches GNOME's; the only lever on it is the
5.3 ms submit itself.

Our split, for scale: only ~1 008 misses (13%) are 1–2-cycle, i.e. the continuous case where
deadline dispatch would apply at all. `RedrawState::WaitingForVBlank` already gives that case a full
refresh interval of headroom.

Deadline dispatch has since landed anyway (`5b7ad854`) — **not** as a fix for anything on this page.
It cannot reduce a miss count: a frame started at the top of the cycle already had the whole
interval. What it buys is latency in the continuous case, where the old behaviour finished the frame
and then sat on it for ~14 ms while input and animation aged. It also closes the ahead-by-one caveat
at the end of this document, by re-coupling the stamp to the landing. Everything else — the idle
short-circuit above included — still dispatches immediately, which is the regression bound.

**It measured worse, and now ships off.** Two counterbalanced four-block A/Bs on the gsrs seat under
a continuous 60 fps client (`vkcube`, FIFO), toggled inside one session so both arms share one set
of background work, ~14 300 frames per arm:

| round | pairs (held vs immediate), gaps ≥2 cycles |
| --- | --- |
| 1 (on first) | 2 vs 3 · 16 vs 5 |
| 2 (off first) | 14 vs 1 · 16 vs 4 |

48/14 353 held (**0.33%**) against 13/14 372 immediate (**0.09%**) — held is worse in three of the
four adjacent pairs, and the exception is round 1's first block, where the render-time estimate was
still empty and the arm was therefore behaving like the other one. The deepest misses in the whole
run were in the *immediate* arm and are population 2 (`waiting 33.60ms (first scanout)`), untouched
by either setting.

**The host was inflating the control, not the treatment.** Re-run after a VMM deploy quieted the
host, as a margin sweep — eight 60 s blocks, every treatment block between two baselines:

Two sweeps, sixteen blocks, ~58 000 frames. Arms below are the ones the **journal** confirms, not
the ones the script labelled — see the trap at the end of this section:

| margin | drops / frames | rate |
| --- | --- | --- |
| off (8 blocks) | 12 / 28 795 | 0.042% |
| 1 ms | 12 / 3 588 | **0.33%** |
| 2 ms | 4 / 3 596 | 0.11% |
| 4 ms | 5 / 3 595 | 0.14% |
| 6 ms | 1 / 3 599 | 0.028% |
| 8 ms | 7 / 14 392 | 0.049% |

It is a **dose-response that reaches parity**. 1 ms runs 8x the baseline and has reproduced at
0.33% three times, on two hosts of very different quietness — that regression is real. 2 and 4 ms
sit ~3x baseline on counts small enough to be worth little (p≈0.06 and p≈0.02). 6 and 8 ms are
indistinguishable from not holding the frame at all, over 18 000 frames of held arm.

What the extra milliseconds pay for is everything mutter gets and we do not: the display's vblank
duration, and a hardware deadline timer where ours is an event-loop timer whose wakeup on this VM is
itself worth milliseconds. That is also why the number is so much larger than mutter's 1 ms.

The cost is the latency the feature exists for. Releasing at `vblank − (estimate + 8 ms)` starts the
frame ~5 ms after the previous presentation instead of immediately — a fraction of the freshness a
1 ms margin samples, though still ahead of dispatching now. Whether that residue is worth a code
path cannot be settled with frame-perf, which has no input-to-photon number. **That measurement is
the prerequisite for turning this on**, not another drop-rate sweep.

**The trap that nearly buried this.** The second sweep inherited a session the first had left
*holding* frames, while its own state tracker assumed the shipped default — so every toggle landed
backwards and all eight arms were inverted. Read naively it said the dose-response was noise (six
drops in a "baseline" block that was really an 8 ms held block). The journal's
`deadline dispatch is now …` lines were the only ground truth, and they are what the table above is
built from. `msg frame-perf` now reports `Dispatch:` and the live margin for exactly this reason: a
block that labels itself from what the script believes it set is one dropped toggle from inverting
its own conclusion. **Take the arm from the sample, never from the script.**

So the acceptance criterion at the bottom of this page — "the miss count must not regress" — is not
met, and `SYNOIK_DEADLINE_DISPATCH=1` (or `debug-toggle-deadline-dispatch`) is now what turns it on
rather than off. The code stays because the sweep above shows the cause is **calibration, not
structure** — a release at `vblank − (estimate + 1 ms)` is too tight on this stack, and widening the
margin buys the drops back in full. `debug-set-render-time-margin <ms>` re-runs the sweep without a
rebuild or a relogin.

Two things the runs cost, worth not re-learning: a **backgrounded VT renders at ~1 fps** and
frame-perf keeps counting regardless, so a seat timing run is void unless `loginctl` says
`Active=yes` (two full A/Bs were thrown away to this); and `Late presentations` is **not** comparable
across the arms, because an immediate-dispatch frame carries a `reachable()`-advanced target and
absorbs silently the same slip a held frame reports. Judge on the gap histogram.

### What is a real bug here

`Synoik::redraw` freezes the animation clock at a target it then systematically misses:

```rust
let target_presentation_time = state.frame_clock.next_presentation_time();
self.clock.set_unadjusted(target_presentation_time);
```

`FrameClock::next_presentation_time` returns the *next* vblank boundary unconditionally. In this
population we cannot reach it, so every such frame samples its animations ~16.67 ms stale. Mutter
does not have this problem: it pushes the target forward to one it can actually hit,

```c
while (next_presentation_time_us - min_update_time_estimate_us < now_us)
  next_presentation_time_us += refresh_interval_us;
```

and, on the dispatch-now path, flags the result `is_target_presentation_time = FALSE` — an explicit
"this is best-effort, not a promise". So the structural fix following mutter is **not** deadline
dispatch. It is two things:

1. **Pick an achievable target** *(landed)*. `FrameClock` now tracks what recent frames cost —
   mutter's two-tier maximum, a short-term max that rises at once and a long-term max that decays
   by halves once a second — and advances the target past vblanks that cost cannot reach, bounded
   at two cycles of advance. The bound matters: aiming further out on one catastrophic frame would
   jump every animation ahead of what renders, which reads worse than the miss it avoids.
2. **Account honestly** *(open)*. A best-effort target that slips is not the same event as a
   promised target that slipped; conflating them inflates the miss count with unavoidable sporadic
   latency. Note this cuts the other way too — with (1) landed, a frame that aims one cycle out
   and hits it is no longer counted late, so miss counts before and after this change are not
   directly comparable.

## Population 3 — the process is not running

1 432 `main loop busy` warnings, and the split inside them is the whole finding:

```
mean 112.47ms total — 7.81ms of CPU, 104.66ms not running (frame CPU 0.22ms)
worst: 3150.38ms outside the frame path (10.58ms of CPU, 3139.80ms not running)
```

85% are not-running-majority; 93% of the aggregate stall time is wall-clock with no CPU behind it.
`blocked = wall − thread CPU` (`frame_log.rs`, `loop_turn_end`), so it covers both deschedule and
blocking syscalls — including major faults. The corroborating numbers, all from the same seat:

- synoik held **153 MB swapped out against 129 MB resident**, with 12 365 major faults;
- its cgroup: `memory.current` 204 MB, `memory.swap.current` 185 MB;
- firefox and its content processes held ~1.5 GB of swap on an 8 GB VM;
- the worst hour (4 178 misses, vs 200–1 300 in the others) is also the worst stall hour (532);
- `ghost::render: forced a foreground re-present (watchdog: suspected stale frame)` fired 173 times
   — a client independently noticing that presentation was late.

`steal 0` in `/proc/stat` is not evidence against host contention; this hypervisor does not report
steal. The swap and major-fault counters are the evidence.

Only 27% of the missed-by-one seconds contain a stall, so this population does not explain
population 2 — they are additive, not the same thing.

### Keeping the compositor resident

The answer is a systemd drop-in on the compositor unit — not a code change. Applied on this seat,
live, without a restart (`systemctl --user daemon-reload` re-applies cgroup properties to a running
unit):

```ini
# ~/.config/systemd/user/org.gnome.Shell@user.service.d/memory.conf
[Service]
MemoryLow=512M
```

`MemoryLow` is reclaim *protection*, not a guarantee: under pressure the kernel reclaims from
unprotected cgroups first and only comes back for this one if it must. That is the right shape here
— the compositor should be the last thing paged out, not a thing that cannot be paged out.

**There is no `madvise` for this.** The obvious reach is a "don't swap this region" hint, and the
kernel has no such advice — `madvise` tunes *reclaim order and readahead*, never residency.
`MADV_WILLNEED` starts readahead on a range, so it can pull pages back, but nothing stops them
leaving again the moment pressure returns; using it as protection is a treadmill. Residency is
`mlock`'s job, and `mlock` is the thing with the hazards below.

Two `madvise` uses would nonetheless be real, if we ever want them:

- **`MADV_COLD` / `MADV_PAGEOUT` on what we know is cold** — the inverse move, and the one that
  actually fits a compositor. Instead of protecting the hot set, volunteer the cold set (a
  populated-once icon or texture staging cache, an overview bake nobody has opened in an hour) so
  reclaim takes that and leaves the frame path alone. This needs a page-aligned region we own
  end-to-end, which today means an arena, not a `HashMap` of `Vec`s.
- **`MADV_FREE` on caches we can rebuild** — tells the kernel it may drop the pages *without*
  swapping them, and we fault them back as zeroes and re-bake. Cheaper than a swap round trip for
  anything reconstructible.

Both share the same blocker, and it is worth stating plainly: `madvise` works on page ranges, and a
general-purpose allocator interleaves our hot structures with our cold ones inside the same pages.
"Protect the important data structures" is not expressible until those structures live in their own
arena. That is a real project, not a flag.

One thing the numbers above do *not* say, and worth keeping straight: `VmSwap` (153 MB) is anonymous
memory only. The 12 365 major faults also include file-backed pages — the binary's own text, evicted
from page cache and re-read from disk — and no amount of anon-swap tuning touches those. `mlock` on
the text segment would; `MemoryLow` helps because it protects the cgroup's page cache too.

Two stronger dials exist and are deliberately not used:

- **`MemorySwapMax=0`** forbids swapping this cgroup outright. It converts memory pressure into an
  OOM kill of the compositor, which on this unit already carries `oom_score_adj=100`. A hard cliff
  in place of a soft one.
- **`mlockall(MCL_CURRENT|MCL_FUTURE)`** is the literal "unswappable". It needs
  `LimitMEMLOCK=infinity` (the unit's limit is 8 MB, far under a 130 MB RSS) or `CAP_IPC_LOCK`, and
  `MCL_FUTURE` makes *every future allocation* unswappable — which turns any leak into unkillable
  system-wide pressure. Given [the VMM exhaustion history](./frame-cost-investigation.md), that is a
  trade this project should not take by default. If it is ever wanted, `MCL_ONFAULT` alongside it
  (or `mlock2(MLOCK_ONFAULT)` on a specific range) is the version to reach for: it locks pages as
  they are actually faulted in rather than pre-faulting and pinning whole mappings, so the locked
  set is the working set. The `RLIMIT_MEMLOCK` requirement does not go away.

`MemoryLow` does not un-swap the 185 MB already out; the pages come back on demand, or on the next
restart.

## The instrument gap

`AUTODUMP_MAX = 20` in `frame_log.rs` is a **lifetime** cap. In this session it was exhausted at
08:06, hours before the worst window at 12:00 — the flight recorder went blind exactly when the
session got interesting, and there is no IPC command to dump on demand. `kill -USR1` saved the run
this time, but that requires knowing the signal exists.

Two follow-ups, neither in the scope that produced this document:

- make the cap a **rate** (per hour) rather than a lifetime total, so a long session stays
  instrumented;
- add a `msg frame-perf dump` so the ring can be taken without signalling the compositor.

## Backlog this leaves

- **The 5.3 ms submit round trip** is now the only lever on population 2, which is 93% of all
  misses. Nothing above makes those frames arrive sooner — (1) only stops them lying about when
  they will arrive.
- The autodump cap and the missing dump IPC, above.
- Honest miss accounting (population 2, item 2) — distinguishing a best-effort target from a
  promised one in the frame log's tallies.
- An arena for reconstructible caches, without which the `madvise` options above cannot be
  expressed at all.

## What is not yet measured

Everything here was read from a session running the *old* code. The three landed fixes have unit
guards but no live number yet: the next long seat run should show the once-a-minute 3–4 vblank
misses gone from population 1, and should be read with the accounting caveat above in mind before
comparing totals.

The ahead-by-one caveat this section used to carry is now mostly closed, and it is worth recording
what it *was*, because the shape recurs. Mutter's target is self-fulfilling because it delays
dispatch to `target − max_render_time`: the frame queues near the deadline and lands on the vblank
it stamped. Advancing the target *without* delaying the flip decouples the stamp from the landing in
the other direction — when the estimate overshoots (a slow-decaying max over a bimodal cost, so it
will) and the frame turns out fast, KMS presents it at the *earlier* vblank with animations sampled
one cycle ahead. In a frame log that shows as headroom going *positive*. Deadline dispatch removes
it in the continuous case by construction. It survives in the dispatch-now cases, which keep the
advance and not the delay — bounded at two cycles, decaying by halves within a second or two of the
slow frame that caused it.

What to watch on the first seat run with deadline dispatch, since it is the change that spends
margin: headroom p50 should tighten from about a full refresh interval toward estimate + 1 ms, the
`aim` histogram should be unchanged or better, and the miss count must not regress. Client pacing
shifts under it, so ghost's re-present watchdog is the canary for having got it wrong.
