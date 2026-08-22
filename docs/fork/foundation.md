# The foundation: allocation, DRM and the renderer

The single description of how synoik gets pixels onto glass on this stack — what is true today,
what will bite you, and what is left to build. It replaces nine journal documents; their blow-by-blow
history is in git, and nothing here needs it.

Companions, all still live: [`frame-submit-discipline.md`](./frame-submit-discipline.md) (the rule
to read before adding a `vkQueueSubmit`), [`explicit-sync.md`](./explicit-sync.md) (client buffer
producer sync), [`first-frame-costs.md`](./first-frame-costs.md) (the cold-cost class),
[`virtual-display-identity.md`](./virtual-display-identity.md) (what the VMM would have to tell us
about displays), [`lavapipe-submit-timeline.md`](./lavapipe-submit-timeline.md) (one open,
deliberately small handoff).

---

## 1. The stack, and the two facts that shape everything

The compositor runs in a krun VM on an Apple laptop. Guest Mesa `venus` talks to a
`virglrenderer` fork (`vkr`) in the VMM, which talks to a host Vulkan driver (KosmicKrisp) on
Metal. Two consequences run through every section below:

- **Every `vkCreateImage` and `vkAllocateMemory` is a synchronous host round trip.** GPU *execution*
  is not what a frame usually waits for; the guest↔host ring is.
- **The virtual display is LINEAR-only, two planes, ~60 Hz.** Primary (XRGB/ARGB8888) and cursor
  (ARGB8888, 64×64 hard cap). No overlay plane, no `zpos`/`alpha`/`rotation`, no
  `COLOR_ENCODING`/`COLOR_RANGE`, `DRM_CAP_ASYNC_PAGE_FLIP = 0`. **144 Hz cannot be
  acceptance-tested on this rig at all**; the proxy is `gpu p99 < 6.94 ms`.

The renderer is hand-rolled Vulkan and is the *only* renderer — the co-resident GLES renderer was
deleted 2026-07-17, and smithay builds without `renderer_gl`/`backend_egl`/`renderer_multi`
(`renderer_pixman` stays as the render tests' CPU oracle, not a fallback). **Single-device,
LINEAR-only, single-plane is a configuration of this renderer, not its architecture.**

Non-LINEAR scanout was measured on the host side and the answer is a useful *no*: on a tile-based
deferred GPU the scene costs the same into linear, tiled, shared and IOSurface-backed targets to
within 0.2%, and a fullscreen blit is ~35% *cheaper* into linear. **Tiled scanout would buy
nothing. Do not build modifier plumbing for it.**

---

## 2. Allocation and scan-out

### We allocate what we render into

`src/backend/vulkan_scanout.rs` allocates KMS scanout buffers on the renderer's own `Gpu`
(`Texture::allocate_scanout`). There is no gbm on the scanout path and no fallback.

The break this closed is the generalisable part: the tty backend used to allocate with gbm and
import the exported dmabuf into venus. That only ever worked by side effect — gbm's dri backend
follows `MESA_LOADER_DRIVER_OVERRIDE`, and while the session set `=zink`, gbm's buffers *were*
zink→venus blobs, so vkr recognised them as its own. When the default flipped to `=virtio_gpu`, gbm
handed out classic virgl resources, vkr refused them, and every frame died with
`vkGetMemoryFdPropertiesKHR: ERROR_INVALID_EXTERNAL_HANDLE` while the host window still showed
Plymouth. **A compositor that renders in Vulkan should allocate what it renders into**; routing
that allocation through a second driver stack makes the two agree only by coincidence, and the
coincidence is somebody else's default to change.

Per scanout buffer:

1. `vkCreateImage` with `tiling = DRM_FORMAT_MODIFIER_EXT`, a
   `VkImageDrmFormatModifierListCreateInfoEXT` carrying the candidates the *plane* offered, and
   `VkExternalMemoryImageCreateInfo { handleTypes = DMA_BUF }`. Candidates the device does not
   enumerate, or that lack the format features the bind path needs, are filtered out first — the
   list create-info gives no way to learn afterwards *why* creation failed.
2. Dedicated + exportable allocation. **No `vkGetMemoryFdPropertiesKHR`**: that query answers "which
   heaps can hold this *foreign* handle", and there is no foreign handle — we are creating it.
3. `vkGetImageDrmFormatModifierPropertiesEXT` for the modifier the driver actually picked. That is
   what the exported dmabuf names, and therefore what both KMS and the renderer are told.
4. `vkGetImageSubresourceLayout(VK_IMAGE_ASPECT_MEMORY_PLANE_0_BIT_EXT)` for offset/rowPitch.
   **Never `width * 4`** — a driver may pad.
5. `vkGetMemoryFdKHR(DMA_BUF)`. On virtio-gpu this is a prime export of the venus blob GEM — the
   same handle a venus WSI client hands a compositor.

The framebuffer comes from our own `PrimeFramebufferExporter` (`drmPrimeFDToHandle` + `AddFB2` with
`DRM_MODE_FB_MODIFIERS`). Smithay's `GbmFramebufferExporter` imports the buffer *back into gbm*,
which is the same driver mismatch in reverse — swapping only the allocator would have moved the
failure, not removed it.

**Requirements are fail-closed.** `VulkanScanoutAllocator::new` refuses to start the compositor
without `VK_EXT_image_drm_format_modifier`, `VK_EXT_external_memory_dma_buf` or
`VK_KHR_external_memory_fd`, and each allocation fails if no offered modifier survives the feature
check. Deliberate: there is no fallback to fall back *to*, and a silent one would reintroduce
exactly the class of bug this replaces.

**What still uses gbm:** the cursor plane only. `DrmCompositor` takes a `GbmDevice` purely for
CPU-written `CURSOR | WRITE` buffers framed with `framebuffer_from_bo`; Vulkan never sees them.
(The cursor is software today anyway — §6.)

### The implicit-modifier plane

A **stock** virtio-gpu (no `DRM_CAP_ADDFB2_MODIFIERS`, no `IN_FORMATS` blob) advertises its plane
formats with the implicit/`INVALID` modifier. A plain intersection against a LINEAR-only renderer
set is then empty and `DrmCompositor::new` fails with *"No supported plane buffer format found"* —
the fourcc matched, only the modifier did not. That is a compositor that never starts, which reads
as a **boot hang** (gdm never takes the display, `plymouth-quit-wait` never completes). The guest
kernel 7.1.8 bump reintroduced exactly this: the LINEAR `IN_FORMATS` advertisement was a limina
kernel patch, not upstream.

Four places cooperate to accept it, and none of them guesses a layout:

- `scanout_render_formats` (`backend/tty.rs`) adds an `INVALID` twin per LINEAR entry **for the
  compositor negotiation only** — `owned_vulkan_dmabuf_formats`, which is what clients see, stays
  explicit.
- `VulkanScanoutAllocator::create_buffer` asks Vulkan for LINEAR when offered `INVALID` (`INVALID`
  has no encoding in `VkImageDrmFormatModifierListCreateInfoEXT`) and still reports back whatever
  the driver picked.
- `framebuffer_from_dmabuf` drops the `DRM_MODE_FB_MODIFIERS` flag when the device has no
  `DRM_CAP_ADDFB2_MODIFIERS` — still `AddFB2`, just without naming a modifier to a device that
  cannot hear one. A non-LINEAR buffer is refused there rather than handed over unnamed.
- **Pass-through scan-out needs a smithay fork patch.** `DrmCompositor` gates every promotion on
  `plane.formats.contains(&format)`, which on an implicit plane compares a buffer naming LINEAR
  against a plane naming nothing and refuses everything — for the primary plane too, so it is not
  steerable from the caller. `try_assign_primary_plane` reads `self.surface.plane_info()`, a
  `DrmSurface` field built at surface creation, so the `planes:` argument to `DrmCompositor::new`
  cannot reach it. The fork patch matches on fourcc alone **when every plane entry is `INVALID`**;
  a plane that names some modifiers is still matched exactly.

This is safe **because of what this plane is**: virtio-gpu has no tiling, so its buffers are linear
by construction, and our scanout buffers are created by the host through venus, so the host already
knows their exact layout. On real hardware `INVALID` means "unknown, ask the allocator" and refusing
it stays correct — which is why `PrimeFramebufferExporter` refuses an INVALID dmabuf outright rather
than falling back to a modifier-less `AddFB2`. Weston's reasoning (an unknown layout displays
garbage) is why.

