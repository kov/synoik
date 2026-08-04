# Venus GPU-timestamp gap — VM-stack handoff

**Audience:** whoever works on the VM / host graphics stack (guest Mesa `venus`, our
`virglrenderer` fork's Venus backend, the VMM's virtio-gpu device, the host Vulkan driver).
Written from inside the guest (`gnome-shell-rs` dev VM) on 2026-07-24.

> **RESOLVED 2026-07-26 — fixed host-side, in the host Vulkan driver (KosmicKrisp), not in Venus.**
> Two stacked bugs; see §7 for the root cause, the validation (100/100 on this VM's own host GPU,
> where it was 0/100), the ~0.4%-of-a-frame cost, and how to tell a deployed build has it.
> **Everything between here and §7 is the original handoff, written while the cause was unknown —
> read it as the investigation, not as current state.** The one correction it needs inline: the
> partial hit rate it anticipates is gone, and every pair should now be written.

**One-line summary:** this stack advertises Vulkan timestamp queries in full and then **silently
resolves every one of them to zero** — the queries come back *available*, not *unavailable*, so
nothing in the API distinguishes "the GPU pass took no measurable time" from "the driver does not
implement this". The device clock itself works: `VK_EXT_calibrated_timestamps` returns a live
`DEVICE` timestamp advancing at exactly one tick per nanosecond. So the gap is the **query-pool
write/resolve path**, not a missing GPU clock.

**Reproducer:** [`venus-bugs/repro-vk-timestamp-query/`](./venus-bugs/repro-vk-timestamp-query) —
`cargo run`, no arguments. Run it against Venus and against lavapipe on the same guest for the
contrast.

**What it costs us:** the compositor's frame logger (`SYNOIK_FRAME_LOG`, `src/frame_log.rs`) can time
everything *except* how long the GPU spent on a pass. Everything else — per-phase CPU cost, dropped
frames from the DRM vblank sequence, widget bake counts — is unaffected and works here today. See
§4 before deciding how much this is worth.

---

## 1. What was measured

The compositor's Vulkan renderer submits and fence-waits synchronously, so GPU execution time is
already inside the CPU-measured "submit" phase with no way to separate the two. Timestamp queries
were added to split them (`gpu_timer_begin` / `gpu_timer_end` / `gpu_timer_collect` in
`src/render_helpers/vulkan/renderer.rs`): reset a two-slot pool and write a timestamp at the top of
the command buffer, write another before ending it, read the pair back after the fence wait the
renderer already does.

The Khronos validation layer reports nothing against this usage (`SYNOIK_VK_VALIDATION=1`), and the
same code on lavapipe returns sensible values.

## 2. Environment

| | |
|---|---|
| Vulkan driver | `driverName = venus`, `driverInfo = Mesa 26.1.4` |
| Vulkan device | `Virtio-GPU Venus (Apple M4 Pro)`, `INTEGRATED_GPU`, API `1.3.353` |
| Guest kernel | `Linux 7.1.4-limina16k aarch64` |
| VMM / host | `systemd-detect-virt = vm-other`; limina VM, our `virglrenderer` fork, host GPU Apple M4 Pro |
| Control ICD | lavapipe (`lvp_icd.aarch64.json`), same guest, same reproducer |

## 3. The finding

### 3.1 What the device claims

```
device: "Virtio-GPU Venus (Apple M4 Pro)"
  timestampPeriod             = 1
  timestampComputeAndGraphics = true
  timestampValidBits (gfx q)  = 64
```

Full support, on the graphics queue, at nanosecond resolution. `vulkaninfo` agrees.

### 3.2 What actually comes back

A command buffer whose only contents are `vkCmdResetQueryPool` plus two `vkCmdWriteTimestamp`
calls, submitted and fence-waited to completion:

```
after submit + fence wait:
  WAIT               -> Ok(()), values [0, 0]
  (no WAIT)          -> Ok(()), values [0, 0]
  WITH_AVAILABILITY  -> Ok(()), query0 value 0 avail 1, query1 value 0 avail 1
```

**The availability word is the important line.** `avail = 1` means the implementation considers
each query *resolved*. It is not stalling, not returning `VK_NOT_READY`, not erroring. It resolved
the query and the answer is zero.

That makes every read path behave "correctly":

- `VK_QUERY_RESULT_WAIT_BIT` returns `VK_SUCCESS` — which is right, since the result *is*
  available.
- Omitting `WAIT` also returns `VK_SUCCESS` rather than `VK_NOT_READY`.
- Asking for availability confirms the query is available.

