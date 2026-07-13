# Owned Vulkan renderer — known gaps and deferred work

Status: 2026-07-13, at the end of Phase C (the owned Vulkan renderer is the renderer for live
sessions; the co-resident GLES renderer is being deleted).

This records what the owned renderer *cannot* do that the GLES renderer could, why we are deleting
GLES anyway, and what it would take to close each gap. See `STRATEGY.md` §3.10 for the posture this
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

---

## 2. Multi-GPU (render on GPU A, scan out on GPU B) — deferred, and cheap to defer

niri's GLES path supports this through Smithay's `GpuManager<GbmGlesBackend>` / `MultiRenderer`
(`backend/tty.rs`, the non-Vulkan branch of `TtyOutputState::render`). The Vulkan branch returns
before ever reaching it, so **a `--renderer=vulkan` session has already had no multi-GPU support**;
deleting GLES does not remove a capability Vulkan sessions ever had. What it removes is the ability
to fall back to `--renderer=gles` to get it.

### Why this is not a one-way door

The GLES multi-GPU machinery is Smithay's, typed to GLES top to bottom. **None of it transplants to
Vulkan.** Keeping it would buy a shipping fallback for GLES users, not a head start on a Vulkan
implementation — the future work is identical either way. Reference implementations (Smithay's
`multigpu` module; mutter's `doc/multi-gpu.md`, which describes its three copy modes) remain
readable in the read-only checkouts.

### Where single-device is actually baked in

Shallowly. There is no global/static state in the `vulkan` feature; each `Gpu` owns its instance and
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

- **`wl_drm` legacy EGL clients.** The global is only bound on non-Vulkan sessions. Dying protocol;
  small loss.
- **Vulkan-less hardware** (old GPUs, GLES-only ARM blobs). A hard fork targeting a modern base can
  legitimately not care.
- **The GLES test oracle — less than it looks.** The per-draw oracle in `render_helpers/vulkan/tests.rs`
  is **Pixman, not GLES**, and survives deletion untouched. What GLES deletion forces is flipping the
  headless conformance corpus (`tests/fixture.rs` defaults to `RendererKind::Gles`) over to
  Vulkan-on-lavapipe. The byte-identical GLES A/B check was scaffolding for the port; it structurally
  **cannot** catch the bug class that actually bit us repeatedly during Phase C (the
  neutral-per-target block-out leaks) because it renders both sides identically wrong. Pixman plus the
  structural pins are the durable oracle.

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

## Roadmap order (recommended)

1. **Multi-planar / non-LINEAR import** (§1) — affects every machine, including the VM.
2. **DRM-node-aware device selection** (§4) — a day, fixes a real latent bug.
3. **GPU-side capture readbacks** (§5).
4. **Multi-GPU** (§2) — only when there is bare-metal multi-GPU hardware to validate on.
