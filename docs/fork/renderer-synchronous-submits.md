# The synchronous-submit ceiling

**Status:** open, and now the only frame-cost item left. Deliberately not attempted alongside
the work in `844c02d6`…`f2300f36`; that removed *wasted* round trips, and this is about the cost
of the one that remains. Full evidence in
[`frame-cost-investigation.md`](./frame-cost-investigation.md).

**One-line summary:** every `vkQueueSubmit` in the owned renderer is immediately followed by
`wait_for_fences(…, u64::MAX)`. The CPU blocks until the GPU drains, every time. On the scanout
submit that wait is 12–14 ms of a 16.67 ms frame, it does not depend on anything we draw, and we
never overlap it with anything.

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

Measured on the live seat: the submit that renders into the scanout buffer costs **12.4–13.9 ms**
when frames run back-to-back at 60 Hz, and 3.7–5.5 ms when they are sparse. Every other submit
in the same frames is 0.55–1.8 ms. It does not track coverage or draw count at all — only how
closely frames follow each other.

~13 ms is about one refresh interval, which is what you would expect if the host compositor
executes Venus's command stream on its own 60 Hz loop and our fence wait absorbs a host vsync.
That cannot be confirmed from inside the guest
([`venus-timestamp-gap.md`](./venus-timestamp-gap.md)).

The full evidence, and the four content hypotheses that were measured and rejected on the way
to it, are in [`frame-cost-investigation.md`](./frame-cost-investigation.md). The short version:
everything else in an animation frame now fits in ~3.5 ms of a 16.67 ms budget, and the wait is
the rest.

Two consequences worth writing down:

- **Nothing overlaps.** The CPU cannot build frame N+1 while the GPU finishes N, and the GPU
  idles while the CPU builds. On a virtualized stack, where the round trip is mostly latency
  rather than execution, that idle time is most of the budget.
- **The goal is not to make submits faster.** We probably cannot reach whatever paces them. It
  is to stop *blocking the CPU* on one, so a frame's remaining budget is not spent staring at a
  fence.

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

The condition this document used to set — "revisit when a frame's remaining submits are all
load-bearing" — **is now met**. Every round trip that did not need to exist has been removed
(`1020cd4f`, `bdec0c84`, `6da5f9a4`, `79def103`), the idle frames are under budget, and the
submit counter has stopped falling while the time stayed high. See
[`frame-cost-investigation.md`](./frame-cost-investigation.md) §4.

**The next step is scoping, not building.** Of the four items above, item 2 — a real
`SyncPoint`, handed to KMS instead of waited on — is the one that matters here and the
smallest. Items 1 and 3 are what make it safe. Before committing to the whole rewrite, find
out how far Smithay's `SyncPoint` already threads through our KMS path and whether handing the
fence to `queue_frame` is separable from the deferred-destruction work. That answer decides
between a contained change and a renderer project.

## Related

- [`frame-cost-investigation.md`](./frame-cost-investigation.md) — how this was arrived at, and
  every hypothesis measured and rejected on the way.
- `src/frame_log.rs` — the `NIRI_FRAME_LOG` grammar and what each phase covers.
- `niri-vk/src/stats.rs` — the submit/draw/shape counters.
- [`venus-timestamp-gap.md`](./venus-timestamp-gap.md) — why GPU-side timing cannot separate
  "the GPU was busy" from "we waited" on this VM.
- [`explicit-sync.md`](./explicit-sync.md) — the client-facing sync work, already landed.
- [`renderer-gaps.md`](./renderer-gaps.md) — the other standing renderer limitations.