So there is **no error to check and no status to branch on**. A caller that follows the spec
exactly gets a well-formed zero. The only way to detect this from the guest is a heuristic: a
free-running GPU clock cannot read 0 at *both* ends of a pass, so an all-zero pair means the writes
never happened. That is what the compositor now does — it warns once and disables GPU timing rather
than reporting `gpu 0.00ms`, which would read as an instantaneous GPU.

### 3.3 The same code on lavapipe

Identical declared limits, real results:

```
device: "llvmpipe (LLVM 22.1.8, 128 bits)"
  timestampPeriod             = 1
  timestampComputeAndGraphics = true
  timestampValidBits (gfx q)  = 64

after submit + fence wait:
  WAIT               -> Ok(()), values [12338652120341, 12338652171341]
  WITH_AVAILABILITY  -> Ok(()), query0 value 12338652120341 avail 1, query1 value 12338652171341 avail 1
```

(51 µs between the two writes for an otherwise-empty command buffer, which is the expected shape.)
This rules out the reproducer as the confound and puts the gap in the Venus path.

### 3.4 The device clock is fine

`VK_EXT_calibrated_timestamps` is advertised **and works** on the same device:

```
calibrated time domains: [DEVICE, CLOCK_MONOTONIC, CLOCK_MONOTONIC_RAW]
  DEVICE   910245973975916 -> 910246074717958 (advanced 100742042 ticks over ~100ms)
  MONOTONIC 12366537509646 -> 12366638252146
  max deviation: 60377 ns
```

Two things follow:

1. There **is** a device timestamp domain, it advances, and `timestampPeriod = 1` is *accurate* —
   100 742 042 ticks over a ~100 ms sleep is 100.7 ms at 1 ns/tick. The advertised limits are not
   stub values; they describe a real clock.
2. Therefore this is not "no GPU clock reaches the guest". Something between
   `vkCmdWriteTimestamp` and the query result is dropping the write, while the clock it would have
   sampled is readable by another route.

The `DEVICE` domain sits in a different epoch from `CLOCK_MONOTONIC` (≈ 910 000 s vs ≈ 12 366 s of
guest uptime), which is consistent with it being the host GPU's clock forwarded through.

## 4. What this does and does not cost

**Unaffected — works today, on this VM:**

- Per-phase CPU frame cost (elements / collect / submit / queue / callbacks / captures).
- Dropped-frame detection from the DRM vblank sequence, which is the signal that actually
  corresponds to a user-visible stutter.
- Widget bake counts, element counts, damage and overview/animation context.
- The `submit` phase still *includes* GPU execution, because the renderer fence-waits its own
  submit. A GPU-bound stall is therefore still visible as a slow frame.

**Lost:** the attribution *within* `submit` — how much was recording and how much was the GPU
executing. On a slow frame you learn that submit took 12 ms, but not whether the GPU was busy for
11 of them or for 1.

> **Superseded by §7.** This section sized the loss to help decide whether the VMM work was worth
> doing. It was done, and the attribution below is no longer lost — keep reading §4 for what the
> gap *was*, not for what it costs now.

That is worth having but it is a second-order tool, so weigh the VMM work accordingly. Note also
that the compositor's `submit` includes an unavoidable full pipeline stall (the synchronous
fence wait), so the GPU number would mostly answer "is the GPU or the CPU the bottleneck" rather
than feed a per-pass budget.

## 5. Where to look

Ordered by where the evidence points, not by ease:

1. **The Venus protocol path for query pools.** `vkCmdWriteTimestamp` has to be encoded into the
   ring, decoded host-side, and replayed against a host query pool; the results then have to come
   back through `vkGetQueryPoolResults`. The availability word arriving as `1` says the *result
   readback* path is alive and answering — so the likely break is either the command never being
   replayed host-side, or the readback filling from a pool that was never written.
2. **The host driver's timestamp support.** If the host Vulkan implementation is the one silently
   accepting `vkCmdWriteTimestamp` and resolving to zero, the fix belongs there and Venus is only
   faithfully relaying it. Worth checking first whether the host driver passes the same reproducer
   natively — that single run splits the problem in half.
3. **Metal's counter-sample plumbing**, if the host driver maps timestamps onto
   `MTLCounterSampleBuffer`. Metal only supports GPU counter sampling at certain boundaries and on
   certain hardware, and a driver that cannot honour a mid-command-buffer sample point may choose
   to resolve the query rather than fail it. If that is what is happening, the honest behaviour
   would be to report `timestampValidBits = 0` on the queue — which the guest already handles: our
   renderer checks it and skips GPU timing without a word.

