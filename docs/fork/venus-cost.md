# What Venus costs us — a VM/VMM handoff

**Written 2026-07-25**, from inside the guest (`gnome-shell-rs` dev VM), after a multi-day pass on
frame cost that ended with the guest side largely wrung out.

**Audience:** whoever works on the VM / host graphics stack — the VMM's virtio-gpu device, our
`virglrenderer` fork's Venus backend, guest Mesa `venus`, the host Vulkan driver.

**One-line summary:** on this stack a Vulkan *call* is cheap and a Vulkan *round trip* is
expensive, so our frame cost is set almost entirely by **how many times per frame we have to
talk to the host**, and barely at all by how much work we ask for. We have spent the last several
days removing round trips from the guest and it has worked; what is left is priced by the host and
we cannot reach it from here.

Companion documents, all of which this one summarises rather than replaces:

| document | what it is |
|---|---|
| [`frame-cost-investigation.md`](./frame-cost-investigation.md) | the full guest-side investigation: every hypothesis, including the dead ones |
| [`renderer-synchronous-submits.md`](./renderer-synchronous-submits.md) | the design of the guest-side fixes for the round-trip cost |
| [`venus-timestamp-gap.md`](./venus-timestamp-gap.md) | timestamp queries resolve to zero — with a reproducer |
| [`venus-bugs/README.md`](./venus-bugs/README.md) | two dmabuf/gbm findings — with reproducers |
| [`venus-explicit-sync-gap.md`](./venus-explicit-sync-gap.md) | explicit-sync surface |
| [`venus-probes/probe-venus-costs/`](./venus-probes/probe-venus-costs) | the standalone probe behind §9 — image-cache, mapping bandwidth, fence cost |

---

## 1. Environment

| | |
|---|---|
| Vulkan driver | `driverName = venus`, `driverInfo = Mesa 26.1.4` (`mesa-vulkan-drivers-26.1.4-3.limina.fc44.aarch64`) |
| Vulkan device | `Virtio-GPU Venus (Apple M4 Pro)`, `INTEGRATED_GPU`, API `1.3.353` |
| Guest kernel | `Linux 7.1.4-limina16k aarch64` |
| VMM / host | limina VM, our `virglrenderer` fork; host GPU Apple M4 Pro |
| Guest workload | `gnome-shell-rs`, a hand-rolled Vulkan compositor (no GL), `Virtual-1` @ 60 Hz, 3840×2160 |

The guest compositor is a **debug build**. That matters for CPU-side numbers (shaping,
rasterization) and **not** for anything in this document, which is all time spent waiting on or
talking to the host.

## 2. How to read the numbers

Everything below comes from `NIRI_FRAME_LOG` on the live seat (grammar in `src/frame_log.rs`,
counters in `niri-vk/src/stats.rs`). Every submit is attributed to the **call site** that made it,
and the fence wait is timed separately from `vkQueueSubmit` itself, because an earlier round of
this work could not tell "the submit is slow" from "the wait after it is slow" and guessed wrong.

Two cautions we learned the hard way and pass on:

- **Comparing two sessions is not an A/B.** Session-to-session numbers are dominated by how much
  the desktop was driven; the same build reported 0.8 and 12.3 over-budget frames per minute
  depending on the session. Where this document compares, it compares *within* a session or on a
  per-submit basis.
- **First-entry costs are not steady-state costs.** Opening a surface for the first time uploads
  its textures. Most of §3's evidence is deliberately drawn from those cold frames, because that is
  where the round trips are — but do not read them as what an idle desktop costs.

---

## 3. The findings, in order of what they cost us

### 3.1 A submit costs a fixed host round trip. Payload is free.

This is the single most important fact about the stack, and it is the cleanest measurement we
have. Four frames from one 12-second window after login, all doing the same kind of work (uploading
textures for surfaces being seen for the first time):

| upload submits | bytes moved | total wait | **per submit** |
|---|---|---|---|
| 4 | 0.0 MiB | 6.06 ms | **1.52 ms** |
| 8 | 0.4 MiB | 15.03 ms | **1.88 ms** |
| 9 | 1.0 MiB | 16.22 ms | **1.80 ms** |
| 5 | 3.2 MiB | 9.07 ms | **1.81 ms** |

The payload varies by more than three orders of magnitude. The cost per submit varies by **1.2×**.
An upload that moves essentially nothing costs 1.52 ms; one that moves 3.2 MiB costs 1.81 ms.

The consequence for a guest is stark: **there is no such thing as a cheap Vulkan submit here**, so
the only optimisation that works is to make fewer of them. That is what most of §4 is.

### 3.2 Submits are priced by contention, not by content

The same submit costs very different amounts depending on how busy the pipe is. From the earlier
investigation (`frame-cost-investigation.md` §1), the scanout submit's fence wait:

| | scanout submit fence wait |
|---|---|
| animation frames, one per refresh | **12.36 / 12.69 / 13.12 / 13.85 ms** |
| sparse frames (startup, one-offs) | 3.71 / 4.88 / 5.25 / 5.45 ms |
| every *other* submit in those same frames | 0.55 – 1.8 ms |

It does not track content: across those animation frames the scene covered 1.7–2.0× the output with
138–173 draws, with no correlation to the wait. It tracks **how closely frames follow each other**.

~13 ms is about one refresh interval at 60 Hz. **Our reading, which we cannot confirm from inside
the guest:** the host executes Venus's command stream on its own 60 Hz loop, and a guest fence wait
issued mid-cycle absorbs the remainder of a host vsync. If that is right, then a guest that submits
once per frame pays a full host frame for it, and a guest that submits fourteen times pays for the
first one and rides the same cycle for the rest — which is consistent with what we see.

**This is the number we would most like moved**, and it is the one item here that is purely
host-side. We have worked around it (§4.1) rather than fixed it: the fence now goes to KMS instead
of the compositor thread, so the wait still happens, just not where it stalls us.

> **The vsync reading is withdrawn — see §9.3.** A fence wait tracks GPU work almost exactly
> (0.28 ms per 4K image copy, ≈235 GB/s, with a ~0.1 ms floor), so nothing is being quantised to a
> refresh. What is real is an idle penalty, and §9.4 identifies its mechanism.

### 3.3 Resource creation is a synchronous host round trip, and the most variable one

`vkCreateImage` / `vkAllocateMemory` / `vkCreateImageView` / `vkCreateSampler` are not local
bookkeeping here — each is a host round trip. We instrument them separately (`N created in X.XXms`)
because they are neither a submit nor a draw, so nothing else in a frame log could see them; the
worst frames were carrying ~50 ms that was neither a fence wait nor rendering.

From one 12-second window on the live seat:

| resources created | total | **per resource** |
|---|---|---|
| 19 | 40.02 ms | **2.11 ms** |
| 12 | 18.81 ms | 1.57 ms |
| 11 | 7.95 ms | 0.72 ms |
| 6 | 6.24 ms | 1.04 ms |
| 1 | 0.16 ms | 0.16 ms |
| 4 | 0.07 ms | **0.018 ms** |

A **117× spread** between the cheapest and the most expensive resource, inside one session, on the
same device, for the same kinds of object. That spread is the contention signature from §3.2 again.

> **Superseded — read §9.1 first.** The mechanism here is a venus image-requirements cache miss,
> and the cost of a miss depends on the image *shape*: microseconds for a plain image, but
> 0.06–0.7 ms for the dmabuf/DRM-modifier shape a window texture takes.

This is now the **largest remaining item in a slow frame on our side**, and unlike the round trips
in §3.1 we cannot coalesce it away: these are genuinely new images for content that genuinely just
appeared. We have removed the ones that were avoidable (§4.4, §4.5). A `vkCreateImage` that did not
have to round-trip — or a way to create N images in one — would be worth more to us than anything
else on this list except §3.2.

### 3.4 Host-visible memory is roughly half the bandwidth of ordinary guest memory

A 4K wallpaper upload, straight out of the frame log:

```
48.0MiB uploaded in 8.46ms
```

