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
per-instance field. `NiriRenderer` is a generic blanket impl, not a singleton. **`VulkanRenderer`
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

## Roadmap order (recommended)

1. **Multi-planar / non-LINEAR import** (§1) — affects every machine, including the VM.
2. **DRM-node-aware device selection** (§4) — a day, fixes a real latent bug.
3. **GPU-side capture readbacks** (§5) — fold in **GPU-side RGBA→NV12 color conversion** (§7) while there.
4. **HW video encode via Vulkan Video** (§6) — when the VMM/host forward it (Vulkan Video is being added to our VMM); a Venus-backed `EncoderBackend`.
5. **Multi-GPU** (§2) — only when there is bare-metal multi-GPU hardware to validate on.

Independent and slottable anytime: **HW cursor plane** (§8, once the Smithay patch lands) and
**direct-scanout verification** (§9).