**Please do not "fix" this by making the guest tolerate zeros.** It already does, and defensively;
the interesting question is entirely host-side.

## 6. How to tell it is fixed

Run the reproducer under Venus. Fixed looks like the lavapipe output in §3.3: both queries
available, two large advancing values, a plausible delta. Then in the compositor:

```
SYNOIK_FRAME_LOG=1,gpu
```

A working stack logs `(gpu 3.21ms)` inside the frame line and `gpu avg …` in the summary, and never
emits `this device advertises timestamp queries but does not write them`.

If instead the right answer is that this stack *cannot* support timestamp queries, the conforming
way to say so is `timestampValidBits = 0` on the queue family. The guest already treats that as
"GPU timing unavailable" and stays quiet about it — no zero-valued measurements, no warning, no
heuristic.

## 7. Resolved (2026-07-26) — two bugs in the host Vulkan driver

§5 asked for one experiment before anything else: *does the host driver pass the same reproducer
natively?* That was the right call and it split the problem exactly in half — the answer was no,
and one run exonerated everything above the host driver. What follows is what it turned out to be.

**Not Venus, not our virglrenderer fork, not the VMM, not the guest.** Reproduced natively on the
host with no VM running at all. It is **KosmicKrisp**, the host Vulkan-on-Metal driver, and it is
**machine-dependent**: an M1 Max passes the same binary, the M4 Pro this VM runs on fails. That is
why the advertised surface was honest rather than a stub — the driver does implement timestamp
queries, and on other hardware they work.

Two independent bugs, stacked, which is why the first fix only half-helped:

1. **The sample was never taken.** The driver built a counter-sampling blit encoder that encoded no
   data movement. **Metal elides a blit encoder that encodes nothing**, and an elided encoder never
   takes its counter sample. Fixed by giving that encoder real work (`patches/kosmickrisp/0012`,
   deployed here 2026-07-25 — the change that moved the hit rate from nothing to the ~7% of pairs
   your `UNWRITTEN_LIMIT` comment records).

2. **The GPU resolve could not see the sample.** A Metal counter sample only materialises when the
   command buffer that took it **completes**. A `resolveCounters:` encoded before that writes
   **zero, silently** — not `MTLCounterErrorValue`, not a failure. On the M4 Pro that is 94% of the
   time from the same command buffer, and still 18% from a later one. And because KosmicKrisp's
   availability test for a timestamp pool is `value != UINT64_MAX`, that zero read back as *a query
   that resolved to zero*.

That second one is exactly the `avail = 1, value 0` your §3.2 pinned down, and it is worth stating
plainly: **the driver did consider the query resolved, because as far as it could tell, it was.**
The two bugs are also indistinguishable from the guest — one wrote nothing and the other wrote
zero, and both arrive as a well-formed available zero. Only a CPU-side read of the Metal sample
buffer, from inside the driver, separated them. Your §3.2 was right that there is nothing in the
API to branch on; there was nothing on the host side either until the driver was instrumented.

Fixed in `patches/kosmickrisp/0013`: the resolve moved off the GPU entirely and onto the CPU, in
the command buffer's completion handler, which is the first moment the sample is readable. That
moves the report write out of GPU command order, so the ordering it used to get for free is now
explicit — the sampling encoder marks the report unavailable in command order, and an in-stream
`vkCmdResetQueryPool` or `vkCmdCopyQueryPoolResults` waits on an event the completion handler
signals. (Venus's query-feedback command buffer rides in the *same* submission as the frame, so
that wait is on the common path, not a corner case. It is costed below.)

### Validation

The reproducer's shapes, run natively on **this VM's host** — the same M4 Pro, no VM involved:

| | before | after |
|---|---|---|
| `vkGetQueryPoolResults` | `0 / avail=1`, every run | **100/100 real advancing timestamps** |
| `vkCmdCopyQueryPoolResults` | `0 / avail=1`, every run | **100/100** |
| …the same, in a separate command buffer submitted alongside — **what Venus actually produces** | `0 / avail=1`, every run | **100/100** |

Zero occurrences of a zero-presented-as-available in 300 samples. Also checked: a full enhanced
desktop boots and renders normally on the fixed driver, and the host's own GL timer-query path
(zink-on-KosmicKrisp `glQueryCounter` / `GL_TIME_ELAPSED`) still returns sane monotonic
nanoseconds, so nothing regressed to buy this.

**Not yet measured: this reproducer, in this guest, on the fixed driver.** That is the run that
closes it out from your side, and it needs the deploy in §7.4.

### 7.1 Expected hit rate: all of them