That is **5.95 GB/s** into a mapped `HOST_VISIBLE | HOST_COHERENT` staging buffer. A standalone
measurement earlier in the same work put HOST_VISIBLE writes at ~5.6 GB/s against ~13.6 GB/s for a
plain guest-heap copy — consistent with write-combined memory, and not obviously a defect, but it
is large enough to matter: at 4K that one copy was a third of the frame it landed in.

> **Wrong, and corrected in §9.2.** The mapping is cached, not write-combined; both numbers above
> are first-touch page-fault cost, and the same buffer written a second time runs at ~58 GB/s. The
> fix is ours: stop allocating a fresh staging buffer per batch.

Note also that building a staging buffer at all is **five** host round trips —
`vkCreateBuffer`, `vkAllocateMemory`, `vkBindBufferMemory`, `vkMapMemory`, `vkUnmapMemory` — which
is why §4.4 collapsed N of them into one, and §4.6 moved the write to another thread.

### 3.5 We are blind to GPU time

Timestamp queries are advertised in full (`timestampPeriod = 1`, `timestampComputeAndGraphics`,
64 valid bits on the graphics queue) and **resolve every query to zero, with the availability word
set to 1**. So nothing in the API distinguishes "this pass took no measurable time" from "not
implemented"; there is no error to branch on. The device clock itself is fine —
`VK_EXT_calibrated_timestamps` returns a live `DEVICE` timestamp ticking at 1 ns.

Full write-up and a standalone reproducer:
[`venus-timestamp-gap.md`](./venus-timestamp-gap.md) /
[`venus-bugs/repro-vk-timestamp-query/`](./venus-bugs/repro-vk-timestamp-query) (`cargo run`, no
arguments; run against lavapipe on the same guest for the contrast).

**What it costs us:** every number in this document is CPU-side wall clock. We can say a submit
cost 1.8 ms; we **cannot** say how much of that was the GPU doing work and how much was the round
trip. That distinction is exactly the one this handoff is about, so fixing this would make the
next round of measurement much sharper — and would let us tell §3.2 (host scheduling) apart from
genuine GPU cost without guessing.

### 3.6 Blob churn — no longer fatal, still a cost

Reallocating GPU images every frame used to **abort the session**; that was fixed at the VMM level
and we have corrected our own docs and code comments accordingly. It remains a performance and
host-resource concern, and our caches are built to avoid it — but the failure mode is now "the seat
gets slower", not "the seat dies". Worth knowing because it changes the risk calculus on our side:
we shipped a change this week (deferring offscreen finishes) whose downside, had we got it wrong,
would have been exactly this churn.

### 3.7 Two dmabuf/gbm findings, already written up

Both reproduce deterministically, both have standalone reproducer crates, both are in
[`venus-bugs/README.md`](./venus-bugs/README.md):

1. **Mesa `venus`, bug** — `vkGetMemoryFdPropertiesKHR` returns `ERROR_INVALID_EXTERNAL_HANDLE`
   with `memoryTypeBits = 0x0` for a dmabuf that the immediately following `vkAllocateMemory` +
   `vkBindImageMemory` import and bind without complaint. A renderer that masks image requirements
   by the query result (the usual pattern) is left with zero valid memory types and wrongly refuses
   an importable buffer. We work around it by treating the query as best-effort.
2. **Mesa gbm / virtio-gpu, question** — `GBM_BO_USE_WRITE` flips an otherwise-identical
   allocation to `EINVAL`, and the legacy non-modifier `gbm_bo_create` paths `ENOENT`, so
   `create_buffer_object_with_modifiers2` is effectively the only usable allocation entry point.
   Nothing is blocked (allocate LINEAR without `WRITE`, then `gbm_bo_map`), but we would like to
   know whether that is the sanctioned path.

### 3.8 One thing that is *not* Venus's fault

Blur is real GPU work. On an overview frame covering 5.3× the output, `1 blur in 13.63ms` is the
entire wait, and earlier measurements put a blur submit at ~21 ms on an 8.2× frame. That cost
scales with fill rate, tracks content, and is ours to fix (fewer/cheaper passes, or caching the
blurred result). We mention it so the ratio in this document is not read as "everything is the
VMM" — it is not.

---

## 4. What we have already done on our side

Listed so the host-side reader knows the guest is not the low-hanging fruit any more. Every one of
these removes host round trips or moves them off the frame-critical path.

1. **The scanout submit no longer blocks the compositor thread.** Its fence is exported as a
   `sync_file` and handed to KMS as `IN_FENCE_FD` instead of being waited on. Measured on the seat:
   12 min of real use, **zero frames over budget** after the first three seconds, `submit` p50
   11.1 → 0.48 ms, no presentation penalty.
2. **Offscreen finishes no longer block it either** (widget bakes, window snapshots, effect
   buffers — several per frame). Measured across two sessions: offscreen fence waits of
   2.23–41.54 ms on 20 of 21 over-budget frames → **0.00 ms on every frame**, 216.9 ms of blocked
   time removed.
3. **Glyph-atlas copies ride the frame's own command buffer** instead of one submit per shaped
   line. A frame that shaped 13 new strings used to make 13 submits into one image; the site is
   gone from the logs entirely.
4. **One staging buffer per batch, not per texture.** Uploading the ~24 app-grid icons was 24
   staging buffers = ~120 host round trips (§3.4); it is now one.
5. **Symbolic icons are uploaded once, not once a frame.** A symbolic icon is an element, rebuilt
   each frame; the cache held the CPU raster but not the GPU upload, so a quick-settings popover
   cost ~9 uploads (~13 ms) per frame it was open. Now zero after warm-up.
6. **The wallpaper is staged on the decode thread.** The 8.46 ms host-visible write from §3.4 now
   happens on a worker; the render thread does image creation, copy and submit only.
7. **One persistent glyph atlas** instead of one image per string; memoized shaped runs and
   measured widths.

Still on our side, and being worked: coalescing the remaining per-frame uploads, blur cost (§3.8),
and text shaping/rasterization (one cold paragraph measured 19.42 ms — a debug-build CPU cost, not
a Venus one).

---

## 5. What would help most, ranked

1. **Do not make a guest fence wait absorb a host vsync (§3.2).** This is the biggest single number
   in the stack and the only one we cannot touch. If the host command stream can execute on demand
   rather than on a 60 Hz cycle — or if a guest submit can be acknowledged without waiting for the
   host's next frame — that ~13 ms goes away for everyone, not just us.
2. **Make resource creation cheap, or batchable (§3.3).** `vkCreateImage`/`vkAllocateMemory` at
   0.7–2.1 ms each is now our largest remaining in-frame cost, and it is irreducible from the guest
   because the resources are genuinely new. Even a batched "create N images" round trip would help;
   an asynchronous one would help more.
3. **Fix timestamp queries (§3.5).** Cheap relative to the others, has a reproducer, and it is what
   would let the *next* round of this work distinguish host scheduling from GPU execution instead
   of inferring it. Every number above would get sharper.
4. **Host-visible write bandwidth (§3.4)**, if there is anything to be had — ~6 GB/s is survivable
   now that the big write is off-thread, but it prices every texture upload.
5. **The two dmabuf/gbm findings (§3.7)**, which are correctness rather than performance and
   already have reproducers waiting.

## 6. The baseline, pinned

Written down so a **host-side change can be judged against it**. Everything below is one session on
the live seat, `2026-07-25T19:19:48Z` onward, guest build `v26.04-591-g4504c5b5`, VMM build as of
2026-07-25. The journal on this guest is persistent (`/var/log/journal`), so the raw lines survive
a reboot: `journalctl _UID=$(id -u gsrs) --since "2026-07-25 19:19"`.

| measure | baseline | where |
|---|---|---|
| per upload submit | **1.52 – 1.88 ms**, flat across 0.0 → 3.2 MiB | §3.1 |
| per created resource | **0.018 – 2.11 ms** (117× spread, same session) | §3.3 |
| worst frame's creation cost | `19 created in 40.02 ms` | §3.3 |
| host-visible write | **5.95 GB/s** (48.0 MiB in 8.46 ms) | §3.4 |
| scanout fence wait, back-to-back frames | 12.36 – 13.85 ms | §3.2 |
| offscreen fence wait | **0.00 ms** — already fixed guest-side, do not read as a host win | §4.2 |
| frames over the 16.67 ms budget | 6, all within 13 s of login; none after | — |

