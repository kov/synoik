# Submit-timeline counter disagrees under lavapipe — VM-stack handoff

**Audience:** whoever works on the VM / host graphics stack (guest Mesa, our `virglrenderer` fork's
Venus backend, the VMM's virtio-gpu device) — and equally anyone who knows Mesa's **lavapipe**
timeline-semaphore implementation, since that is the device this shows up on. Written from inside
the guest (`gnome-shell-rs` dev VM) on 2026-08-06.

**Status: OPEN, and deliberately small.** One test is `#[ignore]`d. Nothing user-visible is known to
be affected. This exists so the VM/host side can say "that's us" or "that's you" before we spend
more on it.

**One-line summary:** a test that asserts every `vkQueueSubmit` goes through `Gpu::submit` — by
comparing a submit counter against a timeline-semaphore value — failed **once** on lavapipe with
`submits = 2, timeline = +1`, and then refused to reproduce across 16 further runs.

---

## 1. What the test asserts, and why it exists

`render_helpers::vulkan::tests::every_submit_is_chained_on_the_queue_timeline`.

Our renderer's correctness rests on **one queue with one timeline semaphore**: submits are totally
ordered on it, which is what lets work be *recorded* into an already-scheduled command buffer
instead of waiting on a fence (`docs/fork/frame-submit-discipline.md`). A `vkQueueSubmit` that goes
around `Gpu::submit` is unordered against everything else, and nothing about the pixels would show
it — hence a counter test rather than a pixel test.

It drives two different submit paths (a glyph upload via `run_commands`, a render pass via
`VulkanFrame::finish`), then asserts:

```
Gpu::submit_order_value() delta  ==  synoik_vk::stats::submits() delta
```

## 2. The observation

Full workspace suite, guest, `VK_DRIVER_FILES` forced to `lvp_icd.aarch64.json`:

```
device: "llvmpipe (LLVM 22.1.8, 128 bits)" [cpu] api 1.4.354  graphics-queue: Some(0)  render-node: None
enabling device extensions: [VK_KHR_external_memory_fd, VK_EXT_external_memory_dma_buf,
  VK_EXT_image_drm_format_modifier, VK_EXT_queue_family_foreign,
  VK_KHR_external_semaphore_fd, VK_KHR_external_fence_fd]

assertion `left == right` failed: 2 submits advanced the timeline by 1 —
  one of them bypassed Gpu::submit and is unordered against the rest
  left: 1   right: 2
```

**Everything else passed: 1677 of 1678.** That is the headline for the VM side — lavapipe runs this
compositor's whole renderer suite, including the DRM-modifier and dmabuf paths, which is why we want
it as a CI device in the first place.

## 3. What we did to pin it down, and what it cost us

| Attempt | Result |
|---|---|
| 15 × the test alone, under lavapipe | **clean, 15/15** |
| Full suite again under lavapipe, with a polling probe | **clean**, and the probe waited **0 ms** |
| Full suite under Venus (the normal device) | clean, always |

The probe replaced the single read with "poll `submit_order_value()` until it reaches the submit
count, up to 2 s, and report how long that took". It reported `waited 0ms` on **every** run,
including inside the full parallel suite. So we could not catch it in the act.

## 4. Two candidate mechanisms — and what would discriminate them

**(a) The read is of *completed* submits, and completion lags.** `submit_order_value()` is
`vkGetSemaphoreCounterValue` on the order semaphore, and its own doc comment says "how many submits
the queue has **completed**". The test compares that against submits *issued*. On a device where a
submit is still in flight at read time, issued > completed and the test fails without anything being
wrong. This is the mechanism we would bet on a priori — **but the probe argues against it**, since a
lagging counter should have made the probe wait at least sometimes.

**(b) Something really did bypass `Gpu::submit` on that run.** Which would be a genuine defect, in
our code or in a path lavapipe takes and Venus does not. Note the counter is **thread-local**
(`SUBMITS` is a `Cell` in a `thread_local!`), and libtest gives each test its own thread, so
cross-test contamination is *not* an available explanation — we checked that specifically, and it
was the theory we liked before we looked.

**What would settle it:** a lavapipe-side answer to "can `vkGetSemaphoreCounterValue` on a timeline
semaphore transiently report a value lower than the last *signalled* one, under load, in a way a
1 ms poll would not see?" If yes → (a), and the fix is ours: wait on the timeline before comparing.
If no → (b), and it is worth real digging, because the invariant is load-bearing.

## 5. What we did in the meantime

`#[ignore]`d the test, with the reason in the attribute so a run says why it skipped. **Not**
deleted, and not made lenient: an intermittent failure in a fail-closed invariant test is worse than
no test, because the next person to see it reads it as noise and stops trusting the check.

Re-enable by dropping the attribute once this doc lands on an answer.

## 6. Environment

| | |
|---|---|
| Guest | Fedora 44 aarch64, kernel `7.1.6-limina16k` |
| Device under test | `llvmpipe (LLVM 22.1.8, 128 bits)`, Vulkan 1.4.354, Mesa lavapipe ICD |
| Normal device | Virtio-GPU Venus (Apple M4 Pro host), Vulkan 1.4.343 |
| VMM / host | limina VM, our `virglrenderer` fork, host GPU Apple M4 Pro |
| Reproducer | `VK_DRIVER_FILES=/usr/share/vulkan/icd.d/lvp_icd.aarch64.json cargo test --workspace -- --include-ignored every_submit_is_chained` |

Note the device is a **CPU** rasterizer, so unlike the other docs in this directory the VMM is not
obviously in the path at all — lavapipe does not go through virtio-gpu. That is itself a useful
datum: if this is a driver-side counter subtlety, it is Mesa's, not the VMM's, and the VM stack can
close this out quickly.

---

## Related

- `docs/fork/frame-submit-discipline.md` — why the single-timeline invariant matters, and what
  breaks if a submit escapes it.
- `docs/fork/foundation.md` — the other "a counter reads wrong on this stack" handoff,
  resolved host-side. Worth reading for the shape, not the cause; that one was reproducible on
  demand and this one is not.
