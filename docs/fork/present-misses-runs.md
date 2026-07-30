# Run ledger for the present-miss measurements

Companion to `docs/fork/present-misses.md`. That document argues; this one records **which runs the
numbers came from**, so a later comparison does not have to re-derive the arms by inference.

It exists because that inference was attempted once and was nearly wrong: the pre-0051 arm in §19 was
originally located by guessing driver timestamps out of a docstring and widening the range until the
shape looked right. It happened to land correctly (the slice covered all five runs plus two idle
windows) but it was luck, and the run count in the first draft of §19.1 was wrong by one.

**The journal is persistent** (`/var/log/journal`), so every run below is still recoverable:

```
journalctl -b -1 --no-pager -o short-iso --since "..." --until "..." > arm.log
scripts/correlate-frame-log.py arm.log --labels before
```

Boot indices shift as boots accumulate — match on the timestamps, and confirm with
`journalctl --list-boots`. Boot `-1` below is `e9fa4abb` (2026-07-27 11:24:39 → 16:06:31); boot `0`
is `5b096dbf` (16:07:22 →).

## Pre-0051 (boot e9fa4abb, 2026-07-27)

Guest binary: `b7fb8b76`-era `target/debug/niri`, built 11:50. `NIRI_FRAME_LOG=all,gpu`,
`NIRI_VK_ASYNC_SCANOUT=1`, both from `~gsrs/.config/environment.d`.

| when | what | notes |
|---|---|---|
| 12:15-12:17 | hand-poking | light-to-medium |
| 12:21:30-12:22:30 | hand-poking | **heavy** — draws 200-268, gpu 6.4-8.5 ms, 9-26% miss |
| 12:24:10 | `touch /tmp/disable-limina-fence-present` | fence-present OFF from here |
| 12:24:15-12:25:30 | hand-poking | **heavy** — the §15/§16 OFF arm |
| 12:38:12-12:40:02 | `drive-workload.sh` | mixed profile |
| 12:47:32-12:49:42 | `drive-workload.sh` | mixed profile |
| 12:50:21-12:52:31 | `drive-workload.sh` | mixed profile |
| 12:54:34-12:56:46 | `drive-workload.sh` | **§16 ARM-OFF** (fence-present disabled) |
| 12:58:21-13:00:33 | `drive-workload.sh` | **§16 ARM-ON** (fence-present re-enabled) |
| 15:29, 15:53 | hand-poking | tail of the session |

The five driver runs pool cleanly as the pre-0051 scripted arm: §16 found no fence-present effect, so
ARM-OFF and ARM-ON are not separate populations for this purpose. Slice `12:35:00`-`13:01:00` to get
all five (the extra windows are idle and drop out under `--min-aim1`).

**Every heavy number in §17 and §19.2's before column is hand-driven, not scripted.** The mixed
profile cannot produce a window whose *median* draw count exceeds ~55 — see §19.2. This is the single
biggest weakness in the before/after and the reason `PROFILE=heavy` now exists.

## Post-0051 (boot 5b096dbf, 2026-07-27)

Same guest binary, unchanged — nothing in `src/` moved between the arms. Host gained virgl `0051`
(two-lane journal) **and** `0049` (ring-relax ladder v2); the guest cannot separate them.

| when | what | notes |
|---|---|---|
| 16:18:25-16:20:41 | `drive-workload.sh` run 1 | **DISCARDED** — a stray click landed in it, and a commit hook ran `cargo clippy` inside it |
| 16:20:41-16:29:45 | `drive-workload.sh` runs 2-5 | the scripted after arm |
| 16:32:21-16:34:24 | ad-hoc: held overview, then held grid | **negative result** — a settled UI is static, flat 43 draws / 1.66 ms; holding a UI open does not make a heavy window |
| 16:35:04-16:36:14 | ad-hoc: rapid workspace alternation | draws 69-98 |
| 16:37:01-16:38:12 | ad-hoc: rapid overview toggling | draws 239-240 |
| 16:40:11-16:42:34 | `drive-workload.sh ... heavy` | the same two phases, through the committed tool |

## Results

Scripted, like-for-like, matched draw range (§19.1):

| draws | before flips | before | after flips | after | ratio |
|---|---|---|---|---|---|
| 0-40 | 28 860 | 0.57% | 12 275 | 0.05% | 11x |
| 40-60 | 4 548 | 0.90% | 11 563 | 0.42% | 2.1x |
| all | 33 408 | **0.62%** | 23 838 | **0.23%** | **2.7x** |

