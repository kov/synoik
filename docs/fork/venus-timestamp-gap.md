# Venus GPU-timestamp gap — VM-stack handoff

**Audience:** whoever works on the VM / host graphics stack (guest Mesa `venus`, our
`virglrenderer` fork's Venus backend, the VMM's virtio-gpu device, the host Vulkan driver).
Written from inside the guest (`gnome-shell-rs` dev VM) on 2026-07-24.

**One-line summary:** this stack advertises Vulkan timestamp queries in full and then **silently
resolves every one of them to zero** — the queries come back *available*, not *unavailable*, so
nothing in the API distinguishes "the GPU pass took no measurable time" from "the driver does not
implement this". The device clock itself works: `VK_EXT_calibrated_timestamps` returns a live
`DEVICE` timestamp advancing at exactly one tick per nanosecond. So the gap is the **query-pool
write/resolve path**, not a missing GPU clock.

**Reproducer:** [`venus-bugs/repro-vk-timestamp-query/`](./venus-bugs/repro-vk-timestamp-query) —
`cargo run`, no arguments. Run it against Venus and against lavapipe on the same guest for the
contrast.

**What it costs us:** the compositor's frame logger (`NIRI_FRAME_LOG`, `src/frame_log.rs`) can time
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

The Khronos validation layer reports nothing against this usage (`NIRI_VK_VALIDATION=1`), and the
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
NIRI_FRAME_LOG=1,gpu
```

A working stack logs `(gpu 3.21ms)` inside the frame line and `gpu avg …` in the summary, and never
emits `this device advertises timestamp queries but does not write them`.

If instead the right answer is that this stack *cannot* support timestamp queries, the conforming
way to say so is `timestampValidBits = 0` on the queue family. The guest already treats that as
"GPU timing unavailable" and stays quiet about it — no zero-valued measurements, no warning, no
heuristic.

## 7. Related

- [`venus-explicit-sync-gap.md`](./venus-explicit-sync-gap.md) — the other place a Venus capability
  question shaped a compositor decision; also the source of the "probe it, do not assume the
  advertised surface" habit that found this.
- [`venus-bugs/README.md`](./venus-bugs/README.md) — two earlier Venus/gbm findings with the same
  reproducer-crate convention.
- `src/frame_log.rs` — the consumer, including the `NIRI_FRAME_LOG` grammar.
- `src/render_helpers/vulkan/renderer.rs` — `GpuTimer`, the all-zero heuristic, and
  `timestamp_ticks` (the tick arithmetic, unit-tested because this device cannot exercise it).
