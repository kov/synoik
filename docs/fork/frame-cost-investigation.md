# Frame cost on this stack — the investigation

**Written 2026-07-25.** The record of a multi-day pass on "why do frames miss their budget",
from the first instrumentation to the point where the remaining cost is one number we cannot
reach from inside the guest. Read this before touching frame performance again — most of the
obvious hypotheses are already measured and dead, and re-deriving them costs a day.

Companion docs: [`renderer-synchronous-submits.md`](./renderer-synchronous-submits.md) is the
design of the one fix that is left; [`venus-timestamp-gap.md`](./venus-timestamp-gap.md) is why
GPU-side timing is unavailable here; [`venus-cost.md`](./venus-cost.md) is the same material
rewritten as a **handoff to whoever works on the VM/VMM stack** — what is priced by the host, what
it costs us, and what would help most. Send that one outward; keep this one for the guest side.

---

## 1. The headline

On the live seat (`gsrs`, Virtual-1 @ 60 Hz, budget 16.67 ms), a frame's cost is
**one synchronous wait on the scanout submit**, and nothing else that we control:

| | scanout submit |
|---|---|
| animation frames, one per refresh | **12.36 / 12.69 / 13.12 / 13.85 ms** |
| sparse frames (startup, one-offs) | 3.71 / 4.88 / 5.25 / 5.45 ms |
| every *other* submit in those same frames | 0.55 – 1.8 ms |

It does not track content: across those animation frames coverage ranged 1.7–2.0× the output
and draw count 138–173, with no correlation. It tracks **how closely frames follow each
other** — back-to-back ~13 ms, sparse ~4 ms.

~13 ms is about one refresh interval. The most likely reading is that the host compositor
executes Venus's command stream on its own 60 Hz loop and our fence wait absorbs a host vsync.
That cannot be confirmed from inside the guest.

Everything else in an animation frame now fits in ~3.5 ms.