The after arm ran the heavier desktop (elements p50 159 vs 77, gpu p50 2.92 ms vs 1.19 ms) because
the session was restored to a later state, so 2.7x is a floor.

Heavy, cross-workload (§19.2) — before is hand-driven, after scripted, matched only on draw count:

| region | before | after (ad-hoc) | after (`PROFILE=heavy`) |
|---|---|---|---|
| draws 60-130 | 7.31% (183 / 2 502) | 0.10% (4 / 4 025) | 0.05% (2 / 4 118) |
| draws 200+ | 14.15% (673 / 4 755) | 3.04% (106 / 3 488) | 1.11% (46 / 4 133) |
| draws 200+, warm | 4.92% (55 / 1 117) | 0.67% (12 / 1 788) | 0.67% |

Both arms warm within a run (after: 10.61% → 0.67% at a flat 240 draws as gpu settled 9.00 → 5.13 ms;
before shows the same shape, 25% → 4.5%), so the warm-only row is the fairest single comparison.

## What is still open

- **`VKR_JOURNAL` on/off within one boot on the post-0051 build.** Agreed as the clean attribution
  arm — it is the only way to separate 0051 from 0049 from the guest, and the confound it has to beat
  is now ~10x smaller. Host-side knob. **Parked 2026-07-27 by choice, not by blocker:** the absolute
  miss rate is low enough that further attribution work has poor returns for now.
- **§18's pre-registered correlation test is inconclusive**, not confirmed — 56% of post-0051 windows
  have zero misses, so a rank correlation on a mostly-tied variable says nothing. If it is ever
  revisited, it needs a workload that holds the miss rate well off zero, which `PROFILE=heavy` can
  now produce.
- **The residual.** The scene-cost gradient survived 0051 (0.05% → 3.04% across the draw range). The
  level dropped roughly an order of magnitude; the shape did not.

## New VMM (boot a8a1fbce, 2026-07-29)

First measurement on the newly deployed VMM ("brings some more performance fixes"). Same display
mode (3840x2160, pixel clock 583400), same seat environment (`environment.d` files all predate
the post-0051 arm; `VN_PERF` already absent for both), virtio-gpu feature flags identical across
boots, `/tmp/disable-limina-fence-present` absent on both.

**NOT the same guest binary** — the first version of this entry claimed it was; wrong. The arms
are ~30 `src/` commits apart (old arm `b808c5bb`, new arm `d4c7a61d`, built 13:42), most of them
overview visuals (doubled thumbnail strip + glow shadows, adaptive chrome, preview icons). The
binary A/B that closes this confound is §21.5 of `present-misses.md`.

| when | what | notes |
|---|---|---|
| ~14:05 | first mixed arm | **DISCARDED** — gsrs VT inactive (kov held tty2); DRM paused, summaries show bogus ~330 fps / p50 0.00 ms with **no `aim` clause**. That signature = nothing was rendering; check `loginctl … Active` before trusting an arm |
| 14:24 | populate | 8 windows across ws1/ws2 to match the post-0051 desktop (elements p50 159) |
| 14:26:55-14:38:35 | `drive-workload.sh` runs 1-5 (mixed) | VT confirmed active |
| 14:38:40-14:41:07 | `drive-workload.sh … heavy` run 1 | cold |
| 14:41:12-14:43:39 | `drive-workload.sh … heavy` run 2 | warm |

## Results vs post-0051

| slice | post-0051 | new VMM | ratio |
|---|---|---|---|
| mixed, all | 0.23% | 1.26% | 5.5x worse |
| mixed, draws 0-40 | 0.05% | 0.28% | 5.6x |
| heavy, all | 0.58% | 7.48% | 13x |
| heavy, draws 200+ | 1.11% | 17.16% | 15x |
| heavy, draws 200+, warm only | 0.67% | 21.42% | 32x |
| matched gpu band 6-12 ms | 1.11% | 12.22% | 11x |

No warmup story: the warm-only run is *worse*, not better. The regression is a level shift across
the whole draw/gpu range, largest at the heavy end.

**RESOLVED same day — it is both; see the A/B section below and `present-misses.md` §21.6.**

