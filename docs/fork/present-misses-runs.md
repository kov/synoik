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