### Direct render into the scanout buffer

`matches_render_order` (`Argb8888`/`Xrgb8888`, the common KMS primary-plane byte orders) gives the
imported dmabuf a **direct** render path: it *is* the render-pass attachment, with no shadow and no
present blit. This deleted **85% of a heavy frame's GPU time** (p50 9.34 → 1.47 ms sync,
9.04 → 1.32 ms async, aim-1 miss rate 11.9% → 0.00%). Rendering into a LINEAR dmabuf instead of an
`OPTIMAL` shadow costs ~0.35 ms; the other ~7.7 ms really was the blit.

`present_blit_shadows` survives for the other byte orders and is an LRU cache, not a per-frame
allocation.

> **The trap this created, and it is the shape to watch for.** `VulkanFrame::begin` still decided
> whether to preserve the target with `fb.present.is_some()` — a test for the *shadow* arm. The
> direct arm therefore took a `DONT_CARE` base pass and discarded the whole scanout buffer every
> frame, while the tty backend redrew only `DrmCompositor`'s buffer-age damage. Everything outside
> the damage was, by the spec, undefined; the desktop accumulated trails at every size a window had
> been. The fix is one condition (`!fb.offscreen` — any scanout target that already holds a valid
> frame), because a cycled dmabuf holds exactly its own age-N-ago presentation, the frame damage was
> computed against. **The pixels cannot test this**: `DONT_CARE` leaves contents *undefined* and
> venus happens to keep them for a LINEAR image, so a pixel test passes on the broken code here and
> fails on some other driver some other day. The guard asserts the **pass choice**
> (`VulkanFrame::preserves_target`).

### Two host-side faults, both now closed

**1. Client buffer import — closed host-side.** A client's dmabufs are allocated by the client, so
with GL on vrend they used to arrive as classic virgl resources that `vkGetMemoryFdPropertiesKHR`
refused; `Tty::import_dmabuf` correctly declined them, and Firefox and Epiphany then *hung* rather
than falling back, so in practice it was a dead window. Mutter never hit this because its renderer
is GL, the same driver that allocated the buffer — **a Vulkan compositor is cross-driver by
construction**, and that is the durable part.

**vkr now imports classic virgl resources into venus**, and the guest agrees: both dogfood seats run
GL-on-vrend with **zero** `error importing dmabuf into the Vulkan renderer` lines.
`/etc/environment.d/90-limina-zink.conf` does not select zink either — despite its name it sets
`MESA_LOADER_DRIVER_OVERRIDE=virtio_gpu` + `GALLIUM_DRIVER=virgl`. It protects nothing of ours; a
citation calling it load-bearing is stale.

**2. Scanout surfaces evicted from a bounded host store — fixed host-side.** The limina supervisor
holds Mach-published scanout IOSurfaces in a bounded store that evicted **FIFO by insertion**. A
compositor publishes its permanent scanout ring first and then churns client buffers forever, so the
oldest-inserted surfaces are exactly the ones in continuous use — **ordered by age when the only
thing that matters is use**. The scanouts are deliberately non-global (capability-scoped over a Mach
port), so an evicted one could not be recovered by lookup and nothing re-published it: every later
frame naming that id was skipped silently and permanently, while the worker kept consuming, applying
and correctly ACKing every `SET_SCANOUT_BLOB`. The trigger was **publication volume**, not time —
our own "time under client pass-through scan-out correlates" was volume in disguise.