**The partial-hit-rate expectation is retracted.** The ~7% of pairs you measured on 2026-07-25 was
bug #2 doing the dropping — and the host-side guess that preceded it ("~82% per timestamp, so about
two thirds of pairs") was wrong in the same direction. Your 7% was a measurement and that 82% was
never more than an extrapolation from an isolated Metal probe; the measurement wins. With the
resolve off the GPU there is no drop mechanism left: every sample that is taken is read back.
**Expect every pair written.**

That is not an argument for ripping out the defences. `UNWRITTEN_LIMIT = 256`, the all-zero
heuristic, and `SANE_LIMIT` cost nothing and should simply stop firing — which is the right posture
for a paravirtualised device that has surprised you twice. But the premise recorded in that
constant's doc comment — *"the host-side fixes for our Venus VM are aiming at a partial hit rate"* —
is obsolete, and if the constant is ever retuned, note that 256 was sized against a dry-spell
distribution that should no longer exist.

### 7.2 What it costs

Moving the resolve to the CPU is not free: a timestamp followed by an in-stream consumer now ends
the sampling command buffer and makes the next one wait, on the GPU, for an event the CPU signals.
That is a GPU→CPU→GPU hop inside the frame. Measured on this host (median of 3000 submits, two
runs, stable to the digit shown):

| per submission | before | after |
|---|---|---|
| no timestamps at all | 0.156 ms | 0.158 ms |
| 2 timestamps, read after the fence | 0.206 ms | **0.199 ms** |
| 2 timestamps + in-stream copy — **the Venus shape** | 0.221 ms | **0.286 ms** |

The first row is the control: work that does not use timestamp queries is untouched. The second got
slightly *faster* (two GPU resolve encoders per frame deleted, no barrier needed when nothing reads
the report early). The third is the real cost: **~0.065 ms per submission** that carries both a
timestamp and a consumer.

For this compositor that is **one barrier per frame** — both timestamps share one batch, and the
cost is per submission rather than per timestamp — so **~0.4% of a 16.7 ms frame at 60 Hz**, ~0.8%
at 120 Hz. It lands inside the `submit` phase your frame log already measures. Worth knowing that
the number you are about to be able to measure costs a little to measure; not worth restructuring
anything over.

(Caveat on reading the "before" column: on this machine that driver returned zeros, so those are
"fast but wrong" numbers. The delta is the price of getting an answer at all.)

### 7.3 One guest-side note

`gpu_timer_collect` passing `vk::QueryResultFlags::WAIT` is the right call and stays right. A bare
`vkGetQueryPoolResults` **without** `WAIT` can now legitimately return `VK_NOT_READY` when issued
the instant the queue goes idle, because the value is written by a completion handler that runs
just after the fence signal rather than by the GPU in command order. The driver publishes any
already-resolved sample on such a poll, which closes that window in almost every run, but `WAIT`
sidesteps the question entirely and costs nothing here — by the time you ask, the value is normally
already in the report. No change needed; this is only recorded so a future refactor does not drop
the flag thinking it is free.

Nothing else changes on the guest side. No Mesa change, no kernel change, and
`timestampValidBits` stays 64 — it was never dishonest, the driver really can timestamp.

### 7.4 Getting it

Host-side only. It lands with the next `limina.app` deploy on this host. Check the deployed
artifact rather than the commit — the app bundles the driver directly, so an unrebuilt one ships
stale and silent:

```sh
# on the host, not in the guest
nm -a /Applications/limina.app/Contents/Frameworks/libvulkan_kosmickrisp.dylib \
  | grep -c mtl_device_needs_split_counter_resolve
# 0 = has the fix; nonzero = an older build (that symbol only exists pre-0013)
```

Then §6's own test applies unchanged: the reproducer should look like the lavapipe output in §3.3 —
both queries available, two large advancing values, a plausible delta — and `SYNOIK_FRAME_LOG=1,gpu`
should log `(gpu N.NNms)` with the warning never firing.

## 8. Related

- [`venus-explicit-sync-gap.md`](./venus-explicit-sync-gap.md) — the other place a Venus capability
  question shaped a compositor decision; also the source of the "probe it, do not assume the
  advertised surface" habit that found this.
- [`venus-bugs/README.md`](./venus-bugs/README.md) — two earlier Venus/gbm findings with the same
  reproducer-crate convention.
- `src/frame_log.rs` — the consumer, including the `SYNOIK_FRAME_LOG` grammar.
- `src/render_helpers/vulkan/renderer.rs` — `GpuTimer`, the all-zero heuristic, and
  `timestamp_ticks` (the tick arithmetic, unit-tested because this device cannot exercise it).
