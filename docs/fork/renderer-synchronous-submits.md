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

## The scoping answer (2026-07-25)

The question this document set — *how far does Smithay's `SyncPoint` already thread through our
KMS path, and is handing the fence to `queue_frame` separable from deferred destruction?* — is
answered: **the whole path above us already exists, the hardware here supports it, and the work
is separable.** This is a contained change, not a renderer project.

**Everything downstream of `finish()` is already built.** Smithay takes the `SyncPoint` our
`Frame::finish` returns, stores it on the primary plane's config
(`backend/drm/compositor/mod.rs:2324`), and in `build_planes` (`:777`) exports it to a native
fence FD and hands it to the atomic commit as `IN_FENCE_FD`
(`backend/drm/surface/atomic.rs:1289`). `RenderFrameResult::needs_sync`
(`compositor/frame_result.rs:60`) is precisely "this could not be done, block the CPU instead",
and `src/backend/tty.rs:2888` already honours it. **We would not change a line of the KMS path.**

**The hardware here supports it.** Probed on this VM's `card0`: driver `virtio_gpu`,
`DRM_CAP_SYNCOBJ` = 1, `DRM_CAP_SYNCOBJ_TIMELINE` = 1, the primary plane carries `IN_FENCE_FD`
and the CRTC carries `OUT_FENCE_PTR`. With an atomic surface and no Nvidia, Smithay's
`supports_fencing` (`compositor/mod.rs:1204`) is **true** on the live seat.

**The fence is exportable and real.** `VK_KHR_external_fence_fd` is already enabled on the device
(`niri-vk/src/gpu.rs:333`), and `niri-vk/src/sync_spike.rs` measured that on Venus a `VkFence`
`SYNC_FD` export is genuinely pipelined — non-blocking export, unsignalled at export, a real
`virtio_gpu` `dma_fence`, downstream waits block for the GPU's duration. (`VkSemaphore` export is
emulated here; that asymmetry is why this rests on fences.)

**Deferred destruction is bounded, because the keep-alive list already exists.** `VulkanFrame`
holds `held: Vec<VkTexture>` (`frame.rs:59`) for exactly this reason — every texture a draw
references is ref-count-bumped so it outlives the submit. Today it drops after the wait. Making
it outlive an in-flight submit means moving three things into a retirement record — the fence,
the command buffer, and `held` — and freeing them when the fence signals, checked at the top of
the next frame. Nothing else in the scanout submit is renderer-owned: pipelines, render passes,
descriptor sets (owned by the held textures) and the glyph atlas are persistent, and the target
buffer belongs to `DrmCompositor`'s swapchain, which will not recycle a queued slot.

**`run_commands` keeps its wait.** Item 4 above turns out not to be a prerequisite: uploads,
readbacks, blur chains and layout transitions all keep blocking, so `RunGuard`
(`niri-vk/src/gpu.rs:585`) is untouched and the general in-flight tracker is not needed. Only the
one submit that costs 12–14 ms changes. Offscreen frames keep waiting too — their results are
sampled immediately and there is nothing to hand the fence to.

### The plan

1. A `Fence` impl over `VkFence` — four methods (`is_signaled` / `wait` / `is_exportable` /
   `export` via `vkGetFenceFdKHR` with `SYNC_FD`) — and `finish_internal` returning
   `SyncPoint::from(it)` instead of `SyncPoint::signaled()`, for **scanout targets only**.
2. A retirement list on `VulkanRenderer`: `(fence, cbuf, held)`, drained at the top of `begin`.
   `Gpu::drop` already does `device_wait_idle`, so teardown is covered.
3. Keep the synchronous path selectable and use it where it is load-bearing: the existing
   `wait_for_frame_completion_before_queueing` debug flag, and forced on when
   `NIRI_FRAME_LOG=gpu` is set (one query pool is reused per frame, so `gpu_timer_begin`'s
   `cmd_reset_query_pool` would race an in-flight frame).

### What must be settled before it ships

- **Client buffer release ordering.** Smithay signals a `linux-drm-syncobj-v1` release point
  when the buffer's cached state is replaced (`wayland/drm_syncobj/mod.rs:110`), i.e. on the
  client's *next commit* — not on our GPU completion. Today the wait makes that safe for free.
  Without it, a client can be told a buffer is free while the GPU still samples it. `held`
  already keeps our import alive, so the fix is to gate the release (and arguably the frame
  callback) on retirement rather than on the frame returning. Implicit-sync clients are covered
  by the dmabuf's `dma_resv`; explicit-sync ones are not. **Decide this before landing.**
- **The frame log must not report a fake win.** `stats::submit` currently times the submit *and*
  the wait. Remove the wait and that number collapses to the cost of `vkQueueSubmit` alone, which
  would read as a 12 ms saving that merely moved. The retirement wait needs its own counter, or
  the win cannot be told from the bookkeeping.
- **It may trade CPU time for a frame of latency.** If our fence signals ~13 ms after submit and
  we commit immediately, the kernel's commit worker may miss the upcoming vblank and flip on the
  next one. The CPU stops blocking either way — that is the stated goal — but the thing to
  measure is *presentation* time, not main-loop time, or we will have smoothed the loop and
  added a frame of latency without noticing.

## Related

- [`frame-cost-investigation.md`](./frame-cost-investigation.md) — how this was arrived at, and
  every hypothesis measured and rejected on the way.
- `src/frame_log.rs` — the `NIRI_FRAME_LOG` grammar and what each phase covers.
- `niri-vk/src/stats.rs` — the submit/draw/shape counters.
- [`venus-timestamp-gap.md`](./venus-timestamp-gap.md) — why GPU-side timing cannot separate
  "the GPU was busy" from "we waited" on this VM.
- [`explicit-sync.md`](./explicit-sync.md) — the client-facing sync work, already landed.
- [`renderer-gaps.md`](./renderer-gaps.md) — the other standing renderer limitations.