**The guarantee we may now rely on: presenting a surface id keeps it resolvable, and a dropped id
self-heals.** An id the guest is presenting is never evicted, and one dropped anyway is re-published
on demand — one skipped frame instead of a freeze (833 permanently-skipped frames before, 0 after;
with the pin disabled to force the fault, 3 evictions became 3 single-frame hitches). **So do not
re-create swapchain buffers on a transition as a workaround**, which is what `698ae578` reverted and
why: it worked by republishing, and it hid a host fault behind ~3.2 GB of 4K buffer churn per
session on the one seat whose job is to surface such faults.

**Our publication footprint is two surfaces, and the host's cap is not a reason to shrink it.**
`VkExportMemoryAllocateInfo` appears at exactly one site in `synoik-vk` (`Texture::allocate_scanout`),
so the scanout ring is the only thing we export — up to `SLOT_CAP = 4` buffers, two bound in
practice. Offscreens, blur bundles and widget bakes are device-local and never become IOSurfaces.
The host publishes a surface on **create, not on binding**, so the second device memory we allocate
per scanout buffer — importing the dmabuf we just exported (`import_dmabuf_target`, the redundancy
under "Left on the table") — costs the host nothing. Remove it for the guest-side saving of an
image, a memory import and a `vkGetMemoryFdPropertiesKHR` per swapchain buffer, not for their cap.
That cap is a memory bound on client transients, which churn and do release.

> **The rule this bug earned: require an observation, not a fit.** Six mechanisms explained every
> symptom and were wrong — client-vs-compositor, resource longevity, "stops applying entirely",
> store capacity, a host-side stale-id cache, and a guest-side release. The one that survived is the
> one an instrument printed, after a one-line log was added to a path that had none.
>
> Two corollaries outlive it. **A ruling-out is only as good as the population it counted**: "eviction
> is ruled out" came from counting 20 ids in a log covering only scanout imports, while the store
> received every published surface — 41 of them. And **an unlogged path is where the wrong answer
> lives**: every theory that survived hours did so because the step that would have falsified it
> printed nothing.

We were exonerated by measurement rather than argument, and the instruments that did it stayed:
`debug-dump-scanout` reads the presented framebuffer back out through venus, and the per-frame
presentation tally reports how each frame's elements reached the screen.

---

## 3. The frame path

### One submit per frame

The rule and its two slots live in [`frame-submit-discipline.md`](./frame-submit-discipline.md).
The state it describes: a frame is **one submit** — uploads, dmabuf acquires, glyph copies, layout
barriers and both blur paths all ride it, sharing one grow-only staging pool. On the frame path the
only thing that still waits is CPU readback.

What to watch now that the round trips are gone: the `created` clause of the frame log, since every
`vkCreateImage`/`vkAllocateMemory` is still a synchronous host round trip and that is where the
remaining per-frame host cost lives.

### Async scanout — opt-in, and what it costs to read

`SYNOIK_VK_ASYNC_SCANOUT=1` lets the KMS frame hand its fence to the atomic commit as `IN_FENCE_FD`
instead of parking the compositor thread on it. **Neither dogfood seat runs it**, so every live
number from either seat is the synchronous arm. Read the arm off `/proc/<pid>/environ`, never off a
drop-in: both seats linger, which makes `environment.d` a dead drop that reads as "on". The tty backend brackets `render_frame` with
`set_finish_may_defer`, cleared immediately after so the permission cannot leak to screencopy, a
screencast or a widget bake — each of those hands its buffer straight to a consumer and must still
be finished on return. Offscreen finishes defer on a weaker condition
(`should_defer_offscreen_finish`) because an offscreen never renders inside the tty bracket, and not
deferring made them the largest block of blocked time in the frame log.

Two settled readings:

- **At 60 Hz async buys nothing on the miss rate.** Async and sync converged at 0.00% / 0.01% after
  the direct-render flip. That six-section thread is closed; no further A/B of the pair is worth a
  login.
- **Async smears per-frame GPU attribution.** Holding draws *and* coverage fixed, the means agree to
  3% (1.118 vs 1.153 ms) while async's spread is 3.7× against sync's 1.2×, with lag-1
  autocorrelation +0.63 and slow stretches up to 21 frames. It is redistribution, not cost: the
  fence goes to KMS instead of being waited on, so a timestamp bracket no longer contains exactly
  one frame's work. **Do not quote async per-frame GPU numbers at fine resolution.** This is also
  very probably the "measured better, felt worse" pacing observation (async fps 49.1 ± 9.4 min 14
  against sync 47.3 ± 3.5 min 41), and it weakens the 144 Hz case for async until pacing lands.

### Late frames come in three populations

A session's `missed N vblank(s)` lines are not one phenomenon. Sort a report into these buckets
*before* theorising — two of the three are not renderer bugs, and one is not even ours. The miss
line carries the discriminator: `N cycles since the last flip` says what the screen was doing,
`queued X ms LATE` says how badly we were beaten.

| population | signature | cause | owner |
|---|---|---|---|
| **1. cold bake** | ~3600 cycles, 3–4 vblanks, `queued 30–50 ms LATE` | a cold full-surface bake | ours, fixable |
| **2. sporadic submit** | 3–10 cycles, 1 vblank, `queued 0.3–4 ms LATE` | our submit round trip | ours, cost not scheduling |
| **3. descheduled** | wide, correlated with `main loop busy` | host memory pressure | environmental |

**Population 1 — a cold bake is not a slow bake.** The minute clock tick re-baked the whole panel
bar: 236 bakes, 12 426 ms total, min 0.9 ms but p50 60.7 ms, because a once-a-minute cadence
guarantees the path is cold at every single use. Fixed by splitting each label onto its own
content-keyed texture. The general form is the [cold-cost class](./first-frame-costs.md) — invisible
to every steady-state instrument, because by construction it never happens twice in a row. Guard it
by **counting bakes**, not comparing pixels: a needless re-bake renders identically to a cached one.

**Population 2 — dispatching immediately is correct, and matches GNOME.** 93% of misses are exactly
one vblank after a 3–10 cycle gap, and 94% of the preceding frame's cost is `submit`. It is tempting
to reach for mutter's `max_render_time`; mutter's own source says not to. `should_update_now`
short-circuits deadline dispatch when *"there was an idle period since the last presentation […] it's
best to start working on the next update ASAP, this results in lowest average latency for sporadic
user input."* A 3–10 cycle gap **is** that idle period.

