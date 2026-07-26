# The synchronous-submit ceiling

**Status: built, opt-in, measured on the seat 2026-07-25 — it works.** `NIRI_VK_ASYNC_SCANOUT=1`
turns it on; without it the renderer still waits, exactly as before. The measurement, including
the A/B against the wait forced back on, is [below](#measured-on-the-seat-2026-07-25). Full
context in [`frame-cost-investigation.md`](./frame-cost-investigation.md).

Landed: `6bef18ac` (chain every submit on a queue timeline), `6f645bd8` (the scanout frame hands
its fence to KMS), on top of `7b5f016d` (time the wait apart from the submit).

> **Writing renderer code?** This document is the history and the measurements. The *rule* that
> came out of them — share the frame's submit, never give frame-path work a submit of its own — is
> [`frame-submit-discipline.md`](./frame-submit-discipline.md), which is the one to read first.

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
   and the session to have asked. **GPU timing used to be a third condition** — a single timestamp
   pair per renderer would have been reset by the next command buffer while an in-flight submit was
   still writing it. That made measuring the renderer change the renderer: `NIRI_FRAME_LOG=…,gpu`
   silently put the live seat back on the synchronous finish for the whole session, so every
   reading taken with it described a configuration the seat does not run. `GpuTimer` now keeps a
   ring of pairs, one per outstanding submit, read at retirement instead of after the wait
   (2026-07-26). Neither has to give way.

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

Scoped below — and that scoping turned up a **defect in this change**, already enabled on the
seat: the in-flight record holds the textures a frame *samples* but not the ones it *renders into*,
either of which a cache can free while the submit is in flight. Fix that before anything else and
before dropping the gate; details under
[First, a defect in what already shipped](#first-a-defect-in-what-already-shipped).

**Whether to drop the env-var gate** is the other open decision. The evidence says default-on;
the counter-argument is that this VM is one stack and `supports_fencing` being false elsewhere
falls back to `needs_sync` → a CPU wait, which is today's behaviour anyway.

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
## The other submits — scoping (2026-07-25)

*Same question as the scanout scoping, one level down: what would it take, and what is separable.
The answer is a different shape. The first draft of this section assumed the mechanism was already
built and only policy was left; an adversarial review of it against the code found that wrong in
two central places, and turned up three latent defects in what already shipped. Both corrections
are below — the first draft's claims are kept where they were right and struck where they were not,
because the mistakes are the instructive part.*

### First, a defect in what already shipped

**The in-flight record does not hold the frame's own render target.** `InFlightSubmit._held`
(`renderer.rs:1319`) is populated only by `VulkanFrame::retain` (`frame.rs:216`), which holds every
texture a draw *samples*. The scanout submit's command buffer also references two textures nobody
holds:

- **`fb.buffer`** — the present-blit shadow, owned by `present_blit_shadows`, an 8-entry LRU
  (`renderer.rs:233`, eviction at `:1990`). Bind a ninth size while a frame is in flight and the
  shadow it renders into is destroyed under it.
- **`fb.present`** — the imported dmabuf target, dropped by `dmabuf_target_cache` eviction when its
  weak handle goes (`renderer.rs:1817`).

`VkTextureInner::Drop` destroys the image, framebuffer and descriptor pool with no wait
(`types.rs:61`), so either is a use-after-free on an in-flight submit. Both cache comments still
justify their safety with "rendering is synchronous (`finish` CPU-waits, so no shadow is in flight
when the next bind reuses or drops it)" (`renderer.rs:146`, `:200`) — true when written, stale
since `6f645bd8`. `set_custom_shader` carries the same stale justification, "every submit
fence-waits in `finish`, so the old pipeline is guaranteed idle — no `device_wait_idle` needed"
(`renderer.rs:498`): a config reload that changes a custom shader while a frame is in flight
destroys a pipeline that submit is using.

Reachability is narrow — more than eight live present-blit sizes means several outputs plus casts
plus screencopy, and custom shaders are a debug/animation feature — but this is a correctness bug
in code that is enabled on the seat, not a scoping item. **Fix it before any slice below:** clone
`fb.buffer` and `fb.present` into the record, drain in-flight work before a cache eviction or
pipeline destroy (or hold the resource the same way), and rewrite the three comments so the next
reader is not told a false invariant.

### There are only two submit sites, and we cannot currently tell them apart

Every `vkQueueSubmit` goes through one of:

- **`Gpu::run_commands`** (`gpu.rs:681`) — the one-shot: allocate cbuf, record, create fence,
  submit, `wait_for_fences`, free.
- **`VulkanFrame::finish_internal`** (`frame.rs:1495`) — a whole rendered frame. Plus a third
  path hiding inside it: **`capture_region`'s mid-frame flush** (`frame.rs:1011`) ends the command
  buffer, submits, waits, and re-opens a continuation buffer, once per captured region.

`SubmitKind` has two variants and `submit_kind()` keys on `fb.offscreen` (`frame.rs:1456`), so
"scanout" means *any non-offscreen target* — a screencast or screencopy render into a dmabuf counts
as scanout, and so does every mid-frame capture flush of the KMS frame. `N to scanout` is therefore
not "the frame that went to KMS", and the frame line's `N bakes` counts only *widget* bakes, while
window previews, snapshots, `ClosingWindow`, xray and effect buffers submit without being bakes.
A frame from the seat reads `36 elements, 67 draws covering 0.3x the output, 15 submits, waiting
15.58ms, 1 bakes in 1.13ms`: **fourteen submits attributable to nothing.** (Fixed by slice 0
below; the numbers in this section are what the counters said before it.)

The leading code-grounded hypothesis, from the review: **N xray/translucent surfaces × 2 submits
each** — one offscreen re-render of the effect buffer (`effect_buffer.rs:288`) plus one
`EffectBlur::run` (`effect_blur.rs:96`). Seven such surfaces is fourteen. If that is right, slices
1 and 2 are the whole fix and the upload work is not worth building. That is a hypothesis, and this
investigation's own history (`frame-cost-investigation.md` §3) is four plausible unmeasured
hypotheses that all died — so it is still slice 0's job to confirm it.

### Two hazard classes, and the timeline only closed one

The scanout deferral's risk was **GPU-vs-GPU**: work issued after an in-flight submit executing
alongside it. The timeline chain (`6bef18ac`) closed that uniformly and costs nothing. Two classes
remain, and neither is helped by it:

**CPU-vs-GPU.** A CPU write into a mapped `HOST_VISIBLE` buffer is not a queue submission, so the
timeline does not order it against anything:

- **`VulkanRenderer::shm_staging`** (`renderer.rs:119`) is *one reused staging buffer* for every
  shm client upload: `ensure` → `write` → `reupload_full` → submit → wait (`renderer.rs:2124`).
  Remove the wait and the next commit's `write` memcpys over bytes the previous copy is still
  reading. Worse, `Staging::ensure` **destroys and reallocates** when it grows (`texture.rs:1385`)
  — a use-after-free, not a stale pixel.
- **`Texture::upload` / `TextureBatch::finish`** free their staging buffer and memory immediately
  after `run_commands` returns (`texture.rs:219`, `:893`), commented "the staging buffers have
  served their purpose … `run_commands` already drained the device". That comment is load-bearing.
- **`upload_coverage_regions`** (the glyph atlas, `texture.rs:1154`) allocates staging per call
  under an `UploadGuard` that frees on scope exit — same shape, per-call rather than shared.
- **Readbacks** (`copy_framebuffer` → `Staging::read`) must wait by definition: the CPU reads the
  bytes. Screenshots, screencopy, screencast. Correct as they are; they must stay that way.

**Manual teardown.** Resources freed by an explicit `destroy(&gpu)` rather than a refcount, which a
`Vec<VkTexture>` record cannot express. `BlurChain` is the case that matters: destroyed with no
wait by `EffectBlur::Drop` (`effect_blur.rs:110`), by `BackdropBlur` slot replacement on a size or
pass-count change (`frame.rs:1104`), and by `render_blur` immediately after `run_commands` returns
(`renderer.rs:1181`) — correct only because of the wait. Deferring a blur submit means keeping its
pipelines, level pyramid, framebuffers and descriptor pool alive to retirement. The render-target
defect above is the same class.

So "the timeline already orders it" is true for image *contents* and false for every staging buffer
and every manually torn-down object.

### Site inventory

| site | what fires it | hazard if deferred | verdict |
|---|---|---|---|
| offscreen `VulkanFrame::finish` | widget bakes, window previews, snapshots, `ClosingWindow`, xray/effect buffers | **the render target itself** — caller-owned, recreated by `OffscreenBuffer`/`EffectBuffer` on size change, freed without a wait | **done**, slice 1: record holds the target, reuse test retires first |
| `capture_region` mid-frame flush | one per captured region per frame | none — the consumer is the separately-submitted blur, which the timeline already orders after it | **the cheapest win in the table**: its wait is redundant *today* |
| `EffectBlur::run`, `BackdropBlur::run_blur`, `render_blur` | one per blurred surface per frame | `BlurChain` teardown (manual, no refcount) | defer once the record can hold non-texture resources |
| `make_sampleable` | nothing, in practice | none | confirmed ~dead: an offscreen `finish` already leaves `SHADER_READ_ONLY_OPTIMAL` (`frame.rs:1485`) |
| `import_dmabuf_sampled` | a new or reallocated client dmabuf | the texture must be cloned into the record against cache eviction (`renderer.rs:1824`) | defer, cheap |
| ~~`reacquire_dmabuf_sampled`~~ | **nothing** — the hot path was folded into the frame submit; `record_reacquire_dmabuf` (`renderer.rs:1878`) is the only live user and the standalone submitting variant (`texture.rs:855`) has no compositor caller | — | dead; deferring it is worth zero |
| `reupload_shm` | every shm client commit | **shared staging: overwrite + realloc-free** | needs a staging ring or per-submit ownership |
| `Texture::upload`, `TextureBatch::finish` | icon uploads, first app-grid page | per-call staging freed on return | staging moves into the record |
| `upload_coverage_regions` | new glyphs entering the atlas | same | same |
| `new_coverage_atlas` | atlas creation and growth (`texture.rs:1091`) | none | rare; defer with the transitions |
| `copy_framebuffer` / readbacks | screenshots, screencopy, screencast | the CPU reads the result | **must keep waiting** — and that is what makes slice 1 safe |

### Where the bookkeeping belongs

The in-flight list lives on `VulkanRenderer`, retired by polling the timeline at
`VulkanFrame::begin`. That does not generalise: `run_commands` is called from inside `niri-vk`
(`texture.rs`, `dmabuf.rs`, `blur.rs`) by code with no renderer to reach. Moving the ring onto
`Gpu` is the obvious answer — it owns the queue, the timeline and `submit()` — but it is not free,
and the first draft of this section said none of this:

- **Command buffers are pool-scoped.** `Gpu` outlives `VulkanRenderer` (it is an `Arc`), and
  `VulkanRenderer::Drop` destroys `command_pool` (`renderer.rs:1582`), implicitly freeing every
  cbuf in it. A ring on `Gpu` still holding those handles double-frees. Records must carry
  `(pool, cbuf)`, every pool owner must drain before destroying its pool, and `Gpu::drop` must
  release records without touching cbufs.
- **`Gpu` is shared across threads.** It is behind an `Arc` and `VkTexture` is asserted
  `Send + Sync` (`types.rs:280`), so the ring needs a `Mutex` (`modifier_features`, `gpu.rs:130`,
  is the precedent). `SubmitOrder.next`'s `Relaxed` ordering is justified by "issued from the
  thread that owns the renderer" (`gpu.rs:613`) — that comment becomes load-bearing.
- **`Gpu` cannot name `VkTexture`.** It is a niri-crate type (`types.rs:80`); the record needs
  erasure (`Box<dyn Any + Send>`) or a drop-callback.
- **Retirement wants to poll inside `Gpu::submit`**, not only at `VulkanFrame::begin` — every
  deferring site already calls it, and frameless stretches would otherwise never retire uploads.

### Proposed slices

0. ~~**Attribute the submits. No behaviour change.**~~ **Done.** `SubmitKind` (offscreen/scanout,
   keyed on the target) became `SubmitSite`, keyed on the caller: scanout / dmabuf / offscreen /
   capture / upload / transition / blur / readback. The frame line now ends its submit clause with
   one entry per site, worst wait first. Two things fall out immediately: a screencast render and
   a mid-frame capture flush are no longer counted as scanout, and the fourteen unattributed
   submits will name themselves on the next seat run. Pinned by
   `a_submit_is_counted_at_the_site_that_made_it`, whose dmabuf half fails against the old
   target-keyed logic.
1. ~~**Let offscreen frames defer.**~~ **Done (2026-07-25).** Two things the first draft got wrong:
   this is *not* removing the `!fb.offscreen` clause, because `finish_may_defer` is only true inside
   the tty bracket around `render_frame` (`tty.rs:2892`) and every offscreen finish happens earlier,
   during element collection — dropping the clause defers nothing. Offscreen had to become its own
   eligibility rule (`VulkanRenderer::should_defer_offscreen_finish`): the same two device
   requirements as scanout — a total order on submits (the query-pool clause is gone, see above) — minus the tty
   bracket, and minus the *exportable* fence, since nothing outside the process ever takes an
   offscreen's completion. That last point also means an offscreen finish can never be pushed back
   onto the synchronous path by an export failure, the way a scanout frame can.
   And the record holds the target: `OffscreenBuffer` drops its texture on size increase or
   non-uniqueness (`offscreen.rs:105`), `EffectBuffer` replaces its offscreen on size change
   (`effect_buffer.rs:236`).

   **The trap, and what it cost to close.** Holding a clone makes `is_unique_reference`
   (`types.rs:201`) false, and `OffscreenBuffer` reuses on exactly that test (`offscreen.rs:113`) —
   so a naive keep-alive turns into reallocate-every-frame, trading the fence wait we just removed
   for a synchronous host round trip through `vkCreateImage`. The fix is that
   `OffscreenRenderer::offscreen_is_reusable` now takes `&mut self` and retires first: a record only
   disappears once the GPU has passed that submit, which is the *same* condition that makes
   overwriting the image safe, so the poll is not a workaround but the honest answer. Pinned by
   `retirement_lets_a_deferred_offscreen_be_reused_instead_of_reallocated`, which fails (as
   "not unique") with the retire removed, and whose second half pins that retirement did not
   overcorrect into "reusable, always" — a snapshot someone else still holds must still block reuse.

   Deferral itself is pinned by `an_offscreen_finish_defers_without_the_kms_bracket`, which
   deliberately does *not* set `finish_may_defer` (a rule that needs it is a rule that never fires)
   and reads the target back with no intervening wait, so only the queue timeline can order it.

   Layout bookkeeping had to follow: the deferred branch used to hardcode `TRANSFER_SRC_OPTIMAL`,
   which is right for scanout and wrong for an offscreen — the barrier to `SHADER_READ_ONLY_OPTIMAL`
   rides the same command buffer, and recording it is what keeps `make_offscreen_sampleable` the
   no-op it must be here (`renderer.rs` `make_sampleable` early-returns on that layout). Getting it
   wrong would have put a whole extra submit-and-wait back per offscreen.

   Still gated on `NIRI_VK_ASYNC_SCANOUT`, the same opt-in as slice 0, so the suite exercises it
   only through the two tests above.

   **Severity, corrected 2026-07-25:** the reallocate-every-frame churn used to *abort* the session,
   and that abort was fixed at the VMM level — so it was a performance and host-resource problem by
   the time this landed, not a stability one.
2. **The capture flush and the blurs.** The flush needs only its cbuf and fence deferred; the blurs
   need the record to hold non-texture resources. Together these are the other half of the M1
   hypothesis.
3. **Move the ring to `Gpu`** with the constraints above, and defer the host-free one-shots
   (transitions, dmabuf acquires, atlas creation).
4. **Upload sites**, each needing its staging kept alive; `shm_staging` needs a small ring or
   per-submit ownership. Last, and only if slice 0 says it is worth it.

### The invariant that makes any of this safe

Every capture of an offscreen goes through `copy_framebuffer` → `run_commands`, which submits on
the chained timeline *after* the deferred offscreen submit and then CPU-waits
(`renderer.rs:1009`). So a deferred offscreen feeding a readback is provably finished when the
bytes are read — **iff** (a) readbacks keep their wait and (b) every submit goes through
`Gpu::submit`. Those two conditions are the whole safety argument for slice 1; write them down and
pin them. The existing cross-check that the timeline advanced exactly once per counted submit
(`every_submit_is_chained_on_the_queue_timeline`) is the tool for (b).

### What would tell us it went wrong

Corruption here is **not** a torn frame — it is a stale or garbled *texture*: a window preview
showing the previous frame, an icon with another icon's pixels, glyphs from the wrong atlas slot.
`NIRI_VK_VALIDATION=1` catches the use-after-free half and must be run after every slice. It will
not catch a staging buffer overwritten while a copy reads it: that is legal Vulkan, and no layer
will say a word. Only a test that uploads twice without a wait and checks the first result, or eyes
on the seat, will find that one.

Two smaller things to write down rather than discover:

- **Device loss moves.** Today a wait failure (≈ `DEVICE_LOST`) is caught at the call site, which
  drains the device before any guard frees anything (`gpu.rs:721`). A deferred one-shot surfaces it
  at retirement, where the caller's context is gone.
- **In-flight growth is bounded only by GPU progress.** Retirement polls and never blocks
  (`renderer.rs:1327`). A burst of offscreen submits while the host stalls accumulates command
  buffers and keep-alive lists with no cap. That is probably fine — say so deliberately rather than
  leaving it unstated.

## Related

- [`frame-submit-discipline.md`](./frame-submit-discipline.md) — the forward-facing rule this all
  turned into, plus the follow-ups still open.
- [`frame-cost-investigation.md`](./frame-cost-investigation.md) — how this was arrived at, and
  every hypothesis measured and rejected on the way.
- `src/frame_log.rs` — the `NIRI_FRAME_LOG` grammar and what each phase covers.
- `niri-vk/src/stats.rs` — the submit/draw/shape counters.
- [`venus-timestamp-gap.md`](./venus-timestamp-gap.md) — why GPU-side timing cannot separate
  "the GPU was busy" from "we waited" on this VM.
- [`explicit-sync.md`](./explicit-sync.md) — the client-facing sync work, already landed.
- [`renderer-gaps.md`](./renderer-gaps.md) — the other standing renderer limitations.
