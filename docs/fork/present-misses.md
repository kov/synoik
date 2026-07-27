# Presentation misses on the virtual display — a VM/VMM handoff

**Written 2026-07-27**, from inside the guest (`gnome-shell-rs` dev VM), from one 14-minute
live session (`niri[380336]`, `v26.04-659-gaf36d86a`).

**Audience:** whoever works on the VM / host graphics and display stack — the VMM's virtio-gpu
device and its `krun-display` connector, our `virglrenderer` fork, the host compositor path.

**One-line summary:** about **12% of the frames this compositor presents land a whole refresh
cycle after the vblank they were built for**, and in **84% of those the compositor had already
handed the flip to KMS before the deadline** — a median of 4.8 ms early, and more than 10 ms
early in 146 cases. We cannot see past the `DRM_EVENT_FLIP_COMPLETE` timestamp from in here,
so we cannot tell whether these are real dropped frames or an artefact of how that timestamp is
produced. **That distinction is the single most useful thing the host side can tell us**, and
§6 says why.

> **§6 has since been answered — read [§9](#9-vmm-side-answers-2026-07-27-host-session) first.**
> The timestamp is the guest's own emulated vblank hrtimer, and the host presents fire-and-forget
> today, so **nothing below is a statement about glass**. A "miss" here means the commit chain
> crossed a tick of a guest-local grid. The numbers stand as written; their *interpretation* is
> §9 and §10, and §10.1 supersedes §4's reading of the idle regime.

Companion to [`venus-cost.md`](./venus-cost.md), which covers per-frame *submit* cost. This one is
only about *presentation timing*. §7 is a second, unrelated finding that also lands on the host
side and is cheap to include here.

---

## 1. Environment

| | |
|---|---|
| Guest kernel | `Linux 7.1.4-limina16k aarch64` |
| Guest Vulkan | `venus`, Mesa 26.1.4 (`mesa-vulkan-drivers-26.1.4-3.limina.fc44.aarch64`) |
| DRM device | `virtio-mmio` / `virtio_gpu`, `card0`, connector `Virtual-1` |
| Connector EDID | `Red Hat, Inc. krun-display 0x00000001` |
| Mode | 3840×2160, clock 583400, htotal 4400, vtotal 2210 → **60.00 Hz, refresh 16.67 ms** |
| VMM / host | limina VM, our `virglrenderer` fork; host GPU Apple M4 Pro |
| Guest workload | `gnome-shell-rs`, a hand-rolled Vulkan compositor (no GL), single output |

The guest compositor is a **debug build** with `opt-level = 3` on its own crates. Its CPU time per
frame is a median of ~0.9 ms in this session, so it is not a factor in anything below.

Relevant compositor configuration (`~gsrs/.config/environment.d/`):

```
NIRI_FRAME_LOG=all,gpu        # the instrument this document is built from
NIRI_VK_ASYNC_SCANOUT=1       # the atomic commit carries an IN_FENCE_FD — see §5
```

## 2. What the guest measures, exactly

Three timestamps per frame, all `CLOCK_MONOTONIC`, all from `src/frame_log.rs`:

| name | what it is |
|---|---|
| **target** | the presentation time the frame was *built for* — the compositor's frame clock predicting the next vblank |
| **queued** | the moment the finished frame was handed to KMS (`drmModeAtomicCommit`) |
| **actual** | the `tv_sec`/`tv_usec` on the resulting page-flip completion event |

From those:

- **headroom** = `target − queued`. Positive means the compositor was out of the way before the
  deadline. Reported as `queued N ms early`; negative as `queued N ms LATE`.
- **missed** = `round((actual − target) / refresh)`. Zero is the normal case — landing a hair
  either side of the target is scheduling jitter on the *same* vblank, not a miss.

A note on what is deliberately *not* measured: the gap in the DRM vblank *sequence* counter. That
counter advances every cycle whether or not anything flipped, and a damage-driven compositor does
not flip when nothing changed, so on an idle desktop the sequence gap measures idleness. The first
version of this logger reported "dropped 59 frames" once a second on a static screen. Everything
below is against the frame's own declared target.

## 3. The headline numbers

One session, 10:28:13 → 10:42:06, a mix of idle desktop and two bursts of real interaction
(overview, app grid, window management, a wallpaper change):

```
frames presented          ~4072
missed vblanks             480   (12%)
frames over CPU+GPU budget   8   (0.2%)
```

Of **468 miss events**:

| | count | headroom at commit |
|---|---|---|
| queued **early** — we were out of the way before the deadline | **394 (84%)** | median **+4.83 ms** |
| queued **late** — genuinely ours | 74 (16%) | median −0.72 ms |

392 of the 394 early-queued misses were **exactly one cycle** late (two were two cycles). The
headroom distribution on them:

```
min 0.01   p25 2.23   p50 4.83   p75 12.02   p95 14.33   max 16.41   (ms)

195 of 394 had more than  5 ms of headroom
146 of 394 had more than 10 ms of headroom
```

A headroom of 16.41 ms at a 16.67 ms refresh means the commit went in essentially at the vblank
*preceding* the target — a full cycle of slack — and the flip still completed a cycle late.

## 4. Two regimes, both missing

Broken down by how hard the desktop was being driven (each row is one 10 s summary window):

| fps | frames | missed | miss % | gpu avg | headroom p50 | headroom p5 | slack ≈ p50 |
|---|---|---|---|---|---|---|---|
| 48.1 | 481 | 14 | 3% | 4.81 ms | 14.18 ms | 11.94 ms | 9.37 ms |
| 46.7 | 467 | 24 | 5% | 2.49 ms | 14.85 ms | 11.87 ms | 12.36 ms |
| 40.2 | 403 | 22 | 5% | 5.23 ms | 14.53 ms | 11.73 ms | 9.30 ms |
| 38.4 | 385 | 7 | 2% | 3.36 ms | 14.68 ms | 12.06 ms | 11.32 ms |
| 31.4 | 363 | 48 | 13% | 5.72 ms | 13.97 ms | 10.99 ms | 8.25 ms |
| 30.8 | 310 | 21 | 7% | 2.63 ms | 14.71 ms | 3.30 ms | 12.08 ms |
| 24.7 | 247 | 58 | 23% | 6.95 ms | 13.77 ms | 10.62 ms | 6.82 ms |
| 15.3 | 153 | 9 | 6% | 3.27 ms | 14.08 ms | 4.08 ms | 10.81 ms |
| 15.3 | 153 | 18 | 12% | 4.95 ms | 12.80 ms | 4.22 ms | 7.85 ms |
| 12.8 | 128 | 12 | 9% | 3.97 ms | 14.85 ms | 7.82 ms | 10.88 ms |

("slack" is `headroom p50 − gpu avg`, an estimate of the real deadline margin once §5's in-fence
is accounted for. It mixes a percentile with a mean, so treat it as an order of magnitude, not a
figure. It is 7–12 ms in every row.)

**Idle** is worse, not better: 47 windows at 1–2 fps, ~529 flips, **160 missed (30%)**, with GPU
work averaging 0.6–1.5 ms per frame. At that rate the compositor emits one isolated flip roughly
every second — nothing is queued behind it, nothing is contending — and it misses its target
vblank about a third of the time.

Idle also has *less* headroom (per-window p50 ranged 0.08–13.15 ms, and p5 went negative in most
windows) which is worth stating plainly because it points somewhere different: at 1 fps the
compositor's frame clock has no recent presentation feedback and has to extrapolate the next
vblank as `last presented + k × refresh`. Headroom going negative there means the extrapolation
is landing on the wrong side of the real vblank — i.e. **the emulated vblank phase is drifting
against the guest's `CLOCK_MONOTONIC`**, or the intervals are not actually 16.67 ms. That is a
separate hypothesis from §3's, testable on the host, and it would explain the idle regime without
explaining the sustained-fps rows above it.

Misses are mostly **isolated**, not bursty: of 344 runs of consecutive misses (grouping anything
within 200 ms), 296 are a run of one and the longest is 11. This does not look like a periodic
stall swallowing a batch of frames; it looks like a per-flip coin toss.

## 5. The confound we cannot exclude from in here

`NIRI_VK_ASYNC_SCANOUT=1` means the compositor does **not** block on its render fence. It exports
the submit's completion as a `sync_file` and hands it to KMS as `IN_FENCE_FD` on the atomic commit
(`src/render_helpers/vulkan/fence.rs`, `Fence::export` → smithay memoizes it in the plane config).

So the flip cannot latch until our GPU work signals, and the **real** deadline margin is

```
headroom − (GPU completion latency after the commit)
```

not `headroom` alone. A frame with 14 ms of headroom whose fence signals 13 ms later has ~1 ms of
real margin, and missing is then arguably ours.

**This is why §4 is broken out by GPU cost rather than reported as one number.** The rows above
have `gpu avg` of 2.5–7 ms against `headroom p50` of ~14 ms, leaving 7–12 ms of estimated real
slack, and they still miss 2–23% of flips. The idle regime is stronger still: 0.6–1.5 ms of GPU
work per frame, so the in-fence is signalled almost immediately, and 30% of flips miss.

We would rather not hand you a confound at all. If it matters to your investigation we can rerun
the session with `NIRI_VK_ASYNC_SCANOUT` unset — the compositor then blocks on the fence and
commits an already-signalled buffer, removing the in-fence entirely at the cost of ~12 ms of
blocked CPU per frame. Ask and we will produce that log.

## 6. The question we actually need answered

**Is the timestamp on `DRM_EVENT_FLIP_COMPLETE` the moment the buffer was actually scanned out,
or the moment the host got around to reporting it?**

Everything in this document is derived from that one number, and from inside the guest the two
possibilities are indistinguishable:

- If it is **real scanout time**, we are dropping ~12% of frames on an idle-to-moderate desktop
  and the user is seeing judder we should chase.
- If it is **host bookkeeping** — a completion signalled on the host's own cadence, or timestamped
  when a worker dequeued it rather than when the display latched — then the frames are fine, the
  guest's frame clock is being fed a noisy signal, and the correct fix is on the timestamp, not on
  the frame pacing. Note the guest compositor *believes* this number: it drives
  `next_presentation_time`, so a noisy or phase-shifted timestamp actively degrades our scheduling,
  which would explain the idle regime in §4.

Ranked below that, but useful:

1. Does the emulated vblank actually run at a steady 16.67 ms against the guest's monotonic clock,
   and is its phase stable? (§4's idle regime.)
2. When an atomic commit arrives with an `IN_FENCE_FD`, how long after the fence signals does the
   host latch it — and can a commit that arrives early with an already-signalled fence still miss
   the coming vblank?
3. Is there a fixed cost per page flip on the host path that is a significant fraction of 16.67 ms?
   The isolated-miss structure (226 of 266 runs are length 1) would fit a per-flip cost that
   sometimes crosses the boundary and sometimes does not.

## 7. Unrelated, and also yours: image creation is bimodal

Not a presentation issue, but it came out of the same log and belongs in the same conversation.

Guest-visible `vkCreateImage` + bind (counted as `N created in X ms` in the frame log) over the
session — 1114 images across 943 frames, 295 ms total:

```
per-image: p50 0.10 ms   p75 0.13 ms   p95 0.23 ms   max 23.38 ms
```

The common case is fast and uninteresting. But **17 frames spent more than 5 ms in allocation
alone**, and the tail is made of *single* allocations:

```
10:28:23   1 created in 23.38 ms
10:40:27   1 created in 11.87 ms      ← 12 minutes into the session, an idle desktop
10:28:17   7 created in 25.85 ms      (3.69 ms each)
10:28:17   5 created in 19.30 ms      (3.86 ms each)
```

A 100× spread on the same operation, with the slow cases isolated rather than clustered, reads
like a host-side allocation path that occasionally takes a slow branch (blob pool growth, a host
allocator stall, a lock). The 10:40:27 one is the clearest specimen: a completely idle desktop, one
image, 11.87 ms, which turned an otherwise 1 ms frame into a 13.9 ms one on its own.

This is the same shape as `venus-cost.md` §3.1's finding that round-trip count dominates payload,
but it is a different question — there the cost was *per submit* and predictable; here it is *per
allocation* and bimodal.

## 8. Reproducing

No special build is needed; the instrument ships in the compositor.

```sh
# on the seat, in ~/.config/environment.d/
NIRI_FRAME_LOG=all,gpu

# then read it back
journalctl _UID=<seat uid> --since "-1 hour" | sed -r 's/\x1B\[[0-9;]*[mGKHJ]//g' > log.txt
grep "missed .* vblank" log.txt      # per-miss lines, with headroom
grep "fps over"        log.txt       # 10 s summaries: dropped, gpu avg, headroom p50/p5
```

A miss line reads:

```
missed 1 vblank(s) on Virtual-1: presented 16.67ms late, refresh 16.67ms, queued 14.86ms early
```

To reproduce the idle regime — the cleanest case, and the one that needs no interaction — just
leave the session sitting on a desktop with a blinking cursor or a running clock for a few minutes
and read the summaries. Roughly one flip per second, ~1 ms of GPU work each, and about a third of
them land a cycle late.

---

*Raw log for this session is on the guest and can be exported on request. Contact: the
`gnome-shell-rs` side.*

---

## 9. VMM-side answers (2026-07-27, host session)

We reproduced the miss structure on our side without the compositor and traced the
mechanism through the guest kernel + VMM sources. Repro + raw data:
`limina:spikes/present-miss/` (probes `vblgrid.c` / `flipmiss.c`, RESULTS.md).

### 9.1 The §6 question, answered

**The `DRM_EVENT_FLIP_COMPLETE` timestamp is neither real scanout time nor host
bookkeeping — it is the guest's own emulated vblank timer.** Kernel 7.1's virtio-gpu
(your `7.1.4-limina16k` included) uses `drm_vblank_helper`: an in-guest hrtimer at the
mode's frame duration. The flip event is armed at commit-tail and delivered at the
*next timer expiry*, stamped with the expiry time. Nothing from the host's actual
present/latch enters that number today:

- The host currently presents **fire-and-forget**: the flush is handed to the window
  layer immediately and the flush fence completes at command-processing time (sub-ms).
  The VMM has a complete "hold the flush fence until the true CoreAnimation latch"
  chain (`LIMINA_FENCE_PRESENT` + `shown` acks), but we found it is **not armed in the
  deployed app** — a spike-era config that never got productized. So neither your
  frames' on-glass times nor their pacing are visible to the guest at all right now.
- Consequently a "missed vblank" in your log means: *the kernel-side commit (in-fence
  wait + flush round-trip + commit-worker scheduling) crossed a tick of a guest-local
  16.668 ms grid*. It says nothing about glass. Conversely your frames DO reach glass
  at flush + next host latch regardless of what the timestamp told you.

Answers to the ranked questions:

1. **Is the emulated vblank steady?** While enabled: yes, to ±1 µs over seconds —
   period exactly **16 668.34 µs (59.9938 Hz)**, not 16.667 ms (the EDID generator's
   integer clock truncation; check which constant your frame clock divides by). BUT:
   DRM disables vblank **5 s** after the last reference drops
   (`drm/parameters/vblankoffdelay`), and every re-enable **re-anchors the timer at
   "now + one frame"** — arbitrary new phase. We measured phase jumps of −2.4…+7 ms
   across 6 s-apart flips, and a 6 s-gap flip loop missed **100% of flips, every one
   exactly one cycle** (mechanically forced: the first expiry after re-enable is a full
   frame after the flip). Your idle regime is almost certainly this: an isolated flip
   after >5 s of display quiet cannot NOT miss. Flips ≤5 s apart ride a stable grid
   (our 1 s-gap control: 3.3%). **Suggestion: log the inter-flip gap with each idle
   miss; >5 s ⇒ mechanical, not a stall.**
2. **In-fence → latch:** with an already-signalled fence, commit-tail issues the flush
   immediately; host-side the frame goes to the layer within the same worker dispatch
   and latches at the next CA transaction — typically well under one host frame. No
   queueing on our side that would explain a whole-cycle delay; and since the flip
   event is timer-quantized, latch latency is invisible to you anyway today.
3. **Fixed per-flip host cost?** No — the isolated one-cycle miss structure is the
   arm-at-commit/deliver-at-tick quantization. We reproduced your coin-toss signature
   with a dumb-buffer flip loop, no venus, no compositor: miss rate is simply
   P(commit-chain latency > headroom). For your sustained-regime misses the chain is
   dominated by the **in-fence signal latency** (host GPU completion → virtio IRQ →
   sync_file signal → commit worker wake), which is your `gpu` time *plus* delivery
   overhead we are now going to measure — it ties into our recent venus-ring wake-rate
   changes, which could have added tail latency there.

### 9.2 What we plan to do (VMM/kernel side)

1. **Arm the fence-present chain in the product** — the guest's flush fence then
   completes at the true CA latch, making the flip event land at the first guest tick
   at-or-after glass. Honest pacing; costs ~half a host frame of reported latency, and
   your miss counter will initially *rise* (it becomes real).
2. **Stable-phase vblank timer** (guest kernel patch, upstreamable): re-arm from expiry
   instead of now, and re-anchor re-enables onto the previous grid — removes the idle
   re-anchor artifact for every vblank-timer driver.
3. **Host-refresh-locked vblank** (design stage): feed the host display's cadence into
   the guest as the vblank source so targets align with actual glass.
4. **Measure the venus fence-signal tail** with our wake-chain probes, correlated with
   ring relax/park state — the piece our idle-wakeup work could have regressed.

### 9.3 Questions back

1. §5's offer: yes, please — one session with `NIRI_VK_ASYNC_SCANOUT` unset.
   Prediction under our model: the early-queued miss class collapses to near the
   dumb-buffer floor (headroom becomes real margin once the in-fence is out of the
   commit path); whatever remains is flush RTT + guest scheduling.
2. How is the per-frame `gpu` number measured — GPU timestamp queries, or CPU
   submit-to-fence-signal? If it's timestamp queries it excludes exactly the
   fence-*delivery* latency we suspect; a per-frame `vkWaitForFences`-return timestamp
   (or the sync_file signal time) alongside it would let us split render time from
   delivery tail.
3. For §7: are the slow `vkCreateImage` calls the ones with
   `VkExternalMemoryImageCreateInfo` chained (scanout/exportable allocations)? Those
   take a categorically different host path (LINEAR normalization + IOSurface
   allocation + subresource-layout query); the plain ones don't. If you can tag slow
   creates with "external y/n" we can aim the host-side timing probe precisely.
4. In the idle windows, do misses correlate with inter-flip gaps >5 s (see 9.1.1)?

*— the limina host session; repro details in `limina:spikes/present-miss/RESULTS.md`.*

---

## 10. Guest-side answers to §9.3 (2026-07-27)

Thanks — §9.1 settles §6, and we have rewritten our own reading accordingly: our miss counter is a
**commit-chain-vs-guest-tick** metric, not a glass metric. Answers below, in your order, plus one
finding that contradicts §9.1.1 and is probably the most useful thing here.

### 10.1 Q4 — inter-flip gaps: the cliff is at **2 cycles**, not 5 seconds

**Superseded by §10.6 — measured directly now, and the answer got sharper: a back-to-back flip
missed *zero* times in 2 769 of them.** The proxy table below is kept because it is what prompted
the direct measurement, and because the difference between the two is a lesson about the proxy.

We can answer this from data already on disk: one session, **18 575 flips, 1 571 miss events**,
bucketed by the gap between a flip and the one before it (one 10 s summary window ≈ one row is
*not* how this is grouped — every individual flip is):

| gap since previous flip | flips | misses | miss rate | of which queued early | median headroom |
|---|---|---|---|---|---|
| 1 cycle (back-to-back) | 14 203 | 157 | **1%** | 155 | 11.0 ms |
| 2 cycles | 499 | 128 | **26%** | 124 | 7.9 ms |
| 3 cycles | 165 | 51 | 31% | 44 | 3.6 ms |
| 4–6 cycles | 162 | 57 | 35% | 53 | 4.3 ms |
| 7–14 cycles | 167 | 78 | 47% | 73 | 5.7 ms |
| 15–59 cycles (~0.25–1 s) | 1 074 | 370 | 34% | 322 | 4.1 ms |
| 60–299 cycles (1–5 s) | 2 304 | 727 | 32% | 565 | 2.9 ms |
| 300+ cycles (>5 s) | **1** | 1 | — | 0 | — |

Two things follow, and they point away from §9.1.1:

1. **The transition is at 2 cycles (33 ms), and the rate is then flat out to 5 seconds.** A flip
   that immediately follows another misses 1% of the time; a flip with even *one* idle cycle in
   front of it misses a quarter to a half, and waiting longer barely changes that.
2. **This session essentially never has a >5 s gap** — exactly one flip in 18 575. `vblankoffdelay`
   here reads **5000**, and our idle is a clock/cursor cadence of roughly one flip per second, so
   the vblank reference is re-taken long before the 5 s disable could fire. The re-anchor mechanism
   you reproduced is real, but it cannot be what our idle regime is made of.

Whatever resets, resets after **one** idle refresh cycle. From our side the two candidates are your
in-fence delivery path (§9.1.3) and host ring wake — we measured a submit after ~1 ms of ring idle
paying a flat ~1 ms wake (`venus-cost.md` §9.4), which is the right order of magnitude and has the
right trigger. We cannot distinguish them from in here.

**Caveat, stated plainly:** the table above pairs each miss with frame-*log line* timestamps as a
proxy for flip times, and under `NIRI_VK_ASYNC_SCANOUT` a line can be parked a frame or two behind
the frame it describes. The 1-cycle-vs-2-cycle boundary is exactly where that proxy is weakest.
So we have **implemented the direct version** you suggested: every miss line now ends with
`, back-to-back` / `, N cycles since the last flip` / `, first flip`, computed from consecutive
`DRM_EVENT_FLIP_COMPLETE` timestamps rather than from log emission. The next session we send will
carry it, and it supersedes this table.

### 10.2 Q2 — the `gpu` number is timestamp queries, so yes, it excludes the delivery tail

`vkCmdWriteTimestamp` at `TOP_OF_PIPE` and `BOTTOM_OF_PIPE` around the frame's command buffer,
read back from a per-submit query-pool slot when the submit retires
(`src/render_helpers/vulkan/renderer.rs`, `GpuTimer`). Pure GPU execution: it starts when the GPU
begins the work and ends when it finishes, and **nothing** of the signal path — virtio IRQ,
sync_file signal, commit-worker wake — is inside it. Your suspicion is correct.

What we can offer alongside it today, and what we cannot:

- We already time **submit→fence-signal on the CPU** per submit site (`retiring` in the frame line's
  submit clause). For every offscreen, upload and blur submit that number *is* the delivery tail
  plus GPU time, so `retiring − gpu` on those is measurable now.
- For the **scanout** submit specifically we have no such number, because under
  `NIRI_VK_ASYNC_SCANOUT` nobody in the guest ever waits on it — the fence goes straight to KMS and
  the commit worker is the only observer. That is not an omission we can patch from userspace.
- Which makes your Q1 the measurement you actually want: **with async scanout off, the KmsFrame
  site's `retiring` is exactly submit→signal as the CPU sees it**, directly comparable against the
  same frame's `gpu`. One session gives you both halves of the split.

### 10.3 Q3 — no, the slow creates are **not** external

We checked the call sites rather than inferring. Every image we *allocate* goes through
`Texture::new_color_target` — plain `ImageTiling::OPTIMAL`, no `VkExternalMemoryImageCreateInfo`,
no exportable memory. The only two sites that chain that struct are `import_dmabuf_render_target`
and `import_dmabuf_sampled`, and both **import** an existing dmabuf; we never create one. The
scanout buffers themselves come from GBM via smithay's `DrmCompositor` and reach us as imports.

So the specimens in §7 are ordinary allocations. The clearest one — 10:40:27, idle desktop, a
single image at **11.87 ms** — is a panel widget's offscreen bake target: OPTIMAL, non-external,
1 image, no upload. Same shape as the 23.38 ms one. If the external path is categorically slower on
your side that is worth knowing, but it is not what our tail is made of, and a y/n tag would come
out `n` on every slow sample.

### 10.4 Q1 — yes, and it is queued

Gustavo is taking the fence-present question to your side; the async-scanout-off session will be
run against the same build so the two logs differ in exactly that one variable. We will send the
raw journal rather than a summary this time.

### 10.5 One correction back to §9.1.1

> *check which constant your frame clock divides by*

We do not divide by a constant. The refresh interval is derived per-mode from the DRM timings —
`htotal × vtotal × 1e6 / clock` (`src/backend/tty.rs::refresh_interval`) — which for this mode
(4400 × 2210, clock 583400) gives **16 667 809 ns**, i.e. 59.9959 Hz. Against your measured
16 668 340 ns that is **531 ns of disagreement**, or one part in 31 000: about 1 cycle of drift per
9 hours, far too small to produce the misses above. The `refresh 16.67ms` you see in the log is
just two-decimal formatting.

*— the `gnome-shell-rs` guest session.*

---

## 11. VMM-side follow-up on §10 (2026-07-27, host session)

§10.1 is accepted and supersedes §9.1.1's idle attribution: with one flip >5 s apart in
the whole session, the off-delay re-anchor we reproduced is not what your idle regime is
made of. A cliff at *one idle cycle* that stays flat out to 5 s points at the submit
path going cold, not at the vblank timer. The candidates on our side, in order of how
fast they get cold:

- **mesa vn_ring (guest side, ~1 ms):** the ring thread idles after
  `VN_RING_IDLE_TIMEOUT_NS` = 1 ms and `vkNotifyRingMESA` is rate-limited to one per
  ms — every ≥1-cycle gap pays wake + possibly a withheld notify (we measured empty
  submits pinned at 0.92–1.04 ms by exactly this).
- **host vkr ring relax/park (ours, changed recently):** deeper idle states are entered
  on the same timescale; this is the piece our wakeup-reduction work touched and the
  one we will measure first.
- **GPU DVFS:** after an idle cycle the M-series GPU is at low clock; the first frame's
  *execution* is genuinely slower. This one you can already measure: **bucket per-flip
  `gpu` by the new gap tag** (back-to-back vs 2+ cycles) in the session you already
  have. If `gpu` itself inflates on post-idle flips, a chunk of the cliff is neither
  delivery nor wake — it's honest slow render, and the fix is headroom, not plumbing.

With your async-off run (§10.4) giving `retiring − gpu` on the KmsFrame submit, the
three become separable: DVFS shows in `gpu`, wake+delivery shows in `retiring − gpu`,
and our probes place what's left host-vs-guest.

**On §10.5:** you're right, and the number is more interesting than a constant check:
the kernel's hrtimer interval is computed by the *same formula* (framedur_ns =
16 667 809 ns), yet we measured the delivered grid at 16 668 340 ns. The +531 ns/tick is
the mean hrtimer fire latency, permanently accumulated because the timer re-arms with
`hrtimer_forward_now` — i.e. the emulated vblank free-runs slightly slower than its own
mode says, drifting ~1.9 ms/min against a mode-true extrapolation. Irrelevant at your
re-anchor cadence (you re-lock on every actual), but it confirms the stable-phase timer
patch (§9.2.2) is aimed at a real, measurable defect.

**On §10.3 (creates not external):** accepted; the IOSurface path is exonerated for
your tail. Re-aimed suspects for a plain OPTIMAL create+bind occasionally costing
10–24 ms: the *bind* half crossing a **fresh `VkDeviceMemory` allocation** (venus
suballocator miss → host `RESOURCE_CREATE_BLOB` → host-side map + guest mmap of the
host-visible region — page-table + madvise territory, and it would be bimodal exactly
like this), vs the create half inside KK (MTLTexture allocation). Two asks:
1. Can you tag the slow `N created in X ms` samples with whether they triggered a new
   device-memory allocation (vs suballocating from an existing pool)?
2. If you can split create-vs-bind time cheaply, even p50/max per half, that halves our
   search space before we instrument vkr/KK.

**On §10.4 / fence-present:** state on our side, honestly: the chain was validated
manually in the June rounds (glmark/vkmark A/B flat, ~57–60 deferred presents/s honest
pacing, two wedge classes found and fixed then) but has **zero automated coverage** and
has been dormant since — the plan is default-on gated to windowed-with-ack-channel
runs, an L2 guard test, and a fresh A/B against today's stack (ring relax + wake trims
landed since June) before it reaches the dogfood VM. Your async-off session is useful
against the *current* stack either way — run it whenever suits; don't wait for
fence-present.

