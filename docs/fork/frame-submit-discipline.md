# Sharing the frame's submit

**Read this before adding any `vkQueueSubmit` to the renderer.** It is the rule the frame path is
built on; the history and the measurements behind it are in
[`renderer-synchronous-submits.md`](./renderer-synchronous-submits.md) and
[`frame-cost-investigation.md`](./frame-cost-investigation.md).

## The rule

> Work on the frame path whose only consumer is **later GPU work** must be *recorded into a command
> buffer that is already going to be submitted*. It does not get a submit of its own.

`Gpu::run_commands` on the frame path is a defect, not a style preference. It submits and then
parks the compositor thread on a fence.

## Why, in numbers

On this stack a submit-plus-wait is a round trip whose cost has almost nothing to do with the work
it carries. Every one of these was a real measurement, not an estimate:

| what | cost | of which, the actual work |
|---|---|---|
| a **barrier**, on its own submit (`transition`) | 2.5–3.5 ms, twice per overview frame | one `vkCmdPipelineBarrier` |
| **11 shm re-uploads**, one submit each | 19.38 ms | 0.33 ms of bytes |
| **9 texture imports**, one submit each | 16.22 ms | 0.24 ms of bytes |
| a **blur** on its own submit | round trip 0.4–1.3 ms | blur ≈ 0.9 ms at output size |
| an **empty** submit + wait | 0.13 ms, or ~1.0 ms with fence feedback | nothing at all |

The pattern is the same every time: the payload is a rounding error and the round trip is the bill.
Ten cheap submits cost ten times more than one expensive one.

It compounds, too. Each blocking wait idles the guest↔host ring past its 1 ms timeout, so the
*next* submit pays a wake on top ([`venus-cost.md`](./venus-cost.md) §9.4) — the waits buy each
other.

## What makes recording sufficient

`Gpu::submit` chains every submit on a single timeline semaphore, each waiting on the previous
value (`niri-vk/src/gpu.rs`, gated by `Gpu::orders_submits`). GPU execution order is therefore
submission order, totally. For a consumer that is itself GPU work, "recorded" is as good as
"submitted and waited for" — which is why tracked image layouts are advanced at *record* time
throughout this code. That is not sloppiness; it is the invariant.

It is also why **submission stays on the renderer's thread**. The order counter is `Relaxed`
precisely because one thread hands out the numbers. See "Not the answer" below.

## The three slots

1. **`VulkanFrame::begin`** — drains the pending queues into the frame's command buffer *before*
   the render pass opens. Barriers, buffer→image copies, anything that must be outside a render
   pass. This is where new work should go by default. Today it carries `pending_glyphs`,
   `pending_dmabuf_acquires` (fresh imports and recommits alike) and `pending_texture_uploads`
   (host imports and shm re-uploads).

2. **The mid-frame gap in `capture_region`** — it ends the frame's render pass, records a blit,
   flushes, and opens a continuation pass. Work that has to run *between* the frame's own passes
   belongs in that gap, where it rides a submit that was happening anyway.

3. **`VulkanRenderer::run_commands_deferred`** — the fallback when the work genuinely cannot be
   folded into someone else's command buffer: its own buffer, submitted, *not* waited on, with its
   resources handed to the in-flight list. Second best — it removes the wait but keeps the submit.
   Falls back to a synchronous `run_commands` where the device cannot order submits.

Adding a queue is cheap: a `Vec` on the renderer, a `record_pending_*` drained by `begin`, and the
staging/resources kept alive until retirement.

## The counter-rule: out-of-frame consumers

Queueing work only helps if nothing looks at the result before the queue drains. Anything that
samples, reads or overwrites a queued resource **outside** a frame must drain first — that is what
`VulkanRenderer::flush_pending_texture_uploads` is for, and its doc lists the callers.

**Every out-of-frame consumer is the reason some flush exists.** Adding one adds a stall, usually
somewhere else than where you wrote it. Removing one is how a flush goes away — the shm re-upload
left that list when it stopped submitting.

The failure this catches is silent: a consumer that reads before the drain sees a blank image, or
gets overwritten *afterwards* when the frame finally records the stale copy. That is exactly how it
was found — an shm re-upload to green came out **red**.

## The other counter-rule: what you record, you keep alive