Deadline dispatch exists (`SYNOIK_DEADLINE_DISPATCH=1`, `debug-set-render-time-margin <ms>`) and
**ships off**. A counterbalanced margin sweep found a dose-response reaching parity: 1 ms runs 8× the
baseline drop rate (reproduced three times, on two hosts of different quietness), 2–4 ms sit ~3×,
6–8 ms are indistinguishable from not holding the frame at all. The reason is not design: `tools/timer-probe`
measured `clock_nanosleep` at mean 1.378 ms / worst 8.007 ms and calloop at 0.920 / 8.152 —
**the raw kernel sleep is worse than calloop**, so neither a better timer source nor a bespoke event
loop can move it. **This VM cannot wake a thread on time**; the margin buys VM scheduling jitter.
Mutter's 1 ms is probably right on hardware that wakes in tens of microseconds. Re-run the sweep —
and `timer-probe` *first* — before treating this as a verdict on the technique.

The real bug in this population was the frame clock lying: `next_presentation_time` returned the next
vblank unconditionally, so every missed frame sampled its animations ~16.67 ms stale. `FrameClock`
now tracks recent frame cost (mutter's two-tier maximum — a short-term max that rises at once, a
long-term max decaying by halves once a second) and advances the target past vblanks that cost
cannot reach, **bounded at two cycles**: aiming further out on one catastrophic frame would jump
every animation ahead of what renders, which reads worse than the miss it avoids. Still open:
*honest accounting* — a best-effort target that slips is not the same event as a promised one, and
conflating them inflates the miss count. Note this cuts both ways, so miss counts before and after
the advance are not directly comparable.

**Population 3 — the process is not running.** 85% of `main loop busy` warnings are
not-running-majority; 93% of aggregate stall time is wall-clock with no CPU behind it. The
corroboration was 153 MB swapped against 129 MB resident with 12 365 major faults, alongside firefox
holding ~1.5 GB of swap on an 8 GB VM. (`steal 0` in `/proc/stat` is not evidence against host
contention — this hypervisor does not report steal.)

The answer is a systemd drop-in, not a code change: `MemoryLow=512M` on the compositor unit, which
applies live via `daemon-reload` because it is a cgroup property. It is reclaim *protection*, not a
guarantee, which is the right shape — the compositor should be the last thing paged out, not a thing
that cannot be paged out. Under it, major faults went from 12 365 to 1 710 over 15h55m.

Two stronger dials are deliberately unused: `MemorySwapMax=0` converts pressure into an OOM kill of
a unit that already carries `oom_score_adj=100`, and `mlockall(MCL_FUTURE)` makes *every future
allocation* unswappable, turning any leak into unkillable system-wide pressure.

**And there is no `madvise` for residency** — `madvise` tunes reclaim order and readahead, never
residency; `MADV_WILLNEED` can pull pages back but nothing stops them leaving again. The genuinely
useful inverse moves (`MADV_COLD`/`MADV_PAGEOUT` on caches we know are cold, `MADV_FREE` on caches
we can rebuild) share one blocker worth stating plainly: `madvise` works on page ranges and a
general-purpose allocator interleaves hot and cold structures inside the same page. **"Protect the
important data structures" is not expressible until they live in their own arena.** That is a
project, not a flag.

If page locking is ever wanted, the measurement is already done and the finding is that almost none
of the *buffers* qualify: device-local Vulkan memory is not guest-pageable; Venus `HOST_VISIBLE`
staging is a *host* blob, so guest residency does not reach its warm-vs-cold cost; client shm is
client-owned tmpfs and pinning it is an authorization we cannot give. What is left is executable
text — `mlock2(MLOCK_ONFAULT)` over synoik's `r-xp`/`r--p` mappings (24.6 MB mapped, 13.4 MB
resident) plus the venus ICD and libc, **excluding libLLVM** (141 MB, the shader compiler: hot at
pipeline creation, cold forever after — that one exclusion is the difference between a ~25 MB lock
and a ~150 MB one), under a finite `LimitMEMLOCK` of 64–128M, never `infinity`. Trap for the
drop-in: `LimitMEMLOCK` is an rlimit and needs a **restart**, unlike `MemoryLow`.

---

## 4. Instruments, and the traps in them