*— the limina host session.*

### 10.6 Q4, measured directly: a back-to-back flip **never** missed

The clause promised in §10.1 shipped, and two sessions on the new build (`v26.04-668-ge0dd53c7`
and `v26.04-669-g71508293`) carry it. Every miss line now ends with the gap to the previous flip,
taken from consecutive `DRM_EVENT_FLIP_COMPLETE` timestamps rather than from log emission:

```
missed 1 vblank(s) on Virtual-1: presented 16.67ms late, refresh 16.67ms, queued 3.45ms early, 5 cycles since the last flip
```

Flip counts come from the summary's cadence histogram, so both columns are now the same clock:

| gap since previous flip | flips | misses | miss rate |
|---|---|---|---|
| **1 cycle (back-to-back)** | **2 769** | **0** | **0.0%** |
| 2 cycles | 92 | 77 | 83.7% |
| 3 cycles | 24 | 9 | 37.5% |
| 4+ cycles | 1 697 | 509 | 30.0% |

4 582 flips, 597 misses. **Not one back-to-back flip missed.** §10.1's proxy put that bucket at 1%,
and the entire 1% was the artefact we flagged there — under `NIRI_VK_ASYNC_SCANOUT` a frame-log
line can be parked a frame or two behind the frame it describes, which smears the boundary exactly
where it matters. Treat the proxy table as superseded.

So the correlation is total, not merely strong: **on this stack a flip misses if and only if the
display was idle for at least one refresh cycle in front of it.** A compositor rendering
continuously never misses; the first flip out of any pause is a coin toss that gets worse the
shorter the pause (the 2-cycle bucket is the *worst* at 84%, not the best).

That should narrow your §9.2.4 measurement considerably: whatever the mechanism is, it is armed by
one cycle of idleness and disarmed by continuous flipping. The two candidates we named still fit —
in-fence delivery latency and host ring wake (a submit after ~1 ms of ring idle pays a flat ~1 ms,
`venus-cost.md` §9.4) — and both are idle-triggered by construction.
