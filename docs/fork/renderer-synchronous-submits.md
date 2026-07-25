# The synchronous-submit ceiling

**Status:** open, deferred by decision (2026-07-25). Deliberately not attempted alongside the
frame-cost work in `844c02d6`…`bdec0c84`; that work removed *wasted* round trips, and this is
about the cost of the ones that remain.

**One-line summary:** every `vkQueueSubmit` in the owned renderer is immediately followed by
`wait_for_fences(…, u64::MAX)`. The CPU blocks until the GPU drains, every time, so a frame's
cost is dominated by how many round trips it makes rather than how much it draws — and the
floor is set by round-trip latency we never overlap with anything.

## What the code does today

`VulkanFrame::finish_internal` (`src/render_helpers/vulkan/frame.rs`) ends the command buffer,
submits it, waits on the fence, then frees the buffer. `Gpu::run_commands`
(`niri-vk/src/gpu.rs`) does the same for every one-shot: texture uploads, glyph-atlas uploads,
dmabuf import transitions, blur chains, readbacks, layout transitions. The returned `SyncPoint`
is already signalled, because the wait happened inside.

This was a deliberate simplification when the renderer was built — it makes resource lifetime
trivial (nothing is ever in flight, so nothing needs deferred destruction) and it is why
`niri-vk` has no frame-in-flight bookkeeping at all.

## Why it is the ceiling

Measured on this VM (Venus over virtio-gpu, host Apple M4 Pro): a submit costs roughly
0.7–2 ms **regardless of its contents**. An empty command buffer costs about what a 1920×1080
render pass does. A composited overview frame draws ~30 quads and spends almost all of its time
in round trips, not drawing — which is why "116 elements" was never the problem and why
`GPU timing` would not have told us much either (see
[`venus-timestamp-gap.md`](./venus-timestamp-gap.md); the GPU-side number is unavailable here
anyway).

Two consequences worth writing down:

- **Frame cost scales with round trips, not work.** Adding draws to an existing pass is nearly
  free; adding a pass is expensive. Every optimisation so far has been "make this stop being a
  separate submit", and each one paid.
- **Nothing overlaps.** The CPU cannot build frame N+1 while the GPU finishes N, and the GPU
  idles while the CPU builds. On a virtualized stack, where the round trip is mostly latency
  rather than execution, that idle time is most of the budget.

## What fixing it involves

Not a local change. Removing the fence wait means the renderer must track work in flight:

1. **Deferred destruction.** Every resource a submit references — command buffers, staging
   buffers, descriptor pools, images whose owning `Arc` may drop mid-flight — has to outlive
   the submit. Today the wait guarantees that for free; `RunGuard` in `niri-vk/src/gpu.rs`
   frees on scope exit precisely because nothing can still be reading.
2. **A real `SyncPoint`.** Smithay's `SyncPoint` is designed for this (it can carry a fence),
   and the compositor already threads sync points through the KMS path. `finish` would return
   an unsignalled one, and explicit sync (`docs/fork/explicit-sync.md`) is already wired for
   the client-facing half.
3. **Per-frame resource pools.** Command buffers and descriptor sets become per-frame-slot with
   a small ring, reset when that slot's fence signals.
4. **A decision about `run_commands`.** Most of its callers genuinely want the result
   immediately (a readback, a measurement). Uploads do not. Splitting it into "queue this" and
   "queue this and wait" is probably the first concrete step, and is useful on its own.

Item 1 is the bulk of the work and the only part with real correctness risk: a use-after-free
here is a GPU fault or silent corruption, not a panic. The validation layer catches a lot of it
(`NIRI_VK_VALIDATION=1`, see CLAUDE.md) but only for paths a test actually exercises.

## When to do it

Not while the cheaper wins remain. The pattern so far is that each frame had round trips that
did not need to exist at all, and removing one is a contained change with a measurable result:

- `1020cd4f` — folded the bake's layout transition into the bake's own submit.
- `bdec0c84` — same for every offscreen render, driven off the target kind rather than an
  opt-in.
- `6da5f9a4` — cached shaped runs, so an unchanged label stops rebuilding its atlas (and the
  upload submit behind it) every frame.

Revisit this document when a frame's remaining submits are all load-bearing. The frame log's
submit counter (`niri_vk::stats`, reported as `N submits in Xms`) is how to tell: when that
count stops falling and the time stays high, the waits are what is left.

### Where the live seat stands (2026-07-25, gsrs, Virtual-1 @ 60Hz, budget 16.67ms)

Measured before and after `6da5f9a4`…`bdec0c84`, overview open. The clock shows seconds, so the
idle case is a genuine 1Hz repaint, not an artefact.

| overview, idle (clock tick) | before | after |
|---|---|---|
| total | 31.3 ms | ~19 ms |
| `collect` | 23.4 ms | ~8.6 ms |
| `submit` phase | 7.3 ms | ~10 ms |
| shaped runs | (uncounted) | 2 in ~4.2 ms |
| submits | (uncounted) | 3 in ~10.4 ms |
| vblanks missed per hitch | 1.60 | 1.14 |

The diagnosis inverted. `collect` — the CPU widget path — went from most of the frame to a
minor part of it, and **submit is now essentially the whole cost**. On an overview *animation*
frame the split is starker still: `collect` ~2 ms, and 2 submits accounting for ~15 ms of a
17–19 ms frame, with ~159 draws.

Two numbers to read off that:

- **~3.5 ms per round trip** on the idle frame (3 submits, ~10.4 ms), against ~0.7–2 ms measured
  headless. Real scene, real scanout target, same order.
- **~30–60 µs per draw**, from the idle/animating pair (53 draws vs 159, ~3.5 vs ~7.5 ms per
  submit). That is not GPU shading cost; on Venus every draw is encoded into the ring and
  replayed host-side. Batching quads into instanced draws is the lever there, and it is a
  separate piece of work from this document — but it lands in the same place: fewer, fatter
  submissions.

So the remaining contained win is *not* here: it is a persistent glyph atlas, which would
retire the per-tick atlas upload submit and the ~4.2 ms of re-shaping. After that, the idle
frame is ~13 ms and under budget, and what is left over is this document.

## Related

- `src/frame_log.rs` — the `NIRI_FRAME_LOG` grammar and what each phase covers.
- `niri-vk/src/stats.rs` — the submit/draw/shape counters.
- [`venus-timestamp-gap.md`](./venus-timestamp-gap.md) — why GPU-side timing cannot separate
  "the GPU was busy" from "we waited" on this VM.
- [`explicit-sync.md`](./explicit-sync.md) — the client-facing sync work, already landed.
- [`renderer-gaps.md`](./renderer-gaps.md) — the other standing renderer limitations.
