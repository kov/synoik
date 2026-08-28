# pacing-sweep — calibrating the frame clock's dispatch dials

Deadline dispatch holds a frame until `vblank − (render estimate + margin)` instead of rendering
as soon as a frame is needed, so its contents are sampled closer to the photons. It can only lose
frames, never gain them: starting later leaves no slack, and the `margin` is what pays for a late
wakeup and for the render-time estimate being wrong. This tool measures that loss.

```
vkcube &                                        # deadline dispatch needs a CONTINUOUS client
tools/pacing-sweep/sweep.sh -c 8 off 1 2 4      # ~32 min, prints the table below
tools/pacing-sweep/report.sh sweep.jsonl        # re-read an earlier run
```

`-u gsrs` drives another seat over sudo; `-o <connector>` pins which output must carry the load;
`-s` names the socket when autodetection cannot. The session needs `SYNOIK_FRAME_LOG` set (`ring,gpu`
is enough) — with logging off every tally is zero, which reads exactly like a flawless session, so
the tool refuses to run rather than print it.

## What it does, and why it is shaped this way

Each *block* sets one condition and differences `msg frame-perf` across it. Conditions are
**interleaved in short blocks** — 8 cycles of `off 1 2 4` rather than one long run each — and the
within-cycle order reverses on alternate cycles. Every design decision below exists because the
straightforward version of this experiment produced a confident wrong answer first.

- **The host imposes a variance regime that steps on its own schedule.** Measured 2026-08-28,
  mid-run, with the guest workload unchanged and our own GPU time flat: scanout-wait p99 fell from
  7–12 ms to 1.6–3.1 ms. At five-minutes-per-arm that step lands on whichever arms straddle it —
  two arms of the *same* condition read 24 and 7 misses. Short blocks dilute it: every condition
  gets a share of every regime. `over_budget` per block is the regime proxy, so balance is
  **checked** rather than assumed.
- **Rates from different runs are not comparable.** The same sweep an hour apart differed 3× in
  absolute rate purely by regime. Only the within-run contrast means anything, which is why the
  report normalises to that run's own `off` arm.
- **The arm is read back from the compositor at both ends of every block.** `debug-toggle-deadline-dispatch`
  is a *toggle*: a dropped flip inverts the label silently, and every number still looks plausible.
- **`over_margin` is reported next to the miss rate.** It is the share of held frames released later
  than that block's own margin — the mechanism, printed beside the outcome it should explain. If a
  margin costs frames while `over_margin` is zero, the wakeup is not what is costing them.
- **The median block is reported next to the pooled rate.** Misses arrive in bursts; a pooled total
  can be carried by one or two blocks. The two disagreeing means the difference is not real yet.

## Two failure modes with no visible symptom

**A condition you did not verify is a condition you did not control.** A 30-minute run was void
because the load never moved to the display under test: `msg action move-window-to-monitor` takes
`--id`, a bare positional is a CLI error, stderr was discarded, and the idle output logged one frame
per block — which reads as a row, not as an error. The sweep now refuses to start unless some output
is presenting continuously, and warns when the busiest output is not the one requested. It cannot
check what it was not told to check, so *state the output*.

**Deadline dispatch does nothing after an idle period.** It short-circuits to immediate dispatch
whenever the next vblank is more than one interval out (mutter's `should_update_now`: "lowest
average latency for sporadic user input"). On an ordinary desktop most frames take that path, so a
sweep against an idle session measures nothing and says so with clean numbers. That is why a
continuous client is mandatory, and why these results do not describe a normal desktop's frames.

## Reading a result

```
arm       blocks  frames  misses  rate      vs_off  med_blk  over_budget  late_mean  P(late>margin)
off       8       57563   12      0.000208  1.0x    2        12           -          -
on@1.0ms  8       57508   36      0.000626  3.0x    4        16           0.037ms    0.0003
on@2.0ms  8       57506   26      0.000452  2.2x    3        15           0.036ms    0.0001
on@4.0ms  8       57532   13      0.000226  1.09x   2        11           0.035ms    0.0
```

Parity with `off` is the best a margin can do. `docs/fork/foundation.md` §3 carries the standing
result and what it means for shipping the feature; §4 carries the wake floor the margin is
calibrated against, and `tools/timer-probe` re-measures that floor — **run it first** after any VMM,
kernel or hardware change, because a floor that moves invalidates every margin taken against it.

## What this tool cannot tell you

Only the cost. The *benefit* — input and animation sampled closer to the photons — is invisible to
`frame-perf`, and needs an input-to-photon measurement with host-side timestamps. A sweep showing
parity says the feature is affordable, never that it is worth enabling.
