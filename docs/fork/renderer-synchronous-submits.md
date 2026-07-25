# The synchronous-submit ceiling

**Status: built, opt-in, measured on the seat 2026-07-25 — it works.** `NIRI_VK_ASYNC_SCANOUT=1`
turns it on; without it the renderer still waits, exactly as before. The measurement, including
the A/B against the wait forced back on, is [below](#measured-on-the-seat-2026-07-25). Full
context in [`frame-cost-investigation.md`](./frame-cost-investigation.md).

Landed: `6bef18ac` (chain every submit on a queue timeline), `6f645bd8` (the scanout frame hands
its fence to KMS), on top of `7b5f016d` (time the wait apart from the submit).

**One-line summary:** every `vkQueueSubmit` in the owned renderer was immediately followed by
`wait_for_fences(…, u64::MAX)`. The CPU blocked until the GPU drained, every time. On the scanout
submit that wait is 12–14 ms of a 16.67 ms frame, it does not depend on anything we draw, and
nothing overlaps it. The scanout submit can now skip that wait and hand its fence to KMS instead;
every other submit still waits, and so does that one unless the session opts in.

## What the code did, and still does by default

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

## What fixing it involved

*Written before the work; kept because the estimate is worth comparing against
[what was built](#what-was-built). Items 1 and 2 landed, item 3 turned out to be unnecessary and
item 4 was not a prerequisite — but the estimate missed submit **ordering** entirely, which is
the part with the real correctness risk.*

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

### What was built

1. **`VkSubmitFence`** (`src/render_helpers/vulkan/fence.rs`) — Smithay's `Fence` over a `VkFence`
   created exportable *before* the submit, since a `SYNC_FD` fence's handle types are fixed at
   creation. Cheaply clonable: the same completion is both the sync point KMS holds and the
   renderer's proof that a command buffer is still busy, and the fence dies with the last clone.
2. **A queue timeline** (`Gpu::submit`) that every submit is chained on, wait(N) → signal(N+1).
   This is the part the scoping missed and it is not optional; see above.
3. **An in-flight list** on `VulkanRenderer`, holding `(timeline, cbuf, fence, held)` and retired
   by **polling** the timeline at the top of `VulkanFrame::begin`. The timeline rather than the
   fence, because the `SYNC_FD` export resets the fence and one device call covers every
   outstanding submit; polling rather than waiting, for the reason in the last bullet above.
4. **Eligibility, deliberately narrow.** Only the frame going to KMS: `tty.rs` brackets
   `DrmCompositor::render_frame` with `set_finish_may_defer`, because a screencopy or screencast
   render into a dmabuf is indistinguishable from inside the renderer and hands its buffer to a
   consumer that expects it finished. Deferral additionally requires the device to order submits,
   GPU timing to be off (its query pool is per-frame), and the session to have asked.

## Measured on the seat (2026-07-25)

One session, `niri[79726]` at `v26.04-573-g6c5a5c10`, `NIRI_VK_ASYNC_SCANOUT=1` +
`NIRI_FRAME_LOG=1`: 12.2 min of real use (startup, overview open/close, the app grid, workspace
switches), then the same interactions again for 56 s with
`debug { wait-for-frame-completion-before-queueing }` written into the live config — which niri
re-reads on save, so the control ran **in the same process, on the same binary, minutes apart**.
That flag leaves the renderer deferring and still hands the fence to KMS; it only makes
`tty.rs` block on the sync point before queueing. So it isolates *who pays the wait* and nothing
else.

| | deferred (12.2 min) | wait forced back on (56 s) |
|---|---|---|
| frames over budget | **4, all within 3 s of startup; zero after** | **2 in 56 s**, both overview-close |
| `submit` phase | p50 **0.48 ms** | **13.33 / 16.51 ms** |
| scanout wait on the frame line | absent (`1 to scanout in 0.00ms`) | absent — the wait is `tty.rs`'s, not the renderer's |
| missed vblanks | 41.6/min | 52.5/min |

The two control frames are the whole argument:

```
took 18.46ms — collect 4.49ms submit 13.33ms; 2 submits in 0.02ms, waiting 3.04ms
              (1 to scanout in 0.01ms), 1 bakes in 3.36ms, animating, overview 1.87
```

A light frame — two submits, 4.5 ms of collect — pushed over budget by nothing but the wait.
Frames of exactly this shape were **376 of 400** over-budget frames in the last pre-change
session (`138865`: median total 18.6 ms, median submit phase 11.3 ms) and 21 of 34 in the one
before it. Under the deferred path that entire class is gone: every over-budget frame in both
new sessions has more than three submits.

Answers to the four questions, in the order they were asked:

1. **The wait went, it did not move.** `waiting … on earlier work` never printed once, and the
   renderer's own `waiting` on a 2-submit frame is ~3 ms, not ~13.
2. **Yes.** `submit` collapsed from p50 11.1 ms to p50 0.48 ms, and the frame class the wait
   created stopped appearing.
3. **No — presentation does not get later.** 41.6 missed vblanks/min deferred against 52.5/min
   with the wait on, and 41.3 / 45.6 per min in the two comparable pre-change sessions. Every
   event is "presented 16.67 ms late", one refresh, in every build. The once-per-second idle miss
   (the clock repaint; 70–79% of baseline missed-vblank events are 1.00 s apart) is unchanged in
   rate and magnitude — it is a scheduling artefact of the tick, not something this touches.
4. **No corruption, no fallback.** `could not export the frame's fence` never fired, so the
   `SYNC_FD` reached the atomic commit every frame; `needs_sync` stayed false, so smithay never
   took its CPU-wait path (`queue` held at 0.07–0.08 ms); no Vulkan errors; no visual artefacts
   observed while driving it.

### What is left

**The other submits, and they are now the whole cost.** A frame issues 7–27 non-scanout submits —
widget bakes, glyph-atlas uploads, dmabuf import transitions — and each still does create-fence →
submit → `wait_for_fences` → destroy at **~1.3–1.6 ms**. Fifteen of those is ~20 ms, it all lands
in `collect`, and after this change it is the only thing that puts a frame over budget. Matched
startup frames confirm it: the 9-submit startup frame costs 46.4 ms deferred against 50.3–68.3 ms
before, i.e. the scanout wait was never its problem.

The same treatment applies, and the seam is already built. `should_defer_finish` is a policy
question, not a mechanism one: the timeline chain already guarantees GPU order, so a bake's
consumer — later GPU work sampling the result — needs no CPU wait at all. The waits that must
stay are the ones where the **CPU** reads the result (`map_texture`, screenshots) or a foreign
consumer takes the buffer (screencopy, screencast). See
[`frame-cost-investigation.md`](./frame-cost-investigation.md) §6.

**Whether to drop the env-var gate** is the other open decision. The evidence says default-on;
the counter-argument is that this VM is one stack and `supports_fencing` being false elsewhere
falls back to `needs_sync` → a CPU wait, which is today's behaviour anyway.

*Superseded, kept for the record — the questions this section used to pose, all four now answered
above: does `waiting` leave the frame line without reappearing as `waiting … on earlier work`; do
frames stop going over budget and does `submit` collapse toward `queue`; does presentation get
later; is there any visual corruption. Headless covers none of them — there is no KMS plane to
take the fence. What a test does cover
(`a_deferred_finish_returns_a_fence_and_still_orders_what_follows`) is that the sync point comes
back exportable, that the `sync_file` export succeeds on Venus, and that a readback issued with
no wait still sees a finished frame — which is only true because of the timeline chain.*

### Client buffer release ordering — settled, not a blocker

Smithay refcounts the client buffer: `renderer::utils::Buffer` is an `Arc<InnerBuffer>` whose
`Drop` does *both* `wl_buffer.release()` and the `linux-drm-syncobj-v1` release-point signal
(`backend/renderer/utils/wayland.rs:68`). The only holder is `RendererSurfaceState.buffer`, so
today the release fires on the client's **next commit** — not on our GPU completion. Our fence
wait is what makes that safe: by the time the frame returns, every read of that buffer is done.

The question was whether removing the wait breaks it. It does open a window — a client that
commits again inside our in-flight frame can be told to reuse a buffer we are still sampling —
but **that window is upstream Smithay's status quo, not something this change introduces.**
`GlesFrame::finish_internal` returns a real unsignalled EGL fence whenever `export_sync_point()`
succeeds (`backend/renderer/gles/mod.rs:2514`), falling back to `glFinish` only when it cannot;
every Smithay compositor on the GLES path — including this one, before the owned renderer — has
always released client buffers on their next commit while a frame was still in flight. Our
synchronous renderer is *stricter* than upstream here, not correcting an upstream bug.
Implicit-sync clients are additionally covered by the dmabuf's `dma_resv` where the driver
participates; explicit-sync clients are the exposed ones, and are equally exposed on GLES.

So: match upstream, do not build for it. If it ever bites, the mitigation is known and small —
hold a `Buffer` clone (`RendererSurfaceState::buffer()` is public) on the imported texture, so
the existing `held` list defers the release to retirement along with everything else.

### What had to be settled before it shipped

*All four settled; the measurement above is what settled the last two.*

- **Nothing may execute alongside an in-flight frame unless proven disjoint.** This is the real
  weight of item 1, and lifetime is only half of it. The present-blit shadow is *one image per
  size* (`renderer.rs:140`), shared by consecutive frames: the moment two scanout submits can
  overlap, frame N+1's render pass writes the shadow that frame N's present blit is still
  reading. The glyph atlas is the same shape — an upload issued while a frame samples it. Both
  are impossible today only because the frame completed before we returned.

  The fix that keeps today's semantics exactly: **order every submit on the queue after the
  previous one**, with a timeline semaphore chained across all three submit sites (the frame
  finish, the mid-frame capture flush, `Gpu::run_commands`). GPU execution order then equals
  submission order, as now, and the only thing that changes is that the CPU stops blocking —
  which is the entire goal. It is uniform, it needs no per-frame resource pools, and it costs
  nothing on a stack where the GPU is not the bottleneck. `timelineSemaphore` is available here
  (Venus reports Vulkan 1.3 and the feature true) but is **not currently enabled** — the device
  is created with no feature struct at all (`niri-vk/src/gpu.rs:355`).
- **The frame log must not report a fake win.** `stats::submit` used to time the submit *and* the
  wait, so removing the wait would collapse that number to the cost of `vkQueueSubmit` alone and
  read as a 12 ms saving that merely moved. Settled by `7b5f016d`: enqueue and wait are timed
  apart, retirement has its own counter, and a frame that waits for work it did not submit says
  so. That split is what let the A/B above attribute 13.33 ms to `tty.rs`'s wait rather than to
  anything the renderer did.
- **Retirement must not become the wait by another name.** Draining the previous frame at the
  top of the next one is where GLES puts it (`renderer.cleanup()` in its `finish_internal`), but
  it has to *poll* — a blocking drain one frame later, with frames back-to-back, moves the 13 ms
  rather than removing it. It polls, and the measurement confirms it: `waiting … on earlier work`
  never printed, and the renderer's wait on a light frame is ~3 ms.
- **It may trade CPU time for a frame of latency.** If our fence signals ~13 ms after submit and
  we commit immediately, the kernel's commit worker may miss the upcoming vblank and flip on the
  next one. The CPU stops blocking either way — that is the stated goal — so the thing to measure
  is *presentation* time, not main-loop time. Measured: it does not. 41.6 missed vblanks/min
  deferred, 52.5/min with the wait forced back on, 41.3–45.6/min before the change.

## Related

- [`frame-cost-investigation.md`](./frame-cost-investigation.md) — how this was arrived at, and
  every hypothesis measured and rejected on the way.
- `src/frame_log.rs` — the `NIRI_FRAME_LOG` grammar and what each phase covers.
- `niri-vk/src/stats.rs` — the submit/draw/shape counters.
- [`venus-timestamp-gap.md`](./venus-timestamp-gap.md) — why GPU-side timing cannot separate
  "the GPU was busy" from "we waited" on this VM.
- [`explicit-sync.md`](./explicit-sync.md) — the client-facing sync work, already landed.
- [`renderer-gaps.md`](./renderer-gaps.md) — the other standing renderer limitations.
