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