**Making the comparison valid.** Our own investigation notes that comparing two sessions is not an
A/B — session-to-session frame counts are dominated by how much the desktop was driven, and the
same build reported 0.8 and 12.3 over-budget frames per minute depending on the session. Across a
reboot that caveat is unavoidable, so:

- **Keep the guest binary identical.** `target/debug/niri` corresponded to `4504c5b5` when this
  was written (`2ca2e164` is docs-only and does not change it). If the guest is rebuilt as well,
  nothing in a before/after is attributable.

  > **The guest has since moved — read this before comparing (2026-07-25).** `target/debug/niri` is
  > now `v26.04-597-g63189b0c-modified`, and the tree carries `01dc9384`, which stops texture
  > uploads submitting at all. That changes *precisely* what §3.1 measures, so a naive
  > before/after across the VMM deploy would credit the host with a guest change. To attribute the
  > VMM honestly, either rebuild at `4504c5b5` and run that binary on both sides, or re-baseline on
  > the new VMM and treat §6 as the old-VMM record only. The `01dc9384` win is separately
  > measurable: `upload` submits per frame, which should now be ~0 either way.
- **Compare per-submit and per-resource numbers, not per-session totals.** The two rate columns in
  the table above are the ones that mean something; "frames over budget" is not, on its own.
- The most sensitive single number is **cost per created resource** (§3.3): it is large, it is
  frequent, and its 117× spread is the contention signature, so a host scheduling change should
  show up there first and most clearly.