**Queue timing rules out a slow CPU-side guest, and no more.** Of the new VMM's misses,
1263/1264 were queued an average of **15.13 ms EARLY**; post-0051 had the identical shape (49/50
at 15.63 ms early), just ~25x less often. Lateness is the same shape on both arms: p50 16.7 ms,
max 33.3 ms — exactly 1-2 vblanks. But under `NIRI_VK_ASYNC_SCANOUT=1` the flip is queued before
the render fence signals, so an early queue does not prove the pixels were ready — and the new
binary's scenes ARE heavier (heavy gpu p50 7.16 ms vs 5.53 ms). **Conclusion: OPEN, not
attributed.** Host-side present regression and guest-side "new overview work × async scanout"
both fit; the `b808c5bb` binary A/B on this boot (present-misses.md §21.5) decides it.

## Binary A/B on the new VMM (boot a8a1fbce, 2026-07-29, 15:09-15:14)

`b808c5bb` (the §19 arm's commit) rebuilt against smithay `e1c10415` (also pinned — the fork
gained the rescale-rounding patch `83175a56` after the §19 arm), seat restarted onto it at
15:07:12, VT verified active. Built via paired worktrees under `~/Projects/ab-b808/` with
`CARGO_TARGET_DIR` pointed at the main repo so the binary landed at the seat's `ExecStart`
path; the `d4c7a61d` binary saved as `target/debug/niri.d4c7a61d`.

| when | what |
|---|---|
| 15:08:56-15:09:16 | populate: 8 kitty windows across ws1/ws2 |
| 15:09:16-15:11:43 | `drive-workload.sh 1002 1 heavy` run 1 (cold) |
| 15:11:48-15:14:15 | heavy run 2 (warm) |

Result (heavy): overall 4.08%, draws 200+ 6.98%, gpu 6-12 ms 4.24%; warm run *decays* to 3.24%
(the new binary's warm run got *worse*, 21.42%). Misses still queued ~15.5 ms early (657/659),
presented 1-2 vblanks late. **Verdict: both effects are real — VMM ~4-6x with the binary held,
our new overview work ~2.5x on top with the VMM held, and the warm-up anomaly is entirely
ours.** Full decomposition in `present-misses.md` §21.6.

## Scale sweep, old binary on the new VMM (boot a8a1fbce, 2026-07-29, 15:21-15:34)

Operator-suggested confound close: the Jul 27 arm likely ran at scale 2 (unrecoverable from the
journal; his recollection), the §21.6 A/B arms at 1.5. Same `b808c5bb` seat process (up since
15:07), same 8 kitties, operator flipped the scale between sweeps via display settings
(the `ApplyMonitorsConfig` persist lines in the journal mark the flips).

| when | what |
|---|---|
| 15:21:49-15:26:48 | heavy x2 at scale 2 |
| 15:29:03-15:34:02 | heavy x2 at scale 1 |

Result (200+ draws / gpu 6-12 ms / overall): scale 2 → 5.77% / 5.03% / 2.82%; scale 1.5
(§21.6) → 6.98% / 4.24% / 4.08%; scale 1 → **21.20% / 18.42% / 9.55%**. Scale does not explain
the VMM leg (matched scale 2: 1.11% → 5.77%), and scale 1 misses 3-4x more on the *cheapest*
frames of the sweep (gpu p50 6.05 vs 7.34/8.05 ms) — the miss driver on this VMM is not guest
GPU cost. Analysis in `present-misses.md` §21.7.

## Stationarity hold, new binary (boot a8a1fbce, restart 15:45, 2026-07-29)

Seat restarted onto `d4c7a61d` (scale restored to 1.5 by monitors.xml). 8-kitty populate, then
one uninterrupted 4-minute overview hold (continuous Super_L toggling + pointer churn),
15:47:53-15:51:54, pid 2289737, RSS sampled every 10 s.

Result: no guest-side accumulation (RSS/draws/elements/bakes all flat); misses arrive in
10-30 s episodes (gpu p90 ~10 → 13-15 ms, rate 8-9% floor → 35-50%) with 20-40 s gaps. The
"warm run worse" fingerprint from the morning cells was these episodes under-sampled — claim
withdrawn in `present-misses.md` §21.2/§21.4.

## Async-scanout A/B, new binary (boot 2dd2ea15, 2026-07-29, 16:22-16:28)

Post-wedge reboot; seat auto-started on the same `d4c7a61d` binary (built 13:42), scale 1.5,
`NIRI_VK_ASYNC_SCANOUT` commented out of `environment.d` → async OFF. 8-kitty populate,
heavy x2 (16:22:58-16:25:25, 16:25:30-16:27:57).

Result: async OFF is worse — overall 11.92%, draws 200+ 26.67% (vs 7.48% / 17.16% async-on,
same binary, morning cells). Miss character inverted as the mechanism predicts: 1814 misses
queued LATE / ~0 ms early (vs "15.5 ms early" under async-on) — the CPU parks on the slow fence
before queueing instead of after. Async scanout exonerated and restored to on
(`present-misses.md` §21.5). Note the frame log's gpu/draws accounting differs between the two
modes (unwaited vs waited GPU), so only the miss rates compare, not the cost columns.

Perceptual side-note on the same arms (operator, watching live): async OFF looked *smoother*
despite the higher miss rate. Confirmed in the cadence — overview phases, 5 s bins: async on =
fps 49.1 ± 9.4 (min 14), async off = 47.3 ± 3.5 (min 41). The miss metric counts late fences,
not perceived smoothness; see the metric caution in `present-misses.md` §21.5 and the queued
frame-pacing design item.

## Clean deploy (boot 68fa5075, 2026-07-29, 16:47-17:04)

The morning's VMM was a fumbled deploy carrying host debug instrumentation (operator's
diagnosis); this is the clean one. Guest binary `2abfa499` (= §21 binary + §22 hardening),
async on, scale 1.5, 8-kitty populate, VT active.