> Anything a queued command buffer names — an image, a buffer — must outlive **the submit**, not the
> recording.

Recording a barrier or a copy stores the *handle*. Destroy the object before the submit and Vulkan
invalidates the whole command buffer: every command recorded after it becomes invalid usage, and the
submit carries a poisoned buffer. So a queue that hands its entries to a command buffer cannot just
drop them when it drains — it hands them to whoever owns that submit.

That is why `record_pending_texture_uploads` returns its staging batch instead of dropping it, why
`record_pending_dmabuf_acquires` is `#[must_use]` and its batch goes into `VulkanFrame::held`, and
why `run_commands_deferred` takes `held`/`targets` at all. All the same rule.

The trap is that dropping is only fatal when it is the **last** reference, so a cache one layer away
decides whether you have a bug. Client dmabuf imports are cached weakly and swept on every lookup,
so a client that reallocates buffers (anything resizing — our own open animation forces it every
frame) leaves the queue holding the sole reference. Queue a texture the cache still holds and
nothing happens; queue one it has dropped and the compositor dies. It shipped, and the seat came
down on a client that did nothing wrong.

Pixel comparisons cannot see any of this — the image survives whenever the cache happens to hold it.
`NIRI_VK_VALIDATION=1` names it exactly (`VkImage … was destroyed`), which is why it belongs at the
*start* of an investigation, on the live session if that is where the misbehavior is.

## What still blocks, and why each is fine

- **CPU readback** (`download_region`, screenshots, screencopy) — has to wait; the CPU is the
  consumer.
- **`flush_pending_texture_uploads`** — it *is* the drain.
- **`TextureBatch::finish`** — already one submit for N textures (the app grid's ~24 icons).
- **`from_host_staging`** — one per wallpaper change, not per frame.
- **`new_coverage_atlas`** — once per atlas.
- **`make_sampleable`** — usually a no-op (a frame targeting an offscreen finishes it sampleable
  itself), and the one barrier that cannot simply be queued: see the follow-ups.

## Not the answer: a worker thread, or a second queue

The cost is not CPU work, so moving the submit to another thread does not make the GPU finish
sooner — the frame still cannot proceed until the work has run, so the wait relocates rather than
disappears. Worse, `Gpu` holds a **single** queue, so a second submitting thread needs a mutex
against the render thread, and it breaks the total submission order that lets everything else skip
its CPU wait. We would trade four deferrals to fix one barrier.

Worker threads are right for genuine **CPU** cost, and that seam already exists: wallpaper decode,
icon decode, and the staging write (`HostStaging` is `Send`, so any thread can fill it — the render
thread only creates the image, records the copy and submits). CPU work off-thread, submission on
the render thread.

A dedicated **transfer queue** with cross-queue semaphores is the honest version of the idea, but
it means giving up the single-timeline model for explicit inter-queue sync, and on Venus every
submit is a host round trip regardless. Wait for a measurement that demands it.

## Follow-ups, roughly in value order

Not urgent — the measured wins are taken. Do these after the remaining cheaper work.

1. **Fold `BackdropBlur::run_blur` into the frame's command buffer.** It is already called inside a
   `VulkanFrame`, and `capture_region` already ends the render pass, blits and flushes — recording
   the blur's passes in that gap costs **zero** extra submits. Today it only skips the wait.
2. **Fold `EffectBlur::run` in too**, via the pending-queue shape. The obstacle is ownership, not
   ordering: `BlurChain` is not refcounted and has no `Drop`, so a queued entry must keep it alive
   from `prepare_blur_vulkan` to `begin`. Probably means an `Arc` handle around the chain. (The
   rebuild hazard is already handled — both blur `Drop`s drain the device — but that only covers
   *destruction*, not a live reference across the frame boundary.)
3. **Then `make_sampleable` can be queued like everything else**, because (1) and (2) remove the
   only consumers that sample outside a frame. Until then it needs a consumer-side flush, not a
   queue: a queued barrier would be recorded at the next `begin`, i.e. *after* the blur's read.
   **Check the frame log first** — it early-returns on the offscreen path, so it may never fire; a
   `transition` in the log could equally be a coverage-atlas creation, which is not worth
   restructuring anything for.

The endgame those three point at: on the frame path, the only thing that still waits is CPU
readback.
