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

- **Keep the guest binary identical.** `target/debug/niri` currently corresponds to `4504c5b5`
  (`2ca2e164` is docs-only and does not change it). If the guest is rebuilt as well, nothing in a
  before/after is attributable.
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
