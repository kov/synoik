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

## The two slots

1. **`VulkanFrame::begin`** — drains the pending queues into the frame's command buffer *before*
   the render pass opens. Barriers, buffer→image copies, whole render-pass sequences, anything that
   must be outside a render pass. This is where new work goes by default. Today it carries
   `pending_glyphs`, `pending_dmabuf_acquires` (fresh imports and recommits alike),
   `pending_texture_uploads` (host imports, shm re-uploads and the wallpaper), `pending_sampleable`
   (the layout barrier a never-rendered offscreen needs) and `pending_blurs` (the xray blur chain).
   The order within `begin` is the order of dependencies: barriers before the blurs that sample
   them, and everything before the render pass.

2. **The mid-frame gap in `capture_region`** — it ends the frame's render pass, records a blit, and
   opens a continuation pass. Work that has to run *between* the frame's own passes belongs in that
   gap, via its `record_gap` closure; the backdrop blur is what it exists for. It used to submit
   and fence-wait there so that a separately-submitted blur could see the blit — recording the blur
   in the gap removed both.

There used to be a third: `run_commands_deferred`, a submit of its own that the CPU walked away
from. It was second best (it removed the wait and kept the round trip), and once both blurs folded
into a frame it had no callers left, so it is gone. If something genuinely cannot fold, it is in
the history.

Adding a queue is cheap: a `Vec` on the renderer, a `record_pending_*` drained by `begin`, and the
resources kept alive until retirement.

## The counter-rule: out-of-frame consumers

Queueing work only helps if nothing looks at the result before the queue drains. Anything that
samples, reads or overwrites a queued resource **outside** a frame must drain first — that is what
`VulkanRenderer::flush_pending_texture_uploads`, `flush_pending_blurs` and
`flush_pending_sampleable` are for. Between them the callers are two readbacks; each is a wait that
was already happening.

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
- **the `flush_pending_*` drains** — they *are* the drain, and they only run for a readback.
- **`new_coverage_atlas`** — once per atlas.
- **`make_sampleable` on an image that holds contents** — a texture that reached a sampled state by
  some route other than a frame. Rare, and the barrier is not a discard, so it cannot be queued the
  way the `UNDEFINED` case is. Watch the `transition` site in the frame log: it should now be
  quiet.

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

## The third rule: share the staging too

A deferred copy has to keep its source bytes until the submit, and the obvious way to guarantee
that — a staging buffer per upload — is a bug on this stack. A `HOST_VISIBLE` buffer is a
virtio-gpu blob, an shm re-upload happens on every commit of every shm surface, and the host ran
out of blobs two minutes into a live session, after which every `vkAllocateMemory` failed and the
session did not recover.

So staged pixels go into `niri_vk::staging::StagingPool`: one grow-only, persistently mapped
buffer, N offsets, rewound per frame. What decides when it can be rewound is the **reference
count**, not a fence — an upload holds its chunk from staging until its command buffer retires, so
a chunk nobody else holds is one the GPU cannot be reading. Sharing means the pool only ever learns
that *all* of a frame's uploads are done, which is exactly what rewinding a whole chunk needs.

It removes four host round trips per upload as a side effect (create, allocate, bind, map), leaving
a `memcpy` into a mapping that has been warm since the first frame.

Two deliberate exceptions: an upload larger than 16 MiB gets a chunk of its own that dies with it
(pooling a 48 MiB wallpaper would pin its peak for the session), and pixels a worker already wrote
into device-visible memory (`HostStaging`) are staged where they lie — the copy just names that
buffer and holds it by `Arc`.

## Where this got to

All three follow-ups this document used to carry are done: both blurs fold into a frame's command
buffer, and `make_sampleable` queues its barrier for the fresh-offscreen case that turned out to be
the one firing (the note said to check the frame log first, and it was right to — the culprit was
the effect buffer's *no-redraw* branch making a brand-new offscreen sampleable, not the blur's
source).

**On the frame path, the only thing that still waits is CPU readback.** A frame is one submit: the
uploads, the acquires, the glyph copies, the layout barriers and the blurs all ride it. What to
watch instead, now that the round trips are gone: the `created` clause of the frame log, since on
Venus every `vkCreateImage`/`vkAllocateMemory` is still a synchronous host round trip and that is
where the remaining per-frame host cost lives.