**Fixed, and measured on the seat 2026-07-25** — the scanout submit now hands its fence to KMS
instead of blocking the compositor
([`renderer-synchronous-submits.md`](./renderer-synchronous-submits.md#measured-on-the-seat-2026-07-25)).
12 min of real use with `NIRI_VK_ASYNC_SCANOUT=1`: **zero frames over budget** after the first
three seconds, `submit` p50 11.1 → 0.48 ms, and no presentation penalty. The frame class this
wait created — light 2-submit frames, 376 of 400 over-budget frames in the last pre-change
session — no longer exists.

**The headline has moved, and then moved again.** With the scanout wait gone, what puts a frame
over budget is the other submits — and once they were attributed by site (`984774f7`) and the
frame's first wait separated from the rest (`fd0eda51`), they turned out to be **three unrelated
problems**, not one. §6 item 1.

## 2. What the instrumentation reports

`NIRI_FRAME_LOG=1` (grammar in `src/frame_log.rs`; counters in `niri-vk/src/stats.rs`). A slow
frame prints:

```
frame on Virtual-1 took 17.54ms (budget 16.67ms) — elements 0.15ms collect 3.16ms submit 14.15ms
queue 0.05ms callbacks 0.01ms captures 0.01ms; 98 elements, 138 draws covering 2.0x the output,
2 submits in 0.09ms, waiting 14.97ms (1 scanout in 12.69ms, 1 upload in 2.28ms),
1 bakes in 2.57ms, animating, overview 0.56
```

(Captured before enqueue and wait were timed apart and before submits were attributed
by site, so the parenthetical is reconstructed; everything else is as logged.)

Each field exists because an earlier guess was wrong without it:

- **phases** in run order (elements / collect / submit / queue / callbacks / captures).
- **elements** — the *collected* list, not a redraw count. Damage tracking culls before recording.
- **draws** and **covering N.Nx the output** — shaded fragments as a multiple of output area.
- **submits, and where each came from** — the parenthetical is one entry per submit *site*
  (scanout / dmabuf / offscreen / capture / upload / transition / blur / readback), worst wait
  first. Added once the scanout wait was gone and the remaining 7–27 round trips per frame became
  the cost: a bake, an upload and a blur chain are three different fixes, and until this the line
  could only say how many. The site is the **caller**, not the target — keying on the target
  counted every screencast render and every mid-frame capture flush as "scanout", which is why
  "1 to scanout" used to be an unreliable name for the one submit that costs a refresh interval.
- **enqueue time vs `waiting`** — `vkQueueSubmit` against the fence wait after it. Split because
  the fix under consideration moves the wait rather than removing it, and one number cannot tell
  those apart (`7b5f016d`). A frame that waits for work it did not submit says so.
- **bakes / shaped runs** — the widget path, which used to dominate `collect` and no longer does.
- **N created in X.XXms** — GPU resources built this frame (offscreen render targets, upload
  images, blur chains, dmabuf imports, atlas growth). Neither a submit nor a bake, so nothing else
  in the line can see it — and on Venus every `vkCreateImage`/`vkAllocateMemory` is a synchronous
  host round trip, not local bookkeeping. Added 2026-07-25 because the worst seat frames carried
  ~50ms of `collect` that was neither a fence wait nor a bake. A *reused* resource does not count,
  so this distinguishes "allocates every frame" from "allocated once".
- **missed vblanks** — a frame that arrived a refresh later than the one it was built for, *not*
  a gap in the DRM vblank sequence (see §5).

## 3. Hypotheses tested, in order

Every one of these looked obviously right before it was measured.

| # | Hypothesis | Verdict | The measurement that settled it |
|---|---|---|---|
| 1 | Widget bakes dominate `collect` | **partly** | 22 ms of `collect` was assumed to be a bake; breaking it down showed the bake was 1.9 ms and shaping was the bulk |
| 2 | Text shaping is slow | **yes, and worse than it looked** | the *measure* path shapes too and was uncounted; `date_menu_rect` alone measures the clock 4× per frame |
| 3 | A clock tick redrawing 107 elements is the problem | **no** | 107 elements is the collected list; the frame drew 30 quads |
| 4 | ~85 µs per draw, so batch draws | **no** | bad inference — an idle frame (small damage) was compared to an animating one (full screen) and the difference blamed on draw count |
| 5 | Instanced draw batching is the next lever | **no** | fixing 16 draws / 1 submit at 4K and shrinking *only* the damage rect: 1.95 → 1.03 → 0.39 → 0.38 ms. Cost collapses to the submit floor, so cost = fragments shaded, not draws issued. Real per-draw overhead ≈ 5 µs (128 full-screen solid quads cost 0.6 ms) |
| 6 | Overdraw is ~30× | **no** | instrumented it: the real scene covers **1.1–2.3×** the output |
| 7 | The 1 Hz repaint is a wasteful client poke | **no** | `send_frame_callbacks_on_fallback_timer` is throttled to surfaces overdue by >995 ms (occluded ones) and queues no redraw. The 1 Hz repaint is the clock with seconds enabled — legitimate, GNOME does the same |
| 8 | The dropped-frame count means stutter | **no, it was my bug** | it counted DRM vblank *sequence gaps*, which measure idleness on a damage-driven compositor. An idle desktop reported "dropped 59 frames" once a second. Fixed in `b4a613ec` to compare actual vs target presentation time |
| 9 | Venus timestamp queries can attribute GPU time | **no** | they resolve *available* with value 0 — no error to branch on. See [`venus-timestamp-gap.md`](./venus-timestamp-gap.md) |
| 10 | The scanout submit is the cost | **yes** | §1 |

**The pattern worth internalising:** four separate content hypotheses (bakes, draws, fill,
overdraw) were each plausible, each measured, each dead. The cost was never in what we draw.

## 4. What landed

Instrumentation:

- `cdfaaf12` log frames over budget · `3d38a67b` phases in run order · `1b2442c4` GPU timestamps
  (unusable here) · `b4a613ec` missed-deadline metric · `361da67e` bake/shape split ·
  `844c02d6` submit/draw/shape counters · `c5ab83ac` shaded fragments · `25b9ed2f` scanout
  submits counted apart.

Fixes, each removing a GPU round trip that did not need to exist:

- `1020cd4f` — the bake's layout transition rides its own submit instead of a second one.
- `bdec0c84` — same for **every** offscreen render, driven off the target kind rather than an
  opt-in. `finish_sampleable` retired; a `VkFramebuffer` knows whether it is offscreen.
- `6da5f9a4` — memoized shaped runs and measured widths.
- `79def103` — **one persistent glyph atlas** instead of one image per string. Glyphs rasterize
  once and stay; a clock string of resident digits costs **0 submits and 0.011 ms**, against
  1 submit and 0.5 ms cold.
- `ea163e58` — a clock tick stops dropping every uploaded status icon.
- `f2300f36` — the bar background leaves the chrome bake, so an overview fade reuses one bake.

Effect on an idle overview frame (the 1 Hz clock repaint):

| | before | after |
|---|---|---|
| total | 31.3 ms | **under budget, no longer logged** |
| `collect` | 23.4 ms | ~2–8 ms |
| shaped runs | uncounted | 2 in ~4.2 ms → then ~0 |

Several consecutive 10 s windows now report **zero frames over budget** at idle. Every
remaining slow frame is an overview animation or first-time startup work.

## 5. Traps worth not re-learning

- **A damage-driven compositor must not measure dropped frames from the vblank sequence.** The
  hardware counter advances every refresh whether or not we flipped, so gaps measure idleness.
- **Element count is not a redraw count.** It is the collected list; damage culls afterwards.
- **A bake's cost is pixel-independent.** 1920×1080 costs what 220×32 costs — it is the round
  trip. So shrinking a bake buys nothing; *removing* one buys everything.
- **Perf regressions here are invisible to pixels.** A re-baked bar, a re-shaped label and a
  re-uploaded atlas all render identically to the cached version. Tests for this work must
  assert on the counters (`niri_vk::stats`, `frame_log::bakes()`), not on pixels.
- **Verify a mutation actually applied.** A `str.replace` that silently matched nothing once
  "confirmed" a vacuous test for me; the mutation must be asserted to have changed the file.
- **A counter those tests assert on must be per-thread.** Same reason: the assertion is
  "*this* repaint cost nothing", and libtest runs tests in parallel against their own renderers,
  so a process-wide counter folds a neighbour's bake or submit into your delta. It fails about
  one run in five under `NIRI_VK_VALIDATION=1` and names the innocent test in front of you
  (`917c69b0`).
- **An A/B on the live seat does not need a restart, and comparing two sessions is not an A/B.**
  Session-to-session numbers are dominated by how much the desktop was driven — the same build
  reported 0.8 and 12.3 over-budget frames/min depending on the session. `debug` options are read
  per frame out of the live config and niri re-reads the file on save, so the control can run in
  the *same process* minutes later: writing
  `debug { wait-for-frame-completion-before-queueing }` into `~gsrs/.config/niri/config.kdl` is
  what turned "the deferred run looks better" into an attributable 13.33 ms.
- **First-entry costs are not steady-state costs.** Opening the app grid the first time is
  ~40–70 ms of icon uploads (~20 submits). It caches. Do not optimise it by mistake.

## 6. Open items

> **The submit-side items below are largely done, and what came out of them is a rule:**
> [`frame-submit-discipline.md`](./frame-submit-discipline.md) — share the frame's submit, never
> give frame-path work a submit of its own. It also carries the remaining follow-ups (folding the
> two blur paths into the frame's command buffer, and what that would unlock for
> `make_sampleable`). Read it before adding anything that submits.

1. **The other submits — measured 2026-07-25, and they are three problems, not one.** Attributed
   by site on the live seat, with the frame's first wait separated out. What the data killed and
   what it found:

   **Dead: the wait did not move into the next frame.** Every submit is chained on the queue
   timeline, so the plausible reading was that a frame's *first* submit drains whatever the
   previous frame left running, and whichever site went first merely looked expensive. Measured,
   `first` is **1.4–2.4 ms in six of seven frames**. The chain is not costing us a frame's tail,
   and the structural work (dependency-accurate ordering, frames in flight) is **not urgent** for
   this reason. It was a good hypothesis and it was wrong; do not re-derive it.

   **Real, in order of how much they cost:**

   | what | evidence | why it costs | the fix |
   |---|---|---|---|
   | ~~blur execution~~ **wrong — see below** | `2 blur in 42.38ms` + `7 offscreen in 28.81ms` on a 132 ms frame covering **8.2× the output** | ~~fill rate, not round trips — ~21 ms per blur submit~~ | ~~fewer/cheaper passes, or cache the blurred result~~ |
   | one huge upload | `first upload 18.62ms`, **48.0 MiB uploaded** in one frame | payload. The wallpaper. Deferring the fence moves it, it does not remove it | off the frame entirely — the async-decode pattern (`e7a1c2ed`) extended through the *upload* |
   | uncoalesced round trips | `13 glyphs in 13.43ms`, `10 shm in 15.85ms`, `9 upload in 14.67ms` — all in single frames | ~1 ms each, and they go into the **same image** | coalesce at the call site. `absorb_glyphs` (`renderer.rs:792`) uploads per *shaped line*, though `upload_coverage_regions` already takes a slice of regions |

   **The blur row was wrong, and it was wrong in an instructive way (2026-07-25).** Measured
   directly — `cargo test -p niri-vk blur_cost -- --ignored --nocapture`, seven source sizes from
   2.07 to 43.9 Mpx — the default 3-pass chain costs **0.16 ms + 0.092 ms per megapixel**, linear
   across a 21× range. That frame shaded 8.2× the output in total and a blur shades 1.66× its own
   source, so two blurs at a combined 3.3× the output puts each source at roughly output size:
   ~0.92 ms of work, charged 21 ms. Even at 43.9 Mpx a blur is 4.20 ms.

   What the row actually measured is a **fence wait**, which on this stack is not a cost
   attribution: it carries everything queued ahead of it on an in-order queue, plus milliseconds of
   the guest's polling tax (`venus-cost.md` §9.4, §11.2). Run the same benchmark at one blur per
   submit and the readings stop being monotonic in source size — that is the noise, made visible.
   Before attributing a cost to a site because its wait is long, price the work directly.

   The glyph one is the clearest: a frame that shapes 13 new strings makes 13 submits into one
   atlas. Flushing once per frame instead is a call-site change, not a scheduling change.

   Deferring these submits — [`renderer-synchronous-submits.md`](./renderer-synchronous-submits.md)
   slices 1–4 — is therefore **not** the top lever any more. Coalescing removes the round trips
   outright. (The original sentence ended "and the two big costs are real GPU work that no amount
   of scheduling makes cheaper". With blur withdrawn, only the wallpaper upload is left standing —
   and that one is payload, which is why it moved off the frame rather than being made cheaper.)

2. **The panel still bakes during an overview animation** — but not for the reason we fixed.
   `are_animations_ongoing()` (`ui/panel.rs:848`) is the button-fill fades, and opening the
   overview toggles Activities to checked, so the fill fade covers the same window. That bake
   is a genuine content change and is correct; `f2300f36` bought nothing on the seat, though it
   is a correct layer separation and is tested.
3. **First app-grid entry** is ~20 submits of icon uploads. One-time, cached after; batching
   them into one submit would be a real but narrow win.
4. **Idle `collect` outliers.** One idle frame showed `2 shaped runs in 26.39 ms` where every
   other was 3.9–5.1 ms. One sample, no pattern. Watch, don't chase.

## 7. Picking this back up

1. `NIRI_FRAME_LOG=1` is set for the gsrs session via
   `/home/gsrs/.config/environment.d/91-frame-log.conf`, and `NIRI_VK_ASYNC_SCANOUT=1` via
   `92-async-scanout.conf` (the deferred scanout submit —
   [`renderer-synchronous-submits.md`](./renderer-synchronous-submits.md)). `systemd --user` only
   reads `environment.d` at start or `daemon-reload`, so a plain logout/login may not pick up a
   change.
2. Read frames with `journalctl _UID=$(id -u gsrs)`; filter on `frame on`. Sessions are
   delimited by the `frame logging on:` line.
3. `cargo test` does **not** rebuild `target/debug/niri`. After a code change,
   `cargo build --bin niri` and relog, or the seat keeps running stale code.
4. Renderer changes: `NIRI_VK_VALIDATION=1 cargo test --workspace`, and trust the **exit
   status**, not the `test result: ok` line.