| instrument | what it is for |
|---|---|
| `SYNOIK_VK_VALIDATION=1` | **the only spec check.** A plain env var read in `Gpu::with_selector`, so the live session honors it via a unit drop-in |
| `SYNOIK_FRAME_LOG=ring,gpu,autodump` | the flight recorder to leave on a session you actually use |
| `synoik msg frame-perf` | reads the running session's tallies (the rolling summary resets, so the journal cannot answer "has this been happening?") |
| `SIGUSR1` | non-terminating ring dump; the only way to get the ring out of a live session |
| `SYNOIK_SCENE_BREAKDOWN=verbose` | per-element damage and opacity — the cheapest first look at a redraw question |
| `SYNOIK_VK_FULL_DAMAGE=1` | turns the whole partial-damage chain off: separates "we drew the wrong thing" from "what we drew did not survive" |
| `msg action debug-toggle-damage [--age N]` | the same ask, tinted on the glass, one frame late — the visual companion to the line below |
| `SYNOIK_DEBUG_DAMAGE=1` | one line per frame per output: what the screen was told to repaint, for a one-frame-old buffer and a two-frame-old one, and whether the frame went to a swapchain buffer or straight to a plane |
| `SYNOIK_DEBUG_INSTANCES=1` | names the element whenever one drops an instance while a sibling stays put — the damage tracker's only under-report |
| `debug-dump-scanout` | reads back the framebuffer we actually present (and the client's buffer when it owns the plane) |
| `tools/timer-probe`, `tools/blur-probe` | isolate the VM's wakeup floor and blur cost from the compositor |

**A capture cannot tell you what a screen was repainted with.** Every capture path re-renders the
scene — a screenshot, a one-shot screencopy (`grim`), `render_for_screencopy_without_damage` — so a
screen showing stale pixels photographs clean, which proves the scene is right and says nothing
about the buffers. Only two things see the buffers: what is on the glass, and a *continuous* cast,
which draws incrementally through its own tracker and so inherits the same miss. A trace that a
running screencast records and a one-shot capture cannot is therefore a damage question, not a
scene question — that is what `SYNOIK_DEBUG_DAMAGE` is for.

**The damage overlay tints the frame before.** It takes its rects from the composed
`RenderFrameResult` — planes included, at an age you pick — which only exists *after* the frame is
built, so what is on the glass is the previous frame's ask. Two rules keep it from measuring itself,
and both are load-bearing. Its tint elements hold **stable `Id`s from a pool indexed by sorted
rect**, so a region that keeps being damaged at the same geometry tints for free; a fresh `Id` per
rect per frame instead repaints every tinted region for as long as the overlay is on. And its own
contribution — for each pool slot that moved, the rect it left plus the rect it took — is
**subtracted back off** before the rects are shown. That subtraction cannot be done with
`damage_from_age`'s element filter: the tint is composited into the primary plane, so its pixels are
inside that plane's recorded damage and no id can lift them out. Left in, the churn reads as damage,
gets tinted, and never drains. The residual cost is that real damage coinciding with tint churn is
masked for one frame — the right trade for a locator. On a still screen with the overlay on, the
tint converges and `SYNOIK_DEBUG_DAMAGE` drains to empty; if it does not, one of those two rules
broke. Pinned by `render_helpers::debug::tests`.

**A stale rectangle nothing ever asks for is an instance that departed.** The tracker lets one
`Id` appear many times in a frame and we lean on it — one cached texture draws a window in the
workspace and again in every thumbnail showing it. It decides per instance against *any* remembered
one, so an instance that moves damages its new geometry plus every remembered instance and heals
what it left. An instance that simply goes away while a sibling stays unchanged heals nothing: the
survivor takes the cheap branch, and `elements_gone` only fires for an `Id` absent altogether. That
rect is then asked for by nobody, ever, and lives in whichever screen buffer missed the repaint —
surfacing every time that buffer comes round, which reads as a region flickering between two states
at frame rate. Pinned by `tests::damage_instances`; the fix is ~5 lines in the smithay fork (damage
every remembered instance when the count shrinks) and ships only with the fork.

**Turn validation on FIRST when the compositor misbehaves around the renderer** — before profiling,
bisecting or theorising. Undefined behavior surfaces as whatever the driver felt like doing: a wedge
that presented as `ERROR_OUT_OF_HOST_MEMORY` from every allocation was a destroyed `VkImage` inside a
still-recording command buffer, and the allocation failures were the corpse, not the cause. Hours
went into falsifying memory-pressure theories that the layer named in one run.

Traps that have each cost a day:

- **An instrument that omits its event reads as perfect.** Suspect any metric that hits exactly
  zero. Conversely, an instrument that *never fires* can be the answer — `SYNOIK_VK_FULL_PRESENT_BLIT`
  producing zero lines proved the present-blit path never ran for KMS, which is what pointed at the
  direct arm.
- **`AUTODUMP_MAX = 20` is a lifetime cap**, so a long session goes blind exactly when it gets
  interesting. Open: make it a rate, and add a `msg frame-perf dump`.
- **A backgrounded VT renders at ~1 fps** and frame-perf keeps counting regardless. A seat timing run
  is void unless `loginctl` says `Active=yes`. Two full A/Bs were thrown away to this.
- **Take the arm from the sample, never from the script.** A sweep inherited a session left in the
  treatment state while its own tracker assumed the default, inverting all eight arms and reading as
  "the dose-response is noise". `msg frame-perf` reports `Dispatch:` and the live margin for exactly
  this reason.
- **`Late presentations` is not comparable across dispatch arms** — an immediate-dispatch frame
  carries a `reachable()`-advanced target and silently absorbs the slip a held frame reports. Judge
  on the gap histogram.
- **Measuring a cold cost twice in one process gives the warm number**, and the difference can be two
  orders of magnitude (408 ms → 11.8 ms for the same call). Measure the first occurrence in a fresh
  process, and separate I/O-bound cold costs from compute-bound ones.
- **An instrument reads zero because it is off at least as often as because the event is absent.**
  `tracing_on` defaults to `0`, so enabling a tracepoint event alone yields a convincing zero;
  `vulkaninfo` exits silently when it cannot reach a display server, so clear `DISPLAY=` and
  `WAYLAND_DISPLAY=` before believing an empty extension list; `strace` needs the right pid, not the
  session leader. **Positive-control every zero** — drive the event you expect and confirm the
  instrument counts it — before reporting the absence as a finding.
- **A `lost` GPU-timestamp count is the regression signal.** The defect was a rate, not a switch (94%
  failure bare, 18% with the first workaround, 0% since the 2026-07-26 VMM). Nothing averages a zero
  in as if it were a fast pass. **Do not delete `venus-bugs/repro-vk-timestamp-query`** — it is the
  discriminator, it is cheap, and it has reported three different answers on three builds.

---

## 5. Costs that are settled — do not re-derive these

Everything here was measured. Re-deriving any of it costs a day.

- **A submit is cheap; the wait is the bill.** K empty submits then one wait: K=1 0.061 ms, K=16
  0.334 ms — **~18 µs per extra submit**, nowhere near a round trip each. `vn_WaitForFences` is a DRM
  syncobj wait ioctl signalled by a per-queue host CPU thread retiring FIFO: a host thread wake, a
  VMM retire, a guest interrupt, a guest wake. Any "cost of a submit" figure quoted anywhere is
  submit-**plus**-wait.
- **A second VkQueue does not escape the *transport*; the GPU is not the constraint.** One ring per
  `VkInstance` (`vn_instance_init_ring`), `dev->primary_ring = instance->ring.ring` for every submit
  and allocation, one host thread per ring (`vkr_ring_thread`). The per-thread TLS ring is a 16 KiB
  synchronous-command ring, not a submission lane. But the host side answers the other half:
  KosmicKrisp creates **one `MTLCommandQueue` per `VkQueue`** (`kk_queue.c:228`), so two queues do
  get independent Metal queues and Metal schedules them concurrently. **What serialises is command
  encoding on the single ring thread, ~18 µs per submit — not execution.** A second `VkInstance` is
  therefore the shape that buys a lane. Our own strict serialization is **our** design: one queue,
  every submit chained on one timeline semaphore.
- **The ring parks between our frames — and the timeout is ours to move.** The host ring thread
  yields 16 times then sleeps, so blocking waits idle the ring past the timeout and the *next*
  submit pays a wake on top; the waits buy each other. The 1 ms is **not a host constant**: it is
  guest-supplied at ring creation (`vkr_transport.c` takes `info->idleTimeout`) from
  `VN_RING_IDLE_TIMEOUT_NS` in guest mesa's `vn_ring.c` — a tree the VMM side owns and already
  patches. So a mostly-idle second ring being cold exactly when needed is a negotiable trade-off,
  not a constraint to design around.
- **`VN_PERF=no_fence_feedback` cost us ~25–30% of the wall clock on real-work submits** and was
  dropped from the session env on **2026-07-25**. Any figure in git history dated before that is on
  the slow path. Fence feedback is gone from Mesa 26.2 entirely (`venus: deprecate fence feedback`),
  so a mesa bump silently puts us back on synchronous polling with no flag to turn it back on.
- **GPU timestamp queries work, 100%**, since the 2026-07-26 VMM (600/600 across both entry points
  and three resolve paths; 200/200 through our own `GpuTimer`). Every "we are blind to GPU time"
  caveat in the history has expired.
- **Host-visible memory is roughly half the bandwidth of ordinary guest memory**, and the expensive
  part is first touch, not write-combining. This is why `StagingPool` is one grow-only, persistently
  mapped buffer rewound per frame rather than a buffer per upload — and why per-upload staging is
  not merely slow but *fatal*: a `HOST_VISIBLE` buffer is a virtio-gpu blob, and the host ran out of
  blobs two minutes into a live session, after which every `vkAllocateMemory` failed.
- **Image creation is bimodal, and it is the host's.** A single `vkCreateImage` measures p50 0.10 ms
  and max 23.38 ms; the spikes are not first-execution costs (one hit an idle desktop twelve minutes
  into a session) and they are not ours. Venus's requirements cache is real — a *miss* on the
  dmabuf/modifier shape costs 0.06–0.7 ms — which is why a frame that allocates can spend
  milliseconds doing nothing else. **Never allocate per frame.**
- **Blur costs 0.16 ms + 0.092 ms per megapixel**, linear across a 21× range. A frame log once
  charged a blur 21 ms; that reading was a fence wait, which on an in-order queue carries everything
  queued ahead of it. **Before attributing a cost to a site because its wait is long, price the work
  directly.**
- **The first-frame wait does not drain the previous frame.** A frame's `first` submit measured
  1.4–2.4 ms in six of seven frames. The chain is not costing us a frame's tail; dependency-accurate
  ordering and frames-in-flight are not urgent for that reason. Good hypothesis, measured, wrong.
- **JPEG-XL wallpaper decode is not GPU-accelerable in any practical way.** No hardware JXL decode
  block exists anywhere, libjxl is CPU-SIMD only, and the entropy stage (ANS/prefix coding) is
  inherently sequential. Fix it algorithmically — decode at/near target resolution (the 1:8 DC image
  gives an instant preview) plus a variant cache. Make it decode *less*.

---

## 6. Open, in roadmap order

1. **Multi-planar / non-LINEAR client dmabuf import.** The importer accepts only single-plane,
   LINEAR, 8888 buffers, so NV12/P010 — zero-copy hardware video decode — is unreachable and a player
   must fall back to CPU conversion. **This affects every machine, including this VM**; GLES was
   masking it. Split the halves: **multi-planar sampling** (per-plane `VkImage` +
   `VK_KHR_sampler_ycbcr_conversion`, or manual plane sampling in the shader) is ours and is needed
   regardless of hardware planes or host modifiers — start it. The non-LINEAR half is gated on the
   host and, per §1, is probably not worth having. Second-order: video clients will be **dmabuf**,
   which puts minified-LINEAR-texture sampling back on the frame-cost suspect list.
2. **DRM-node-aware device selection.** Dmabuf feedback advertises `primary_render_node.dev_id()`
   while the renderer runs on whatever `Gpu::new()`'s enumeration ranked highest. These coincide only
   because this VM has one GPU; on any multi-GPU machine we would be telling clients to allocate for
   a device we are not rendering on. `Gpu::for_drm_node(node)` via `VK_EXT_physical_device_drm`, ~a
   day, and it is step 1 of item 7 anyway.
3. ~~**The idle redraw loop.**~~ **CLOSED — did not reproduce on either seat.**
   On 2026-08-15 a live seat measured a flat 360 `DRM_IOCTL_MODE_ATOMIC` commits in 6 seconds — 60/s
   at 21% CPU — with the output reported permanently in
   `RedrawState::WaitingForVBlank { redraw_needed: true }`.

   Re-measured the next day at `bd7a845c`, on both seats, all three configurations, every one
   reading `redraw state: Idle` and `animating: false`:

   | seat | state | window | commits |
   |---|---|---|---|
   | gsrs, fresh session | no windows | 8 s | **0** |
   | gsrs, fresh session | one idle `gnome-terminal` | 8 s | **0** |
   | kov, ~4 h uptime, real workload | many windows | 12 s | **0** |

   A `virtio_gpu_cmd_queue` tracepoint agreed independently: **0 commands in 12 s** idle. Every
   instrument was positive-controlled in the same session — 10 injected pointer motions gave exactly
   10 atomic commits, 20 gave exactly 20 `0x301` + 20 `0x104` — so these are measured zeros, not
   blind ones.

   No commit explains the change, so treat the original reading as unexplained rather than fixed.
   If it returns, instrument the redraw *requesters* rather than the commits — and note why it
   matters beyond power: with the rate pinned at 60/s **animation cost is unmeasurable**, because a
   workspace switch adds no visible commits and the frame log cannot tell an animating frame from an
   idle one.
4. **GPU-side capture readbacks, with colour conversion folded in.** `render_to_shm` and the
   PipeWire cursor bitmap read `Abgr8888` back and CPU-swizzle to BGRA because offscreens/readbacks
   are RGBA-order-only here; extend the `Bind<Dmabuf>` B8G8R8A8 + `vkCmdBlitImage` trick so both
   swizzles disappear. Do RGBA→NV12/I420 in a compute shader during the same readback while you are
   there: it removes a per-frame CPU cost, **shrinks the readback ~2.6×** (4 B/px → 1.5 B/px), and
   lands frames in exactly the layout item 6 wants. The compute queue exists today; this is not
   gated on Vulkan Video.
5. **Frame pacing on top of async scanout** — fence off the frame loop, but never run more than one
   frame ahead. This is the concrete next step for the async smear in §3, and it is a design item,
   not a measurement.
6. **Hardware video encode.** The recorder encodes on the CPU (libvpx VP8; VAAPI is unreachable
   through Venus). The Venus device exposes **no** `VK_KHR_video_encode_*`/`video_decode_*` and a
   single combined `GRAPHICS|COMPUTE|TRANSFER` queue. Vulkan Video is being added to the VMM; when it
   lands, a Venus-backed `EncoderBackend` slots behind the recorder's existing trait with no
   compositor-side change.
7. **Multi-GPU** — only when there is bare-metal multi-GPU hardware to validate on. Deferring is
   cheap: the Vulkan branch never reached smithay's GLES `GpuManager`, so **a Vulkan session never
   had multi-GPU support**, and none of that machinery transplants. Single-device is baked in only
   shallowly — no global state, each `Gpu` owns its instance and device, each `VulkanRenderer`
   carries its own `ContextId`, and every cache is a per-instance field. The work is item 2, plus
   per-CRTC present-blit shadows, plus one new engine (a fallback copy stage when the scanout GPU
   cannot import the render GPU's buffer). Note two of our apparent weaknesses — LINEAR-only and a
   synchronous `finish()` — are the two hardest multi-GPU sub-problems pre-collapsed to their trivial
   cases. **The real cost is validation, not code**, and no production Wayland compositor does
   render-on-A/scan-out-on-B with a Vulkan renderer today.

Slottable any time:

- **The hardware cursor plane.** We composite in software because smithay never sets
  `DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT`, which otherwise produces a double cursor. Blocked on the
  smithay fork patch; a real HW offload we are not using, and it removes a full repaint per pointer
  motion.
- **Skip the import on our own scanout buffers.** `DrmCompositor` hands the renderer a `Dmabuf` and
  `import_dmabuf_target` builds a *second* `VkImage` around it — correct but redundant, since the
  allocator already holds the `VkImage` that memory belongs to. Registering it against the exported
  dmabuf would drop one image, one memory import and one query per swapchain buffer.
- **A second venus context for off-composite-path rendering** (see §5). Order: get a VMM status
  update on external-sync primitives → try a second context → renegotiate the cold-ring backoff
  without buying it with idle wakeups. We already have the whole cross-context toolkit and it is
  proven in production — importing a client's dmabuf *is* sampling another context's render, and the
  worker would be a client we own. Images must be dmabuf-backed with modifiers to cross the boundary.
- **An arena for reconstructible caches**, without which the `madvise` options in §3 cannot be
  expressed at all.
- **Read `heapBudget` from `VK_EXT_memory_budget`.** The extension is available under
  `VN_DEBUG=mem_budget`, which the enhanced-tier environment sets — check the gate before concluding
  the driver lacks it. `heapBudget` carries the VMM's
  per-context GPU-memory cap, and it is the **only** backpressure channel the venus transport does
  not discard: over the cap the host kills our context rather than returning an error, because an
  async `vkAllocateMemory` has already answered `VK_SUCCESS`. Today that arrives as an unexplained
  death. `synoik_vk::devmem` complements it rather than replacing it — the census attributes *our*
  allocations by site, `heapBudget` reports the *host's* total. Neither answers the other's question.
  **Do not poll it per frame:** chaining `VkPhysicalDeviceMemoryBudgetPropertiesEXT` makes
  `vn_GetPhysicalDeviceMemoryProperties2` a real synchronous `vn_call_` round trip — which is
  exactly why it survives the transport, and exactly why it is not free. Per-N-frames or on an
  allocation-rate trigger. Freshness is not a concern: the host computes it from its live ledger at
  query time, `heapUsage` being what our context holds and `heapBudget` the cap minus what every
  other context holds.

### Surviving device loss — unblocked, and now schedulable

**The blocker is gone, and it was gone before we knew.** Guest mesa on the enhanced tier carries
`patches/mesa-guest/0005-venus-surface-ring-loss-as-VK_ERROR_DEVICE_LOST-inst`: `vn_ring_wait_seqno`
returns a status instead of looping forever, `vn_relax` returns `bool`, and ring loss surfaces as
`VK_ERROR_DEVICE_LOST` from the entry points where the spec permits it — `vn_get_fence_status`,
`vn_update_sync_result`, `vn_get_semaphore_counter_value`, `vn_query_feedback_wait_ready`,
`vn_get_query_pool_feedback`, plus the ring wait-space and submit paths. Object creation like
`vkCreateImage` is deliberately untouched, since the spec forbids `DEVICE_LOST` there.

Verified on this seat in the shipped binary rather than from the version string: `vn_relax` carries
a `DW_AT_type` resolving to `_Bool`, and `vn_ring_wait_seqno` is likewise non-`void` — upstream both
return `void`, which in DWARF means no type attribute at all. `mesa-vulkan-drivers-26.1.6-1.limina.fc44`
is the floor.

So **recovery is ordinary work now**, not a research problem: detect `VK_ERROR_DEVICE_LOST` on
submit/acquire, rebuild outside the frame dispatch loop, re-realize the scene. If an implementation
still meets a `vn_relax` abort, that is a live bug in the mesa patch and the VMM side wants it.

Expect the hardest part to be the **bake keys**, not the state machine. Mutter's equivalent is the
glyph cache; ours is every baked texture and cached element keyed by `Id`, spread across the widget
layer rather than owned by one renderer object. **A device loss must invalidate every bake key.**
And clients are not recoverable by the compositor — they must re-render their own buffers — so even
a complete implementation means a visibly broken frame or two, not a seamless save.

`MESA_LOG_LEVEL=debug` on the seat stays worth having regardless: venus's warnings log at
`MESA_LOG_DEBUG` and release mesa defaults to `MESA_LOG_INFO`, so without it an abort never names
which path fired.

**Why it read as unimplementable for eleven days**, since the shape recurs: the blocker was recorded
here as a property of venus (the process dies before any `VkResult` reaches us) when it was really a
question about a tree we do not own and had not re-read. **When a blocker names someone else's code,
its status is a question to ask, not a fact to write down.** `VN_DEBUG=no_abort` is still not an
answer, for the record — it removes the abort from a loop with no exit, converting a crash into a
permanent hang.

---

## 7. Standing asks on the VMM side

Booked as limina M15 "Virtual display pipeline v2": per-hardware-display native refresh (120 Hz
ProMotion on the MacBook panels — no host hardware we own does 144), overlay planes (a plane is a
CALayer; we own both ends of the protocol extension), NV12/P010 on those planes with
`COLOR_ENCODING`/`COLOR_RANGE`, and a cursor larger than 64×64.

**Status (2026-08):** wave 4 closed; wave 1 parts 1–2 shipped. Next up are per-host-display,
120 Hz/VRR, and overlay planes — so **overlay planes are not live yet**, and "when it lands" remains
accurate for them, for NV12/P010-on-planes and for `COLOR_ENCODING`/`COLOR_RANGE`.

**Hardware video has no date, and no partial support to discover.** Measured on the host side:
`VK_KHR_video_*` appears **nowhere** in KosmicKrisp or vkr, and `kk_physical_device.c` builds exactly
**one** queue family (`GRAPHICS | COMPUTE | TRANSFER`). It is not on the M15 waves. This is why §6
item 1 splits the work: the multi-planar sampling half is ours and does not wait.

One expectation this fork set and should keep setting: **overlay planes are an adjunct, not the
lever.** They help video and fullscreen surfaces; promoting the wallpaper is the weakest use of one
(occluded most of the time, and blurred in the overview where it is most visible). We want them
because video is part of a good desktop, not because they will make the shell's own drawing faster.

Answered: **vkr imports classic virgl resources into venus** (§2, item 1 — closed).

### To raise: rename `90-limina-zink.conf`

`/etc/environment.d/90-limina-zink.conf` has not selected zink since 2026-08-15 — it sets
`GALLIUM_DRIVER=virgl`, `MESA_LOADER_DRIVER_OVERRIDE=virtio_gpu`, `VK_DRIVER_FILES=…virtio_icd…`
and `VN_DEBUG=mem_budget`. The name survived the value it was named after, and it cost real time on
this side: two of our documents asserted the drop-in was load-bearing *because it selected zink*,
and the name was the only evidence anyone re-checked.

**Proposed: `90-limina-mesa.conf`.** The rule that avoids the recurrence is to **name the knob, not
the value** — every variable in the file is mesa driver selection, which stays true whichever driver
is selected next, whereas `zink` was a setting and settings move.

Two smaller points for the same conversation:

- **`VN_DEBUG=mem_budget` does not belong in a driver-selection file.** It is a tunable, on a
  different change cadence from which ICD the guest loads, and today flipping it means editing the
  file that decides whether GL works at all. Its own drop-in would be safer.
- **Does the file still need the GL half?** Now that vkr imports virgl into venus, our reason for
  caring which GL driver the session picked is gone. Whether anything *else* still needs it is
  theirs to answer, but it is worth asking rather than inheriting.

It is host-managed — not owned by any package, and `limina-agent.service` rewrote it on 2026-08-15 —
so **the rename has to happen on their side.** Renaming it in the guest would leave two priority-90
files with no owner the next time the agent deploys.

Nothing open. §2's two faults are closed host-side.

**Closed: the dmabuf CPU-write coherency issue.** Its draft is retired. Cause was
control-queue work being unordered against venus ring work — virtio-gpu control commands run on
libkrun's gpu worker thread while venus runs on virglrenderer's `vkr_ring` thread, so a guest CPU
write into a venus-shared dmabuf could be read one write behind. Fixed in two halves that **only
work together**: guest mesa flushes and waits on unmap of a `PIPE_BIND_SHARED` write map (restores
*ordering*), and vrend `glFinish`es so the fence means *completion*. A barrier alone was tried first
and reverted after 10/10 still-stale runs. **Both halves are enhanced-tier only — the stock tier
keeps the bug**, so this returns if we ever run against a guest without limina's mesa.

**KosmicKrisp ships with asserts compiled out** (`b_ndebug=true` on both the release and devenv
builds — zero `assert` symbol references and zero assertion-failure strings in
`libvulkan_kosmickrisp.dylib`). So an invalid command from us will not abort the VM.

It will not abort anything, which is the point: **that removed the tripwire, not the hazard.** An
unchecked bad command runs on into undefined behaviour, so vkr's trust-boundary checks are the only
defence rather than defence in depth, and the failure mode changes from a clean crash to **silent
corruption**. That is the direction we are least equipped for — `SYNOIK_VK_VALIDATION` is off by
default, and a corrupted frame passes any pixel comparison whose cache happens to hold the right
image. Treat "the VM stopped dying" as a change in *symptom*, not in *risk*, and keep running
validated runs after renderer work. `virtual-display-identity.md` carries the display-identity
ask, deliberately written so it would help stock mutter too.