**One clean negative result worth having.** `VN_DEBUG=result` has been set for the seat since
2026-07-10 and logs every non-`VK_SUCCESS` result the guest sees. Across the last two hours of
real use it produced **zero** such lines (the only Venus output at all is the 2-line device banner
per `vkCreateDevice`, 38 processes' worth). So the cost in this document is **not** error handling,
retries, or a fallback path being taken — it is plain round-trip latency on calls that all succeed.

## 7. Reproducing / re-measuring any of this

1. `NIRI_FRAME_LOG=1` is set for the gsrs session via
   `/home/gsrs/.config/environment.d/91-frame-log.conf`, and `NIRI_VK_ASYNC_SCANOUT=1` via
   `92-async-scanout.conf`. `systemd --user` only reads `environment.d` at start or
   `daemon-reload`, so a plain logout/login may not pick up a change.
2. Read frames with `journalctl _UID=$(id -u gsrs)` and filter on `frame on`. Sessions are
   delimited by the `frame logging on:` line; the build is on the `starting version` line above it.
3. The per-site submit attribution and the `N created` counter are what make §3.1 and §3.3
   readable — if a future build stops printing them, that is what to restore first.
4. **The headless backend cannot be used for any of this.** It has no real output; frames cost
   0.02 ms and never touch the paths this document is about. Frame-cost work on this fork is
   live-seat-only.

---

## 8. Reply from the VM/host side

**Written 2026-07-25**, from the host (`limina` repo, dev Mac), after reading this document and
§3.5's companion, then checking each claim against the actual sources: our `virglrenderer` fork
(`third_party/virglrenderer`), libkrun's virtio-gpu device
(`third_party/libkrun/src/devices/src/virtio/gpu`), the host Vulkan driver KosmicKrisp
(`mesa/src/kosmickrisp`), and guest Mesa `venus` (`mesa/src/virtio/vulkan`). Line citations
below are into those trees.

**The headline holds, and is sharper than stated.** "A call is cheap, a round trip is expensive"
is correct, and the constant is one number: **a host round trip costs ~1.5–2 ms, uniformly, for
everything that waits for a reply.** §3.1's 1.52–1.88 ms per upload submit and the expensive end
of §3.3's 0.018–2.11 ms are the *same* cost, not two phenomena.

Three of the five items are misattributed, though, and the ranking should change. Details follow;
the revised order is in §8.5.

### 8.1 §3.3 is not irreducible from the guest — it is a cache miss, and it is ~100×

This is the largest correctable item in the document.

Of the four calls §3.3 names, **two are already asynchronous** and cost no round trip at all:
`vkAllocateMemory` (`vn_device_memory.c:28-47` — submit-with-seqno, no reply wait, unless
`VN_PERF=no_async_mem_alloc`) and `vkCreateImageView` (`vn_image.c:833`).

The synchronous ones are `vkCreateImage` and `vkGetImageMemoryRequirements2`, and **only on a miss
in venus's image-requirements cache** (`vn_image.c:387-396`):

```c
if (cacheable && vn_image_init_reqs_from_cache(dev, img, key)) {
   vn_async_vkCreateImage(...);       /* no round trip */
   return VK_SUCCESS;
}
result = vn_call_vkCreateImage(...);              /* round trip 1 */
... vn_call_vkGetImageMemoryRequirements2(...);   /* round trip 2 */
```

The cache key is a BLAKE3 over the contiguous `VkImageCreateInfo` block from `flags` through
`sharingMode` (`vn_image.c:155-163`) — **which includes `extent`**. A novel width×height is a miss
even when format, usage and flags all repeat.

That is the 117× spread. **0.018 ms is the async path; 2.11 ms is the sync path.** It is not a
contention signature — it is hit versus miss, and the two populations sit ~100× apart because one
of them never talks to the host.

So the sentence "we cannot coalesce it away: these are genuinely new images for content that
genuinely just appeared" is true of the *content* and false of the *cost*. The round trip is
priced by the image **configuration**, not by its novelty.

**What to do, entirely guest-side:** bucket image extents — round allocations up to a 32 px or
64 px grid, or to powers of two — so that repeat surface sizes reuse a cached requirements entry.
The dmabuf path is cacheable too: `VkExternalMemoryImageCreateInfo` and the DRM-modifier structs
are all handled by the hasher (`vn_image.c:105-137`); only an *unrecognised* `pNext` disables
caching, and it bumps a counter (`dev->image_reqs_cache.debug.cache_skip_count`) if you ever want
to check.

**Confirm the mechanism before optimising:** run with `VN_PERF=no_async_image_create`, which
disables the cache entirely (`vn_image.c:185-199`) and makes *every* create synchronous. If the
cheap creates become expensive, the model is right.

### 8.2 §3.4 is not write-combining

KosmicKrisp exposes exactly **one** memory type (`kk_physical_device.c:1098-1104`):

```
HOST_VISIBLE | HOST_COHERENT | HOST_CACHED | DEVICE_LOCAL
```

and virglrenderer chooses the guest mapping as
`(coherent && cached) ? VIRGL_RENDERER_MAP_CACHE_CACHED : ..._WC` (`vkr_device_memory.c:906-908`).
Both bits are set, so the staging blob is mapped **cached**. Asking for
`HOST_VISIBLE | HOST_COHERENT` gets the cached type by construction — there is no other type to
pick, and venus additionally guarantees a coherent-cached type exists
(`vn_physical_device.c:1015-1018`).

The 5.95 vs 13.6 GB/s gap is real; the write-combining explanation is not. The first place to look
is the **guest kernel's** page protection on the blob VMA — a layer we own — not the host.

Cheap way to settle it from your side: a 20-line A/B that memcpys the same buffer into a mapped
blob and into ordinary anonymous guest memory, same thread, same size. If the blob side is ~2.3×
slower on a *cached* mapping, that is a guest-kernel finding and we will chase it there.

### 8.3 §3.5 — KosmicKrisp is exonerated; the gap is inside the VM path

§5 of `venus-timestamp-gap.md` asks for exactly one experiment first: *does the host driver pass
the same reproducer natively?* It does.

New probe, committed in the limina repo as `spikes/kk-timestamp-probe/` (`be0d0df`), run natively
on the host with no VM, against our own KK build. It covers three shapes, because a native app and
the guest hit **different** KK code paths:

| | path exercised | result |
|---|---|---|
| A | `vkGetQueryPoolResults` → `kk_GetQueryPoolResults` (CPU readback) | pass, delta 32 250 ns |
| B | `vkCmdCopyQueryPoolResults` → `libkk_copy_queries` (GPU kernel) | pass, delta 27 500 ns |
| C | as B, copy in a **separate** command buffer submitted alongside | pass, delta 28 000 ns |

C is the one that matters: **Mesa venus never calls `vkGetQueryPoolResults` on the host.** It
serves the guest from a guest-visible *feedback buffer* which it fills with a
`vkCmdCopyQueryPoolResults` recorded into a linked feedback command buffer
(`vn_query_pool.c:320` `vn_get_query_pool_feedback`, `vn_feedback.c:580-660`
`vn_query_feedback_cmd_record_internal`). That is the shape your session actually produces, and it
works natively.

Two further eliminations:

1. **The feedback path is not the culprit.** Re-running your reproducer on this guest with
   `VN_PERF=no_query_feedback` — which forces the synchronous `vn_call_vkGetQueryPoolResults`
   host round trip and bypasses the feedback buffer entirely — still returns `[0, 0]` with
   availability 1. **So the host-side query pool genuinely holds zero.**
2. **virglrenderer is a pass-through.** `vkr_query_pool.c:32` forwards `vkGetQueryPoolResults`
   verbatim; `vkr_command_buffer.c:519` and `:883` dispatch `vkCmdWriteTimestamp` and
   `vkCmdWriteTimestamp2` with no special handling.

The zero is itself diagnostic. KK's convention for a TIMESTAMP pool is `UINT64_MAX == unavailable`
(`kk_query_pool.c:28`, `libkk/kk_query.cl:30-49`), so a query that was reset and never written
reads **all ones**, not zero. Your `value 0, avail 1` therefore is *not* "the write was dropped
after a reset" — it is a pool report holding literal zero, which is fresh, untouched memory.

Remaining uncontrolled variable: the probe ran on an M1 Max, this guest runs on an M4 Pro. Closing
that, and instrumenting `kk_CmdWriteTimestamp2` / `kk_encoder_write_timestamp` in a local
enhanced-tier VM to see whether they are reached at all when the commands arrive via vkr replay
rather than via the loader, is the next step and is in progress on our side.

Nothing is being asked of the guest here. The note in §5 — "please do not fix this by making the
guest tolerate zeros" — is the right call and the all-zero heuristic should stay.

### 8.4 §3.2 — the stated mechanism is almost certainly wrong, and §3.5 gates it

"The host executes Venus's command stream on its own 60 Hz loop" does not match the host:

- **There is no timer in the host command path.** The venus ring thread is a poll/park loop with
  no periodic component whatsoever (`vkr_ring.c:503-616`); it either drains the ring, backs off,
  or parks on a condition variable until the guest notifies it.
- **Presents are fire-and-forget.** The VMM hands an IOSurface id to the UI process and returns;
  nothing in the flush path blocks on a drawable, a display link, or a vsync.
- **The one host mechanism that would pace a guest fence to a refresh is off.** Fence-accurate
  present parks a flush until the frame latches, and it is opt-in
  (`virtio_gpu.rs:1888 fence_present_enabled`) — it is **not** set in this VM's `limina-vmm`
  environment (checked on the live process). Nothing host-side is latching you to a refresh.
- **A venus fence wait has no host CPU in it.** After `vkQueueSubmit` the guest polls a word that
  the *GPU* writes; the host CPU is not in the loop. A 13 ms fence wait therefore means the GPU
  genuinely finished 13 ms later.

The innocent explanation you cannot presently exclude: at 3840×2160 with 1.7–2.0× overdraw and
138–173 draws, **the frame's GPU work really is ~13 ms**, and the scanout submit is last, so it
waits behind all of it. §3.8 already reports a single blur at 13.63 ms. The sparse frames at
3.71–5.45 ms are then simply an idle GPU with nothing queued ahead. That fits the data at least as
well as the vsync reading — and note 13 ms is not 16.67 ms.

Distinguishing the two is *precisely* what §3.5 buys. **Items #1 and #3 on the §5 list are the
same item, and #3 comes first.** Until GPU time is available, #1 is unfalsifiable from either side
of the boundary.

One thing that is true and worth knowing: the venus ring is **one thread per context**, and it
decodes the guest's stream and encodes host Vulkan serially (`vkr_ring.c:503-616`). So the
"priced by contention" observation in §3.2 is real — but the contention is largely your own stream
contending with itself, not other tenants.

### 8.5 Revised ranking

1. **Fix timestamp queries.** Not a second-order tool — it gates the biggest item on the list.
   Host-side work, in progress; nothing needed from the compositor.
2. **Bucket image extents (§8.1).** Guest-side, no host change, ~100× on what is currently the
   largest remaining in-frame cost. Verify with `VN_PERF=no_async_image_create` first.
3. **Re-measure §3.2 with GPU time in hand.** It may evaporate; if it does not, the residue will
   be attributable rather than inferred.
4. **Verify §3.4 in the guest before anyone invests.** The host says cached; the A/B in §8.2
   decides whether there is a bug at all.
5. **The two dmabuf/gbm findings (§3.7)** stand as filed — correctness, with reproducers, unchanged
   by anything above.

The §6 baseline is the right thing to hold against, and the per-resource column is the one to
watch: if §8.1 is right, cost-per-created-resource is where a change will show up first and
hardest — and the first mover on it is the guest, not the host.

---

## 9. Guest-side reply: three probes

**Written 2026-07-25**, from the guest, after reading §8. Rather than argue from the frame log
again, each disputed item was turned into a standalone experiment that runs in this guest with no
compositor, no seat and no display:
[`venus-probes/probe-venus-costs/`](./venus-probes/probe-venus-costs) —
`cargo run --release -- [image|memory|fence|idle|all]`. The Mesa and virglrenderer citations below
are into `~/Projects/mesa` (26.2-branchpoint+280) and `~/Projects/virglrenderer` (`986b5fc5`), the
same trees the host side read.

**Read the probe as a set of floors, not as compositor costs.** It holds one context, one queue,
one thread; it never touches KMS; and this guest is also running a 60 Hz desktop of its own, so the
host GPU and the host renderer are never quiet. Where a number is noisy, `min` is quoted, because
the least-contended sample is the closest thing to a clean read of the stack.

**The headline changes.** "A host round trip costs ~1.5–2 ms, uniformly" does not survive
measurement: **an empty submit plus a fence wait, back to back, costs 0.016–0.03 ms.** There is no
fixed millisecond round trip anywhere in this stack. Every millisecond-scale number in §3 is
therefore a *queueing* cost — something the call waited behind — and not the price of talking to
the host. That reframes both documents.

### 9.1 §8.1 — the cache is real; the ~2 ms is not, and the expensive shape is the dmabuf one

The mechanism as described checks out in the source we have here, including the detail that
matters: the BLAKE3 key covers the contiguous `flags..sharingMode` block (`vn_image.c:154-165`),
so `extent` is part of it, a hit takes `vn_async_vkCreateImage` and a miss takes two synchronous
calls (`vn_image.c:387-403`). The cache holds 500 entries with LRU eviction
(`IMAGE_REQS_CACHE_MAX_ENTRIES`, `vn_image.c:25`).

Measured, 200 creates per row:

| image shape | `vkCreateImage`, median | p95 |
|---|---|---|
| plain, same extent repeated (hit) | **0.0002 ms** | 0.0003 |
| plain, novel extent every time (miss) | **0.0032 ms** | 0.0099 |
| plain, those same sizes bucketed to a 64 px grid | **0.0002 ms** | 0.0003 |
| **dmabuf-shaped** (external + DRM-modifier list), novel extent (miss) | **0.06 – 0.31 ms** | 0.37 – 0.69 |
| **dmabuf-shaped**, same extent repeated (hit) | **0.0002 ms** | 0.0004 |

`vkGetImageMemoryRequirements2` is 0.0000 ms on both sides of the cache, which is expected: on a
miss the round trip happened inside `vkCreateImage`, and the query only reads what was stored.

Three conclusions, in order of how much they change:

1. **Hit versus miss is real and large in ratio — but a plain miss costs 3 µs, not 2 ms.** So
   §3.3's 0.72–2.11 ms per resource is *not* explained by the miss alone. A miss is a synchronous
   ring round trip, and a synchronous ring round trip is microseconds when the ring is hot. The
   millisecond version of it is §9.4.
2. **The expensive miss is the dmabuf/DRM-modifier shape — 20–100× a plain miss** on the same
   device, in the same process, at the same extents. That is the shape *every client window
   texture* takes here, so it is the one that lands in real frames. It is cacheable (the repeat
   row is a hit), so this is host-side cost inside a modifier-image create, and the probe
   reproduces it in ~30 lines.
3. **Bucketing works, and we will do it — but it cannot reach the dmabuf path.** For images we
   own (offscreen bakes, blur levels, atlases) the extent is ours to round and the measurement
   above says rounding removes the miss entirely. For an imported client buffer the extent is
   dictated by the client's dmabuf; we cannot round it without breaking the import. So the
   guest-side fix covers the cheap misses and not the expensive ones.

**Your correction on `vkAllocateMemory` and `vkCreateImageView` is accepted, and the concern that
prompted the check did not materialise.** Measured at a cache-hit extent: `vkAllocateMemory`
0.0001 ms, `vkBindImageMemory` 0.0000 ms, `vkCreateImageView` 0.0001 ms. The reason the
guest-VRAM path (`vn_device_memory.c:325`, which does have a `vn_ring_roundtrip` in it) does not
bite is that venus defers bo creation to first map — "we don't want to blindly create a bo for
each HOST_VISIBLE memory as that has a cost" (`vn_device_memory.c:459-470`) — and our image memory
is never mapped. Our own code comments said otherwise; they have been corrected.

### 9.2 §8.2 — you are right, it is cached, and the real cause is first touch

Confirmed from the guest, and more strongly than the read/write ratio you suggested:

| | mapped blob | ordinary guest heap |
|---|---|---|
| sequential write | 61.7 GB/s | 61.2 GB/s |
| sequential read | 57.8 GB/s | 58.2 GB/s |
| page-strided dependent read | 25.6 ns/access | 29.6 ns/access |

A write-combined or Normal-NC mapping could not produce that read column or that latency. **The
staging mapping is cached; §3.4's write-combining explanation is withdrawn.**

The interesting part is what the 5.95 GB/s actually was. Writing 64 MiB into a *freshly mapped*
blob, then writing the same 64 MiB again into the same mapping:

```
round 0: map 0.21 ms   first 8.97 ms (7.48 GB/s)   second 1.13 ms (59.25 GB/s)
round 1: map 0.20 ms   first 9.75 ms (6.88 GB/s)   second 1.14 ms (59.02 GB/s)
round 2: map 0.21 ms   first 8.59 ms (7.81 GB/s)   second 1.14 ms (58.86 GB/s)
```

**Every §3.4 number is a first-touch number.** 5.95 GB/s is the fault path; the mapping itself
does ~58 GB/s, an order of magnitude more. An ordinary fresh guest allocation faults at 6.7–21.6
GB/s across runs — sometimes as slow as the blob, sometimes 2.7× faster — which is where the
original "~5.6 vs ~13.6 GB/s" pair came from: it compared a cold blob against a warmer heap.

So:

- **Nothing is asked of the host here, and item #4 on the §8.5 list can be closed.** The residual
  is whether a blob VMA can fault as fast as an anonymous one at its best; that is the guest-kernel
  question you predicted, and it is worth at most ~2×.
- **The 8× is real but narrower than it first looked, and the correction is ours.** First touch is
  paid per *allocation*, not per mapping: unmapping and re-`vkMapMemory`ing the same
  `VkDeviceMemory` costs 0.001–0.003 ms and its next write runs at full speed, because venus keeps
  the bo's mapping alive underneath. So our already-reusable `Staging` (the shm path) was never
  paying it, and a staging **pool** only helps paths that allocate a fresh buffer per upload. The
  wallpaper is the one that does, at 48 MiB — worth 9 ms → 1.1 ms there, and nothing on the small
  per-frame uploads, whose first touch is ~15 µs.
- One correction to §3.4's own arithmetic: of the "five host round trips" to build a staging
  buffer, only `vkMapMemory` costs anything measurable (0.20 ms for 64 MiB — it is where the bo
  gets created, per `vn_device_memory.c:471`); the create, allocate, bind and unmap are
  microseconds.

### 9.3 §8.4 — three of your four bullets stand, one does not, and GPU time is now available

**The vsync reading in §3.2 is withdrawn.** Grading GPU work linearly and watching the fence wait
follow it settles it. 3840×2160 image copies (≈33 MiB each way), back-to-back submits, 120 rounds
per row:

| copies | fence wait, min | median |
|---|---|---|
| 0 | **0.017 ms** | 0.029 |
| 1 | 0.421 ms | 0.543 |
| 2 | 0.693 ms | 0.813 |
| 4 | 1.246 ms | 1.404 |
| 8 | 2.317 ms | 2.590 |

Least squares over the minima: **wait ≈ 0.094 ms + 0.283 ms per copy**. The slope corresponds to
235 GB/s of copy traffic, which is a believable number for this device — so the wait is tracking
real GPU execution, linearly, with a ~0.1 ms floor. Nothing is being quantised to 16.67 ms, and a
13 ms wait does mean roughly 13 ms of queued GPU work. Your innocent explanation is the right one.

Two things follow that are worth more than the retraction:

- **We have GPU time now, without timestamp queries.** A differential — same submit shape, graded
  work, fit the slope — measures GPU execution to a fraction of a millisecond. It does not
  attribute *within* a frame the way a timestamp would, so §3.5 is still worth fixing, but it is no
  longer a blocker for anything. Item #1 on the §8.5 list can be worked without waiting on item #3.
- **One bullet in §8.4 is wrong:** *"A venus fence wait has no host CPU in it. After
  `vkQueueSubmit` the guest polls a word that the GPU writes."* That describes semaphore and query
  *feedback*, not a `VkFence`. `vn_WaitForFences` → `vn_renderer_wait` → `virtgpu_wait` is a
  **DRM syncobj wait ioctl** (`vn_sync.c:174-225`, `vn_renderer_virtgpu.c:762-796`), and on the
  host the syncobj is signalled by a per-queue CPU thread: `vkr_queue_thread` sits in
  `vk->WaitForFences(...)` and then calls `retire_fence` (`vkr_queue.c:149-192`), with syncs
  retired strictly FIFO. So a fence wait contains a host CPU thread wake, a retire through the
  VMM, a guest interrupt and a guest thread wake — several scheduling events, not a memory poll.
  It does not change your conclusion, but it is the reason an idle pipe can cost milliseconds
  (§9.4), and it means "13 ms of wait" ≠ "13 ms of GPU" as an identity.

For completeness, the other §8.4 bullets match what we see: submits are cheap and pipelined
(K empty submits then one wait: K=1 0.061 ms, K=16 0.334 ms of *total* time — about 18 µs per
extra submit, nowhere near a round trip each), and your note that the ring is one serial thread per
context matches the shape of everything above.

### 9.4 The new item: the ring goes to sleep between our frames, and waking it costs milliseconds

This is the mechanism we think replaces both the vsync guess and "it is just GPU work", for the
specific case of a compositor that idles between frames.

The host ring thread backs off while the ring is empty — 16 yields, then `clock_nanosleep` starting
at 10 µs and doubling (`vkr_ring_relax`, `vkr_ring.c:188-208`) — and once `idleTimeout` has passed
with nothing to decode it parks on a condition variable that only a guest notify can signal
(`vkr_ring.c:265-290`). The guest sets that timeout to **1 ms**
(`VN_RING_IDLE_TIMEOUT_NS`, `vn_ring.c:18`) and rate-limits its own wake-up notifies to at most one
per millisecond (`vn_ring.c:467-479`).

Sweeping the idle gap in front of an *identical* zero-work submit, 400 rounds per cell, `spin`
keeping the guest thread hot on-core and `sleep` parking it too:

| gap before submit | spin: min / median | sleep: min / median |
|---|---|---|
| 0 µs | **0.048 / 0.090 ms** | 0.052 / 0.078 |
| 200 µs | 0.043 / 0.680 | 0.030 / 0.057 |
| 600 µs | 0.024 / 0.052 | 0.023 / 0.056 |
| **1000 µs** | **0.574 / 0.880** | **0.331 / 0.627** |
| 1400 µs | 0.278 / 0.366 | 0.037 / 0.068 |
| 2000 µs | 0.037 / 0.045 | 0.035 / 0.066 |
| 5000 µs | 0.041 / 0.054 | 0.044 / 1.121 |
| 16700 µs | 0.046 / **0.965** | 0.062 / **1.286** |

Two readings we are confident in, and one we are not:

- **The penalty is host-side.** `spin` and `sleep` agree throughout, so it is not guest scheduling,
  not DVFS and not a thread migration — the only thing that differs is how long the host ring sat
  empty.
- **An idle gap of a frame's length costs ~1 ms on a submit that does nothing.** At 16.7 ms of
  idle the median is 0.97–1.29 ms against 0.08–0.09 ms back-to-back: a **10–15× tax on the first
  submit after a frame boundary**, which is exactly the position our per-frame uploads occupy.
- **The shape between 1 ms and 5 ms is not clean and we are not going to claim it is.** The bump at
  exactly the 1000 µs `idleTimeout` is suggestive, the non-monotonicity above it is not explained,
  and the bimodality (min 0.046 ms against median 0.97 ms at 16.7 ms) says two different paths are
  being taken. The notify rate-limit at `vn_ring.c:473` is the obvious suspect for a submit that
  finds the ring parked but is not allowed to notify it; we have not proven that.

**This reinterprets §3.1.** "1.52–1.88 ms per upload submit, flat across 0.0 → 3.2 MiB" is the
signature of a fixed wake cost, not of a payload cost — and our upload path guarantees we pay it:
`Gpu::run_commands` submits and then blocks on the fence (`niri-vk/src/gpu.rs:681-736`). Any wait
longer than 1 ms lets the ring park, so the *next* submit pays the wake, whose wait parks it again.
Every blocking wait we do buys a wake for the call after it.

Both sides have something to do here. Ours: stop blocking per upload (the same medicine as §4.1
and §4.2, applied to the remaining sites). Yours, if it is cheap: the relax/park policy is a
`TODO do better` in the source, and a shorter maximum backoff — or a park that a submit can wake
without the 1 ms notify rate-limit — would take the tax off every guest that idles between frames,
which is every compositor.

### 9.5 Revised ranking, with the mover named

1. **Stop paying a submit and a fence wait per texture upload.** Ours, and **done** — an
   `import_memory` now stages its pixels and lets the next frame's command buffer carry the copy,
   the same way glyph uploads and deferred dmabuf acquires already did. That is what the seat's
   `9 upload in 16.22ms` frames were spending, and it removes the §9.4 wake tax those waits were
   buying for each other. Not yet live-validated. (Staging *reuse* is a separate, smaller item —
   see the correction in §9.2.)
2. **The ring idle/wake tax (§9.4).** Shared. We stop blocking per submit; you look at the relax
   and notify policy. This is what §3.1 and §3.2 were both circling.
3. **Cheapen a dmabuf/DRM-modifier `vkCreateImage` miss (§9.1).** Yours, with a reproducer. 20–100×
   a plain miss, on the path every window texture takes, and the one part of §3.3 the guest cannot
   bucket away.
4. **Bucket the extents of images we own (§9.1).** Ours, small, measured.
5. **Timestamp queries (§3.5).** Yours, in progress — no longer gating anything, since §9.3's
   differential gives GPU time, but still what makes attribution *within* a frame possible.
6. **The two dmabuf/gbm findings (§3.7).** Unchanged, still filed, still have reproducers.

The §6 baseline still stands as the thing to hold against, with one amendment: **cost per created
resource is no longer the most sensitive number.** It moves for reasons on both sides of the
boundary now (cache hit rate, image shape, and whether the ring happened to be awake), so a
host-side change is better judged on the §9.3 fit — intercept and slope, same probe, same
arguments — which is reproducible on demand and does not need a live seat.

### 8.6 §3.7 update — issue 1 is fixed, issue 2 is answered (2026-07-25)

Both re-run on the current stack; full write-up appended to
[`venus-bugs/README.md`](./venus-bugs/README.md).

- **`vkGetMemoryFdPropertiesKHR` (bug): FIXED and already deployed.** Host-side in our
  `virglrenderer` fork since 2026-07-04 (`patches/virglrenderer/0024`), which cites this issue by
  name — venus routes the query through `vkGetMemoryResourcePropertiesMESA`, whose handler gated on
  a Linux-dmabuf fd type that our IOSurface/shm-backed resources never have. The query now agrees
  with the import, so the `image_bits & fd_props_bits` masking pattern is safe and the best-effort
  fallback can be dropped.
- **gbm (question): answered, and it is two separate things, neither of them virtio-gpu.**
  `GBM_BO_USE_WRITE` → `EINVAL` is generic Mesa gbm — `USE_WRITE` is routed to the KMS dumb-buffer
  path, which only accepts `CURSOR|ARGB8888` or `SCANOUT|XRGB8888/XBGR8888` and fails before any
  driver call. So yes: allocate LINEAR without `WRITE`, then `gbm_bo_map`, is the sanctioned path.
  The legacy `gbm_bo_create` `ENOENT`, though, is **specific to zink** — the same binary on the
  same node allocates fine under `virtio_gpu`/virgl. That makes it a zink-on-venus gap worth
  filing, not a property of this stack. And the `ENOENT` itself is a **stale errno**: gbm's failure
  path returns `NULL` without setting `errno`.

---

## 10. Host-side reply to §9: the wake chain is now measured end to end

Written 2026-07-25, after §9. Two of the four open items from §8.5 are closed, and §9.4 is
answered — not in the direction either side expected.

**Everything below is instrumented in the VMM you are about to be running.** See §10.4 for how to
read it.

### 10.1 §9.4 — the host round trip is ~0.08 ms; ~90% of your ~1 ms is inside the guest

§9.4's suspect was mesa's `vkNotifyRingMESA` rate limit leaving work in a parked host ring. That
is disproved, and so is the broader conclusion that the cost is host-side.

Two probes now time every hop the host can see, both cut on the **same idle-gap buckets** you
sweep over. `GPUWAKE` (libkrun) covers guest doorbell → gpu worker scheduled → control queue
drained → used-queue IRQ raised → **your own virtio ISR acknowledging it**. `RINGWAKE`
(virglrenderer) covers `cnd_signal` → venus ring thread running → first command decoded. Between
them there is no unstamped host hop left.

Run against your `probe-venus-costs idle` on an enhanced F44 guest (M1 Max), p50/p95/max in ms:

```
[GPUWAKE idle 1-4ms ] n=391 kick->wake 0.019/0.110/1.833 | wake->drained 0.036/0.085/1.774 | drained->signal 0.003/0.008/0.025 | irq->ack n=371 avg 0.018
[GPUWAKE idle 4-16ms] n=264 kick->wake 0.021/0.143/3.267 | wake->drained 0.046/0.120/0.662 | drained->signal 0.004/0.010/0.026 | irq->ack n=263 avg 0.019
[RINGWAKE idle 1-4ms] n=118 signal->resume avg 0.009 | resume->decode avg 0.000 | lost_signal=0
```

Adding the p50s: **the complete host round trip is ~0.076 ms**, from your doorbell write to your
ISR ack. The same guest sweep in the same window measured 0.43–1.48 ms at those gaps.

So **roughly 90% of what you measure is spent inside the guest**, on one side or the other of the
host window — ioctl entry, virtio descriptor setup and driver locking before the doorbell, or the
driver completing the request, waking the blocked task and the scheduler putting it back on a CPU
after the ack. The host probes cannot split entry-side from exit-side; they can only say neither
is host. **That split is the one measurement still missing, and it is yours**: a stamp at ioctl
entry and one at return, paired against our `kick` and `irq->ack`, would locate it exactly.

This does not contradict your spin-vs-sleep result so much as reinterpret it. Spinning keeps the
guest *thread* runnable, but it does nothing for driver-side and interrupt-path work that still
has to happen on the guest side of the boundary — so "identical under spin and sleep" does not
imply "host-side".

Three supporting facts, each of which independently closes off a theory:

- **`lost_signal=0` in every bucket of every run.** Every ring wake had a matching `cnd_signal`.
  No notify was ever swallowed, so the rate-limit hypothesis is dead on direct evidence. (We had
  already built a bounded-`cnd_timedwait` guard against it, measured it, found it changed
  nothing, and reverted it — this is why.)
- **`resume->decode` is ~0 throughout** (max 5 µs). Once the ring thread runs, it works.
- **Nothing on the host is idle-dependent.** A fixed-work control (identical arithmetic, no lock,
  no syscall) runs on the worker thread beside each sample. Within a report window it is flat
  across the idle buckets — and so are the hops. Between windows it moves ~4× as the machine's
  clocks move, and the hops track *that*. So the host's own variation is CPU speed, not code, and
  the growth-with-idle-gap that §9.4 is actually about is not produced here.

### 10.2 §9.2 blob first-touch — closed, and it is fault *cost*, not fault count

Counted the faults rather than only timing them, on the same guest:

```
  venus blob         cold 10.50 ms ( 5.95 GB/s)  4096 faults  16384 B/fault   warm 20.70 / 60.55 GB/s
  anon mmap          cold  4.93 ms (12.68 GB/s)  4096 faults  16384 B/fault   warm 50.15 / 57.78 GB/s
  anon MAP_POPULATE  cold  1.05 ms (59.64 GB/s)     0 faults      0 B/fault   warm 47.27 / 57.67 GB/s
```

**Identical fault counts** — exactly one per 16 KiB base page, no transparent huge pages on either
arena. So the blob is not penalised by taking *more* faults; subtracting the `MAP_POPULATE` floor
gives **2.31 µs per blob fault vs 0.95 µs per anon fault**. Both arenas converge on ~58 GB/s warm,
which reconfirms your §9.2 and our §8.2: the mapping is cached and full speed.

The host is not mistreating this memory, so **your size-bucketed staging reuse is the fix that
matters** (~8× on the wallpaper case). We have a smaller host-adjacent option — faulting around,
or populating the VMA at `mmap` time, worth ~9 ms per 64 MiB but only on first touch — parked
behind yours.

One correction to our own first pass, since it would have contradicted your §9.2: we initially
reported the blob warm at 19–20 GB/s against anon at 58. That was an artifact of a **single** warm
pass measured straight after a 4096-fault storm. A second warm pass reaches 57–60 GB/s on every
arena. One warm sample cannot distinguish a slow mapping from a warming one.

### 10.3 §9.3 — the vsync/`ARM_ARM_ARM` reading is withdrawn on our side too

Nothing to add: your retraction matches what we see. Noted so the ranking in §8.5 is not read as
still standing.

### 10.4 Reading the probes on the deployed VMM

Both are **on by default** in this build (temporary — they will be removed once we have the data).
The tables go to the worker's stderr, which for a managed VM is:

```
<vm>.liminavm/logs/supervisor.log      # truncated per run
grep -E 'GPUWAKE|RINGWAKE' <that file>
```

One line per idle bucket per ~5 s, printed only for buckets that saw traffic.

- `LIMINA_WAKE_PROBE=0`, `LIMINA_RING_WAKE_PROFILE=0` turn them off.
- `LIMINA_WAKE_PROBE=calib` adds the fixed-work control. It costs ~0.12 ms of the gpu worker's
  critical section per doorbell, so it is not the default — use it when you want to know whether
  a difference is the code or the machine, not for absolute numbers.

Two things to know before drawing conclusions from a line:

- **The tables aggregate all virtio-gpu control-queue traffic in the window**, not just your
  submits. On a real desktop your compositor's own traffic is in there. The bucketing separates
  it partially, not cleanly.
- **`irq_coalesced` will be ~250× the `irq->ack` sample count, and that is correct.** The
  used-queue interrupt is a single level-triggered SPI; the guest coalesces one ISR entry and one
  ack over hundreds of raises. `irq->ack` deliberately times the raise that found the line idle —
  the one that actually causes the ISR entry.
- Ignore the first windows after boot (multi-second `wake->drained`, an `irq->ack` in the
  thousands of ms): that is start-of-day setup and an arm sitting through an idle stretch.

Also fixed in this build, unrelated to timing: a `RINGWAKE` max that underflowed to `1.8e13 ms`
when a notify raced in behind an already-woken ring thread.

### 10.5 Revised state of the §8.5 list

| item | state |
|---|---|
| 1. timestamp queries | **root-caused and fixed** (§8.3 + M4 Pro split-command-buffer fix); this build carries it |
| 2. image-requirements cache | yours |
| 3. §9.4 idle tax | **answered**: ~0.08 ms host, ~90% guest; the remaining split is a guest-side stamp |
| 4. blob first-touch | **closed**: fault cost, not count; staging reuse is the fix |
| 5. dmabuf/gbm | **closed** in §8.6 |

---

## 11. Guest-side reply to §10: the split you asked for does not exist

Written 2026-07-25, on the deployed VMM, right after the reboot. Three results: one of §10.5's
"fixed" items does not reproduce as fixed, the §10.1 measurement you asked us for turned out to be
measuring the wrong boundary, and the cost picture is otherwise unchanged.

New probe: [`venus-probes/ioctl-split/`](./venus-probes/ioctl-split) — an `LD_PRELOAD` shim that
stamps `ioctl` and `clock_nanosleep`. Built for §10.1's request and repurposed when it found
nothing to stamp.

### 11.1 Timestamp queries still resolve to zero on this build

§10.5 lists them as *root-caused and fixed, this build carries it*. On this guest, after the
reboot, they do not. `repro-vk-timestamp-query` now sweeps the whole matrix, because the fix was
described against `kk_CmdWriteTimestamp2` while the reproducer (and our renderer) had only ever
called the 1.0 entry point — two independent axes, so both are tested:

```
  vkCmdWriteTimestamp      GetQueryPoolResults    -> [0, 0]  zero
  vkCmdWriteTimestamp      CopyQueryPoolResults   -> [0, 0]  zero
  vkCmdWriteTimestamp      Copy (separate cbuf)   -> [0, 0]  zero
  vkCmdWriteTimestamp2     GetQueryPoolResults    -> [0, 0]  zero
  vkCmdWriteTimestamp2     CopyQueryPoolResults   -> [0, 0]  zero
  vkCmdWriteTimestamp2     Copy (separate cbuf)   -> [0, 0]  zero
```

**The harness is not the problem.** The same binary, on the same guest, against lavapipe
(`VK_DRIVER_FILES=…/lvp_icd.aarch64.json`) reports **WORKS on all six**, with sane deltas
(45–130 µs). So the matrix exercises each combination correctly and the zero is venus-specific.

We cannot see your build metadata from in here, so we cannot tell whether this VM is missing the
fix or the fix does not cover these shapes. The reproducer is the discriminator either way — a
shape that starts reporting `WORKS` is one we can move `NIRI_FRAME_LOG=gpu` onto.

### 11.2 There is no ioctl to stamp: the wait never enters the kernel

§10.1 asked for "a stamp at ioctl entry and one at return, paired against our `kick` and
`irq->ack`". We built exactly that. It recorded **nothing**, and the reason is the finding.

Every ioctl a 20-round run makes, by DRM `nr` (type `'d'`):

```
  23 × 0x42 EXECBUFFER   6 × 0x43 GETPARAM   3 × 0x4a RESOURCE_CREATE_BLOB
   3 × 0x41 MAP          3 × 0x09 GEM_CLOSE  1 × 0x49 GET_CAPS  1 × 0x4b CONTEXT_INIT
```

**No `DRM_IOCTL_SYNCOBJ_WAIT`, no `DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT`, no `DRM_VIRTGPU_WAIT`** —
across any run we have made. A guest fence wait on this stack does not block in the kernel at all.
So the entry-side/exit-side split does not exist: there is no ioctl entry, no virtio descriptor
setup, no used-queue IRQ waking a blocked task, and no scheduler putting it back on a CPU. That
whole half of §10.1's guest-side theory space is empty.

What it does instead, measured with the same shim over 200 waits at a 16.7 ms idle gap:

| | |
|---|---|
| `clock_nanosleep` calls | **576** — ~2.9 per fence wait |
| requested, every single call | **0.160 ms** |
| actually slept, median | **0.291 ms** (0.131 ms overshoot) |
| → time inside `clock_nanosleep` | ~**0.85 ms** of a 1.23 ms median wait |

**The guest is polling, and its polling quantum is coarser than your entire round trip.** 160 µs is
`vn_relax`'s `base_sleep_us` (`vn_common.c:180-222`), the first rung of its backoff, and the only
rung these waits reach. Your complete host round trip is 0.076 ms. So the answer is ready at
roughly half a quantum, and the guest sleeps through it — twice more, on average, before looking.

That is consistent with everything both sides measured, and it explains the two results that
looked contradictory:

- **~90% guest-side (§10.1) — confirmed**, and now located: not driver or interrupt latency,
  but `~2.9 × 291 µs` of deliberate sleeping in mesa's userspace poll loop.
- **Spin and sleep gaps behave identically (§9.4) — explained.** Keeping our thread hot cannot
  help, because the delay is not the scheduler failing to run us. It is mesa choosing not to look.

Two caveats we would rather state than have you discover:

- **The exact `vn_relax` call site is not pinned.** There is no `VN_RELAX_REASON_FENCE`, and the
  yield:sleep ratio we observe (~5 yields per wait) does not cleanly match either profile's
  `busy_wait_order`. We are also reading mesa `26.2-branchpoint` source against a guest running
  **26.1.4**. The 160 µs constant and the absence of a wait ioctl are measured facts; which loop
  issues them is inference.
- **The 131 µs overshoot on a 160 µs sleep is ours to chase** — guest-kernel timer slack, not
  yours. It roughly doubles the cost of every rung.

### 11.3 What follows

Nothing here needs the host, which is the useful part:

1. **The lever is mesa's poll granularity**, and it is at least 2× off the hardware. A first rung
   near your 76 µs round trip — or a blocking wait through the syncobj path that already exists in
   `virtgpu_wait` but is not being taken — would remove most of the ~1 ms. That is a guest-stack
   change (mesa, or a `vn_relax` tunable), not a compositor one.
2. **Timestamp queries stay open** on our side, with the matrix above as the check.
3. **Our own fix stands regardless**: `01dc9384` stops texture uploads waiting at all, so the
   frames that paid 9 of these waits now pay none. The cheapest wait is the one not taken.

### 11.4 The cost probes on the new VMM: unchanged

Re-run of `probe-venus-costs`, for the record — nothing here moved, which is expected given §10.1
found the cost is guest-side.

| measure | pre-deploy | on this build |
|---|---|---|
| empty submit + fence wait, back-to-back | 0.017 / 0.029 ms (min/median) | 0.020 / 0.031 |
| graded-work fit | 0.094 ms + 0.283 ms/copy (235 GB/s) | **0.108 ms + 0.284 ms/copy (234 GB/s)** |
| plain `vkCreateImage`, cache miss | 0.0032 ms | 0.0038 |
| dmabuf-shaped miss | 0.06–0.31 ms | 0.0596 |
| idle-gap 16.7 ms, empty submit | 0.046 / 0.965 ms | 0.041 / 0.928 |

**`vkGetMemoryFdPropertiesKHR` (§8.6) is confirmed fixed here** — `repro-vk-getmemfdprops` reports
`query and import agree → FIXED on this stack`, which is the gate in `venus-bugs/README.md`. The
guest-side fallback removal is unblocked.

---

## 11. Timestamp queries on M4 Pro: partially fixed — **discard zero samples, do not average them**

Written 2026-07-25, after the fix from §8.3 was deployed to couve and **did not work**. Read this
before trusting any GPU timing the compositor collects on that machine.

### The short version

If you read a timestamp query and get **0**, it is not a fast GPU — it is a **lost sample**.
Treat `0` as "no data" and drop it. Do not average it in, do not feed it to a min/max, do not let
it into a percentile. On an M4 Pro roughly **1 in 5 samples is lost** even with the fix in place,
and because the lost value is `0` rather than an error, every statistic silently skews toward
zero. A frame time that reads suspiciously good is the failure mode to watch for.

Two queries bracketing a region can each be lost independently, so guard the pair: if either end
is `0`, discard the interval rather than reporting a nonsensical (possibly negative, possibly
huge) delta.

`vkGetQueryPoolResults` still reports `avail = 1` for a lost sample, so **availability is not a
validity check here**. The value itself is the only signal.

### Why it is only partial

The original diagnosis in §8.3 — "an M4 Pro cannot resolve a counter sample from the command
buffer that took it" — was written from a single probe run and was over-fitted. Repeating the
Metal probe 50 times on couve:

| shape | ok | zero | failure |
|---|---|---|---|
| resolve in the same command buffer (before the fix) | 3 | 47 | **94%** |
| **resolve in a separate command buffer — what shipped** | 41 | 9 | **18%** |
| separate command buffer **+ wait for completion** | 50 | 0 | **0%** |
| **CPU** `resolveCounterRange` | 50 | 0 | **0%** |

So the defect is **intermittent, not deterministic**, and the shipped fix improves it from 94% to
18% rather than curing it. The reasoning it rested on — that command buffers run in commit order
— is the wrong guarantee: the sample becomes visible at command-buffer **completion**, not at
execution.

A second bug made it worse in practice. The runtime detection that decides whether to apply the
workaround took **one** sample and defaulted to "unaffected" — and its own probe shape passes 4%
of the time, so a few percent of boots cached "this GPU is fine" for the whole session and never
applied the fix at all. That is what you were hitting: 8/8 guest runs read `[0, 0]`, which is
~1e-6 at an 18% rate but entirely expected if the workaround was never active. Fixed by starting
at "affected" and requiring an unbroken run of 8 clean samples to clear it.

### What comes next

The real fix is to resolve the counter on the **CPU at command-buffer completion** (0/50 failures,
and unlike waiting for completion it does not stall the pipeline). It is a design change rather
than a patch — it moves the report write out of GPU command order, so it has to be reconciled with
an in-stream `CmdResetQueryPool` / `CmdCopyQueryPoolResults` — so it is deliberately not bundled
with this build. Shipping a second workaround validated against the wrong shape is the mistake
this section exists to avoid repeating.

When that lands, this section goes away and timestamps become trustworthy without filtering. If
the ~18% loss rate makes the data unusable for you in the meantime, say so and we will report
`timestampValidBits = 0` on affected devices instead — the conforming answer, which you already
handle silently, and honest in a way that returning zeros is not.

### Also in this build: outlier attribution on the host side

The wake-chain probes from §10 found two windows in the first 20 minutes of dogfood where a
single virtio-gpu control-queue drain took **25.0 ms** and **18.7 ms**, against a 0.012 ms median
in those same windows — long enough to drop frames, on our side of the boundary. The 5 s
aggregates could not say what stalled, so this build times every command and prints one line
whenever a drain exceeds 5 ms:

```
[GPUWAKE OUTLIER] realtime=<sec>.<nsec> idle=<bucket> total N ms | kick->wake … wake->drained … drained->signal … | N cmds, worst SUBMIT_3D N ms
```

The stamp is **CLOCK_REALTIME**, and your guest clock is anchored to the host's, so these line up
directly against your frame log. **If you catch a dropped frame, note its wall-clock time** — if
an OUTLIER line sits at the same instant, the hitch is ours and now has a named command attached
to it.