| when | what |
|---|---|
| 16:47:16-16:58:48 | `drive-workload.sh` mixed x5 |
| 16:58:51-17:01:18 | heavy run 1 |
| 17:01:23-17:03:50 | heavy run 2 |

Result: mixed 0.15% overall / 0.05% at 0-40 draws; heavy 0.04% overall / 0.07% at 200+ (6
misses in 16 453 flips) / warm 0.01%. Beats post-0051 by ~15x at the heavy end; episodes gone;
remaining misses queue ~15.5 ms early (the §19 benign shape). Full reading:
`present-misses.md` §23.

## Bridge-fix deploy verification (boot 3cac48f9, 2026-07-29, 20:37-20:54)

New deploy to fix the explicit-sync bridge regression (sync_spike::explicit_sync_bridge — now
PASSES, pipelined, 0.02 ms pending export). Guest binary `657f5c57-modified` (§23 binary + Q7
bluetooth, nothing on the frame path), async on, scale 1.5, 8-kitty populate, VT active.

| when | what |
|---|---|
| 20:32:17-20:38 | DISCARDED pass (host-side stuck Shift ate the driver's Super presses) |
| 20:37:52-20:49:24 | `drive-workload.sh` mixed x5 (clean) |
| 20:49:27-20:54:27 | heavy x2 (clean) |

Recover with `journalctl -b --since 20:37:52 --until 20:54:27 -o short-iso _UID=1002` on boot
`3cac48f9`; score with correlate-frame-log.py. Result: mixed 3.91% (0-40 draws 2.72%), heavy
14.45% (200+ draws 28.90%, 12ms+ gpu band 81.61%) — ~26-200x worse than §23; all misses queued
early (median 15.5/13.4 ms), the late-fence shape. Written up as `present-misses.md` §24.

| 23:33-23:44 | client A/B (§25): same boot `3cac48f9`, close-all → 8× kitty heavy ×2 (23:34:07-23:39:04) → close-all → 8× gnome-terminal heavy ×2 (23:39:29-23:44:26). kitty 12.59% / 200+ 28.31%; gnome-terminal 16.28% / 200+ 38.98%; both all-queued-early (median ~15 ms). Regression is client-independent; kitty adds a second symptom (12 ms+ gpu band + fence-gated commits). |

| 00:03-00:08 (2026-07-30) | fresh-boot control (§26): boot `6eae47e5`, 8× gnome-terminal as first-ever workload (no GPU client this boot), heavy ×2 00:03:28-00:08:25. 7.17% overall / 200+ 15.42%; 1175 misses all queued early (median 15.3 ms). A first attempt 23:58-00:04 was DISCARDED (mouse experimentation on the seat mid-pass). |
