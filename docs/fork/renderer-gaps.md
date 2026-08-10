# Owned Vulkan renderer — known gaps and deferred work

Status: **2026-07-17 — done. Phase C slice 8 deleted the co-resident GLES renderer**, and
smithay now builds without `renderer_gl`/`backend_egl`/`renderer_multi` (`renderer_pixman` stays:
it is the render tests' CPU oracle, not a fallback). The owned Vulkan renderer is the only renderer,
and the only one built.

This records what the owned renderer *cannot* do that the GLES renderer could, why deleting GLES was
worth it anyway, and what it would take to close each gap. The gaps below are unchanged by the
deletion — they were already the Vulkan session's gaps. See `STRATEGY.md` §3.10 for the posture this
sits under.

The short version: **single-device, LINEAR-only, single-plane is a configuration of this renderer,
not its architecture.** Nothing below is foreclosed by deleting GLES.

---

## 1. Multi-planar / non-LINEAR client dmabuf import — **the gap that matters first**

The Vulkan importer accepts only single-plane, LINEAR, 8888 buffers
(`render_helpers/vulkan/renderer.rs`, the import path and the advertised modifier set). The GLES/EGL
importer accepted tiled modifiers and multi-planar formats.

The consequence is **NV12 / P010, i.e. zero-copy hardware video decode**. A video player that would
hand us a decoder-produced NV12 dmabuf must instead fall back to a CPU conversion. Unlike everything
else on this page, **this affects every machine, including the virtio-gpu VM we develop on** — it is
not a bare-metal-only concern. GLES was masking it.

Closing it: multi-planar sampler support (per-plane `VkImage` + `VK_KHR_sampler_ycbcr_conversion`,
or manual plane sampling in the shader) plus `VK_EXT_image_drm_format_modifier` beyond LINEAR. Venus
currently exposes only LINEAR here (see `venus-explicit-sync-gap.md` and the modifier probes), so the
modifier half may stay moot on this VM while the multi-planar half does not.

**This should come before multi-GPU on the roadmap.**

**PROMOTED TO A DATED MILESTONE 2026-07-30.** Gustavo is adding **VA-API and Vulkan Video to the
VMM**, so the "nothing on this machine can produce an NV12 dmabuf" excuse that kept this parked is
expiring. Scheduled in `STRATEGY.md` §6 Phase 3. Split the two halves and start the guest one now:
**multi-planar sampling is ours and is needed regardless** of hardware planes or host modifier
support, while the non-LINEAR modifier half is gated on what KosmicKrisp can expose on Metal.
Second-order consequence worth planning for: video clients will be **dmabuf**, which puts
minified-LINEAR-texture sampling back on the suspect list for GPU frame cost — the current
`present-misses.md` §32 baseline is shm-only and structurally blind to it.

---

## 2. Multi-GPU (render on GPU A, scan out on GPU B) — deferred, and cheap to defer

niri's GLES path supported this through Smithay's `GpuManager<GbmGlesBackend>` / `MultiRenderer`
(`backend/tty.rs`, the non-Vulkan branch of `TtyOutputState::render`). The Vulkan branch returned
before ever reaching it, so **a Vulkan session never had multi-GPU support**; deleting GLES removed
no capability a Vulkan session ever had. What it removed is the ability to fall back to a GLES
session to get it — an escape hatch that stopped existing when `--renderer` did.

### Why this is not a one-way door

The GLES multi-GPU machinery is Smithay's, typed to GLES top to bottom. **None of it transplants to
Vulkan.** Keeping it would buy a shipping fallback for GLES users, not a head start on a Vulkan
implementation — the future work is identical either way. Reference implementations (Smithay's
`multigpu` module; mutter's `doc/multi-gpu.md`, which describes its three copy modes) remain
readable in the read-only checkouts.

### Where single-device is actually baked in

Shallowly. There is no global/static state in the Vulkan renderer; each `Gpu` owns its instance and
device, each `VulkanRenderer` carries its own `ContextId` (which is what Smithay's texture caches key
on), and every cache (`dmabuf_target_cache`, `dmabuf_import_cache`, `present_blit_shadows`) is a
per-instance field. `SynoikRenderer` is a generic blanket impl, not a singleton. **`VulkanRenderer`
could be instantiated once per DRM render node today without a refactor.**

What's missing is that `Gpu::new()` enumerates physical devices and picks the best by device-type
rank, with no notion of which DRM node it is.

### What it would take

The architecture is already "always composite on the primary GPU" (same as mutter), so multi-GPU is
not N renderers — it is two transport problems plus bookkeeping:

1. **Node → device mapping.** `VK_EXT_physical_device_drm` (`VkPhysicalDeviceDrmPropertiesEXT`
   major/minor vs the node's). ~a day. **Do this regardless — see the latent bug in §4.**
2. **Per-CRTC present-blit shadows.** Already has a FIXME in `tty.rs`; needed for multi-output damage
   anyway. ~a day.
3. **The one new engine: a fallback copy stage** for when the scanout GPU cannot import the render
   GPU's buffer — a second minimal `Gpu` on the scanout node doing a single blit (structured like
   Smithay's `finish_internal`), or a CPU copy. ~1–2 weeks. Note the existing shape is already right:
   `DrmCompositor` allocates from the *scanout device's* GBM allocator and the renderer imports each
   buffer via `Bind<Dmabuf>`, which on a two-GPU box performs exactly the cross-device LINEAR import
   (mutter's "zero-copy mode") — it either works or fails loudly at import.
4. **Client-buffer shadow copy** for buffers the render device can't import. Deferrable: with
   LINEAR-only feedback, rejecting at import just makes the client re-allocate.
5. **Cross-device sync: nothing to do** while `finish()` is synchronous (it CPU-waits a fence before
   the flip is queued, so the scanout GPU can never observe an incomplete frame). This becomes real
   work only if submission is pipelined — then export `SYNC_FD` via `VK_KHR_external_fence_fd`
   (already enabled).

Two of our apparent weaknesses — **LINEAR-only formats** and the **fully synchronous `finish()`** —
are precisely the two hardest multi-GPU sub-problems (cross-device modifier negotiation and
cross-device fencing) pre-collapsed to their trivial cases.

### The real cost is validation, not code

Cross-vendor dmabuf import is driver-dependent behavior, and **it cannot be exercised on this machine
at all** (one virtio-gpu device in a VM). Budget an open-ended hardware-dependent debugging tail that
cannot start until we're on a real iGPU+dGPU box.

### Prior art: there is none

- **Smithay has no Vulkan renderer** (issue #134 open since 2019; the only active Vulkan PR is a
  buffer-import draft). Its `multigpu` module is generic over `GraphicsApi`, but `GbmGlesBackend` is
  the only implementation.
- **wlroots** has a Vulkan renderer, but its multi-GPU-critical functions are `// TODO: implement!`
  stubs; working multi-GPU there is GLES-only.
- **Mutter** is GL-only (three copy modes).
- **KWin** shipped its first Vulkan infrastructure in March 2026; its multi-GPU copy-swapchain MR is
  open, explicitly copy-based, and self-described as not yet unlocking the interesting cases.

**No production Wayland compositor does render-on-A/scan-out-on-B with a Vulkan renderer today.** We
would be first, or racing KWin — lifting strategies, not code.

---

## 3. Other things deleted with GLES

- **`wl_drm` legacy EGL clients.** The global was only bound on non-Vulkan sessions, and
  advertising it is an EGL-backend job — with `backend_egl` off there is no EGL at all, so such a
  buffer can never arrive. Dying protocol; small loss.
- **Vulkan-less hardware** (old GPUs, GLES-only ARM blobs). A hard fork targeting a modern base can
  legitimately not care.
- **The GLES test oracle — less than it looks, and it cost nothing.** The per-draw oracle in
  `render_helpers/vulkan/tests.rs` is **Pixman, not GLES**, and survived deletion untouched
  (`renderer_pixman` is kept for exactly this). The conformance corpus needed no renderer at all —
  `RendererKind` is gone and `tests/fixture.rs` never mentions one. The byte-identical GLES A/B check
  was scaffolding for the port; it structurally **cannot** catch the bug class that actually bit us
  repeatedly during Phase C (the neutral-per-target block-out leaks) because it renders both sides
  identically wrong. Pixman plus the structural pins are the durable oracle — and the suite got ~4×
  faster once the GLES machinery was gone (386 tests in ~3.4s).

---

## 4. Latent bug to fix regardless: device selection ignores the DRM node

Dmabuf feedback advertises `primary_render_node.dev_id()` as the main device (`backend/tty.rs`), while
the Vulkan renderer runs on whatever physical device `Gpu::new()`'s enumeration ranked highest. **These
coincide only because this VM has a single GPU.** On any multi-GPU machine we would be telling clients
to allocate for a device we are not rendering on.

Fix: `Gpu::for_drm_node(node)` via `VK_EXT_physical_device_drm`, passing the primary render node at
construction. ~a day, worth doing on its own merits, and it is step 1 of §2 anyway.

---

## 5. CPU readbacks still in the capture path

`render_to_shm` (shm screencopy) and the PipeWire cursor bitmap read `Abgr8888` back and CPU-swizzle
to BGRA, because Vulkan offscreens/readbacks are RGBA-order-only here. The intended fix is to extend
the `Bind<Dmabuf>` B8G8R8A8 + `vkCmdBlitImage` present-blit trick to the offscreen/readback path so
both swizzles disappear and captures stay GPU-side end to end.

---

## 6. Hardware video encode (Vulkan Video) — gated on host + Venus

The screen recorder (the custom-recorder work replacing gnome-shell's gjs recorder) encodes on the
**CPU** (libvpx VP8; VAAPI is not reachable through Venus). Real HW encode means
`VK_KHR_video_encode_queue` + `video_encode_h264`/`h265`(/`av1`) — encoding on the same device we
render on, no CPU readback. The Venus device currently exposes **no `VK_KHR_video_encode_*` /
`video_decode_*` extensions and a single combined `GRAPHICS|COMPUTE|TRANSFER` queue** (no dedicated
encode/decode/transfer queue), so this is blocked in the guest until the VMM/host forward Vulkan
Video.

**Plan (owner-confirmed): Vulkan Video is being added to the VMM we run on; once that lands, implement
a Venus-backed `EncoderBackend`** — encode session + SPS/PPS + rate control + DPB, fed from our
rendered images/dmabufs. It slots behind the recorder's `EncoderBackend` trait with no compositor-side
change (the trait exists precisely as this seam). A VAAPI backend is worth adding for bare-metal GNOME
targets, but that is a separate path (VAAPI ≠ Venus).

---

## 7. GPU-side color conversion (RGBA → NV12/I420) — available on today's Venus

Independent of §1/§6 and **not gated on Vulkan Video**: the capture/encode path converts RGBA→I420 on
the **CPU** (`yuv` crate). Doing the conversion + 4:2:0 subsample in a compute/graphics shader during
the existing GPU readback (§5) removes that per-frame CPU cost **and shrinks the readback ~2.6×** (RGBA
4 B/px → I420 1.5 B/px). It also lands frames in exactly the NV12 layout a future Vulkan-Video encoder
(§6) wants. The compute queue is present today, so this is doable now and compounds with §5 — fold it
into that readback rework.

---

## 8. Hardware cursor plane (KMS) — currently software cursor on the Venus path

We composite the cursor in **software** on the Vulkan/virtio-gpu path because Smithay never sets the
virtio-gpu cursor-plane hotspot (`DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT`), which otherwise produces a
double cursor (see the cursor-plane-hotspot note / Smithay fork patches). Using the KMS cursor plane
offloads cursor compositing entirely. Blocked on the Smithay patch; a real HW offload we are not using.

---

## 9. Direct scanout / overlay planes — verify it engages on virtio-gpu

Zero-copy scanout of fullscreen/unoccluded client buffers straight to a KMS plane skips GPU
compositing. The path exists (there is a `disable_direct_scanout` debug flag), but virtio-gpu plane
support varies — **verify it actually engages on Venus and measure**. Efficiency, not a missing
capability.

### Not a hardware-acceleration target: JPEG-XL decode

The 4K JPEG-XL wallpaper decode is a CPU hotspot (seconds; see the wallpaper-decode-slow note), but it
is **not** GPU-accelerable in any practical way: no HW JXL decode block exists anywhere (JXL is covered
by neither VA-API nor `VK_KHR_video_decode_*`), libjxl is CPU-SIMD only, and JXL's entropy stage
(ANS/prefix coding) is inherently *sequential* — the GPU-hostile part, and often the bulk of the cost.
A hybrid (CPU entropy → GPU compute for iDCT / XYB→RGB / upsample / filters) would need deep libjxl
surgery and still leave the entropy stage on the CPU. **Fix it algorithmically instead: decode at/near
target resolution (JXL is progressive — the 1:8 DC image gives an instant preview) + a variant cache.**
Keep JXL decode on the CPU; make it decode *less*.

---

## 10. Implicit-modifier KMS — CLOSED 2026-08-05, by not using gbm

Upstream virtio-gpu used to set `mode_config.fb_modifiers_not_supported` (`virtgpu_display.c`), which
meant three things at once: `DRM_CAP_ADDFB2_MODIFIERS` read 0, no plane carried an `IN_FORMATS` blob,
and `drmModeAddFB2` rejected `DRM_MODE_FB_MODIFIERS`. Smithay then synthesized `Modifier::Invalid`
for every plane format, leaving nothing explicit for `DrmCompositor` to agree on, and
`VK_EXT_image_drm_format_modifier` has no encoding for INVALID. A whole apparatus existed to bridge
that: `backend/scanout_allocator.rs` recovered the modifier gbm implicitly chose,
`owned_vulkan_scanout_formats()` added INVALID entries to the negotiation, and
`SYNOIK_KMS_IMPLICIT_MODIFIERS=1` forced the path on so it could be tested.

**All of it is gone.** The guest kernel has advertised `IN_FORMATS` with `DRM_FORMAT_MOD_LINEAR`
since `7.1.6-2.limina16k`, so negotiation never lands on INVALID; and scanout buffers are no longer
allocated by gbm at all. See `docs/fork/scanout-allocation.md` for what replaced it and why the
replacement is about far more than modifiers — the gbm dependency was a latent break tied to a *GL*
driver-selection env var, and it fired.

What survives from this section is the rule, which is unchanged and now enforced in one place: a
dmabuf with modifier INVALID is never handed to KMS and never imported. Weston's reasoning — an
unknown layout displays garbage — is why, and `PrimeFramebufferExporter` refuses it outright rather
than falling back to a modifier-less `AddFB2`.

**One half is still open, and it is not ours to close.** A *client's* buffers are allocated by the
client, so a GL client on vrend hands us classic virgl resources that venus refuses to import
(`Tty::import_dmabuf` correctly declines them; Firefox and Epiphany then hang rather than falling
back). Mutter does not hit this because its renderer is GL — the same driver that allocated the
buffer. A Vulkan compositor is cross-driver by construction, and the fix is host-side: vkr importing
virgl resources into venus. Until that is confirmed deployed, `/etc/environment.d/90-limina-zink.conf`
must stay — it is no longer protecting our scanout, only client imports. Detail in
`docs/fork/scanout-allocation.md`.

---

## 11. Surviving device loss — we die instead, and on venus we cannot even see it

**Opened 2026-08-10 by a real crash on kov's seat.** The session suspended (s2idle) at 17:41:22 and
resumed at 17:44:42; one second after resume the guest kernel began rejecting virtgpu traffic
(`[drm:virtio_gpu_dequeue_ctrl_func] *ERROR* response 0x1200 (command 0x207)` —
`RESP_ERR_UNSPEC` to `SUBMIT_3D`, later `0x1203 RESP_ERR_INVALID_RESOURCE_ID` on teardown commands:
the host's resource table had lost every pre-suspend entry). Sixteen seconds later synoik took
`SIGABRT` inside mesa, on the first `vkCreateImage` after resume — baking the lock-screen prompt text:

```
Synoik::render → render_inner → LockScreen::render_prompt → widget::bake_uncached_sized
  → VulkanRenderer::create_buffer → ash create_image
  → vn_CreateImage → vn_image_init → vn_ring_submit_command → vn_ring_wait_seqno → vn_relax → abort()
```

**The suspend half is not ours to fix** (the VMM must preserve or re-create the venus context across
guest suspend), and it predates us: on 2026-07-20 the same resume killed `vkmark` while *gnome-shell*
was still the seat compositor, and wedged gnome-shell badly enough that systemd had to abort it.
What *is* ours is that a compositor should not be a casualty of a recoverable GPU event.

### Why venus aborts, and why `VN_DEBUG=no_abort` is not the answer

Read from the mesa checkout (`~/Projects/mesa`, `53a453b3ef7`), `src/virtio/vulkan/vn_common.c:245`.
`vn_relax()` has three abort paths: the ring's `VK_RING_STATUS_FATAL_BIT_MESA` (**not** guarded by
`no_abort`), an expired watchdog "alive" bit, and a plain iteration ceiling (~895 s for the ring-seqno
profile). The stated rationale in the commit that added it (`bbbbf395594`, Feb 2022) is only "more
friendly than printing the messages forever" — but the structural reason is stronger: **there is no
legal way to report the failure.** `vn_ring_wait_seqno` (`vn_ring.c:182`) returns `void` and loops
`do { … } while (true)`, and `vkCreateImage` may return only the OOM codes per spec — never
`VK_ERROR_DEVICE_LOST`. Venus *does* report device loss where the spec permits it (`vn_sync.c`,
`vn_query_pool.c` check `vn_relax_warn()` and return `DEVICE_LOST`); it aborts only where it can't.

So `VN_DEBUG=no_abort` does **not** hand us a `VkResult` on this path — it removes the abort from a
loop with no exit, i.e. it converts a crash into a permanent hang. For a daily driver that is worse.
`VN_DEBUG` is undocumented besides: it appears in `vn_common.h`'s `enum vn_debug` and the option table
in `vn_common.c`, and nowhere in `docs/envvars.rst`.

Related: the warnings that precede an abort are invisible by default — `vn_log` logs at
`MESA_LOG_DEBUG` and release mesa defaults to `MESA_LOG_INFO` (`src/util/log.h:49`). Setting
`MESA_LOG_LEVEL=debug` on the seat is a cheap, standing win: the next occurrence will name *which*
abort fired ("aborting on ring fatal error" / "on expired ring alive status" / plain "aborting").
Worth doing before anything else here, since the 16-second death does not match the ~895 s ceiling
and so was the host actively signalling death, not a timeout.

### What closing this looks like

Mutter is building the same capability — see
<https://blogs.gnome.org/anonymoux47/2026/07/02/gpu-reset-recovery-in-mutter-a-progress-update/>
(on the author's fork, not upstream as of 2026-07). The shape is worth copying:

- A **five-state machine**: Normal → Reset In Progress (poll ~20 ms) → Reset Completed (or a 2 s
  timeout) → Restoring → Normal, with a Failed arm that exits cleanly.
- The rebuild runs **outside the frame dispatch loop**, then unrealizes and re-realizes the scene.
- **Clients are not recoverable by the compositor** — they must re-render their own buffers, so even
  a complete implementation means a visibly broken frame or two, not a seamless save.

Their *detection* story does not transfer: it is GL/EGL, hinging on `EXT_robustness` /
`EXT_create_context_robustness` with `EGL_LOSE_CONTEXT_ON_RESET`. For us `VK_ERROR_DEVICE_LOST` is
core Vulkan — every submit and acquire already returns it, no extension, no polling. Our problem is
narrower and lower: **on venus the process is killed before any `VkResult` reaches us**, so step 0 is
making device loss observable at all. That means upstreaming a venus change (a bail-out from the
seqno wait, and ring-fatal → `DEVICE_LOST` where the spec allows), or a watchdog of our own — not an
env var.

Their hardest bug will be ours too, and worse: the **glyph cache** — actors holding `PangoLayout`s
bound to a dead renderer, needing an explicit forget/unrealize before recreation. Our equivalent is
every baked texture and every cached element keyed by `Id`, spread across the widget layer rather
than owned by one renderer object. A device loss must invalidate **every bake key**, not just the
device handle. Expect that, not the state machine, to be the work.

**Sequencing note:** this is insurance against GPU resets generally; it is *not* the fix for the
suspend crash above, which stays host-side. Do not let it be scheduled as one.

---

## Roadmap order (recommended)

1. **Multi-planar / non-LINEAR import** (§1) — affects every machine, including the VM.
2. **DRM-node-aware device selection** (§4) — a day, fixes a real latent bug.
3. **GPU-side capture readbacks** (§5) — fold in **GPU-side RGBA→NV12 color conversion** (§7) while there.
4. **HW video encode via Vulkan Video** (§6) — when the VMM/host forward it (Vulkan Video is being added to our VMM); a Venus-backed `EncoderBackend`.
5. **Multi-GPU** (§2) — only when there is bare-metal multi-GPU hardware to validate on.

Independent and slottable anytime: **HW cursor plane** (§8, once the Smithay patch lands) and
**direct-scanout verification** (§9).

**Device-loss survival** (§11) is sequenced by its own step 0: set `MESA_LOG_LEVEL=debug` on the seat
now (free), and treat the recovery machine as gated on device loss becoming *observable* — a venus
change, upstream or ours. Until then it is unimplementable, not merely deferred.
