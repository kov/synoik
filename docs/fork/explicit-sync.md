# Client-buffer producer sync (implicit + explicit)

**Status:** implicit sync always on; explicit sync (`linux-drm-syncobj-v1`) wired 2026-07-11 and
unchanged since. Both are handled at the protocol / commit layer, not in the renderer — the same
design mutter and smithay's anvil use.

## The model

A client's buffer must not be *sampled* before its producing GPU work finishes (acquire), and must
not be *released* back to the client while we are still reading it (release).

### Acquire — a commit-time blocker

Every surface that can attach a dmabuf gets a pre-commit hook
(`State::add_default_dmabuf_pre_commit_hook` in `handlers/compositor.rs`, and the mapped-toplevel
hook `add_mapped_toplevel_pre_commit_hook` in `handlers/xdg_shell.rs`). On a commit that attaches a
new dmabuf the hook installs a **blocker** that holds the surface's commit transaction until the
buffer is producer-complete:

- **Explicit sync:** if the client set a `linux-drm-syncobj-v1` acquire point this commit
  (`DrmSyncobjCachedState::pending().acquire_point`), the hook builds a
  `DrmSyncPoint::generate_blocker()` (a `drmSyncobjEventfd` source) and blocks on it.
- **Implicit sync (fallback):** otherwise `Dmabuf::generate_blocker(Interest::READ)`, which
  `poll(2)`s the dmabuf plane fds for `POLLIN` — i.e. waits on the buffer's most recent write fence.

Because the blocker gates the *commit*, the buffer is never made current — never imported, never
sampled — until the producing fence has signalled. **The renderer therefore does no producer wait of
its own**; its dmabuf-import barrier is an ownership acquire (FOREIGN queue family → ours), not a
readiness wait. See `render_helpers/vulkan/renderer.rs::import_dmabuf_as_texture`.

### Release — signalled when the buffer's last reference drops

We never send `wl_buffer.release` explicitly. Smithay signals it — and, for explicit-sync clients,
the release timeline point — from `InnerBuffer::drop` when the last `Arc` reference goes. So the
explicit-sync release point signals from the *exact same site*, at the *exact same time*, as
`wl_buffer.release`, inheriting the existing buffer-release discipline verbatim.

**The load-bearing invariant that discipline rests on:** no frame callback is emitted (and no buffer
released or replaced) before the frame that sampled it has been presented. Two mechanisms enforce it:

- **The buffer stays referenced until presentation.** Frame callbacks are sent only from the vblank
  path — `Tty::on_vblank` and `on_estimated_vblank_timer`, both *after* `frame_submitted()`, never
  right after `queue_frame`. A well-behaved client replaces its buffer only after that callback.
  Directly-scanned-out buffers are additionally held by `DrmCompositor` in its `current_frame` and
  dropped in `frame_submitted()` only once the next flip retires them.
- **The read is complete before presentation.** On the synchronous path `VulkanFrame::finish()`
  waits the fence, so the GPU read is done before the frame is even queued. With
  `SYNOIK_VK_ASYNC_SCANOUT=1` the fence is *not* waited: it is exported and handed to the atomic
  commit as `IN_FENCE_FD`, so the flip — and therefore presentation, and therefore the callback —
  cannot happen until the read has finished. The ordering property is the same either way; only who
  enforces it moves.

**Do not move frame-callback emission before the page flip** (e.g. to right after `queue_frame`)
without re-establishing release-after-read another way. Under async scanout that would signal a
client's release point while our sampling is still in flight, and unlike an implicit-sync client an
explicit-sync client does not additionally wait on the buffer's implicit read fence — it would
overwrite a buffer we are still reading.

## Enabling explicit sync

`Tty::device_added` creates `DrmSyncobjState` from the primary GPU's `DrmDeviceFd` once the primary
renderer is up, gated on `supports_syncobj_eventfd`. It is `None` on winit/headless (no DRM device);
clients there transparently use implicit sync. Handler and delegate live in `handlers/mod.rs`.

**No renderer change was needed for the protocol.** Acquire is a commit blocker; release is
smithay's CPU-side signal on buffer drop.

## What the stack actually provides

Measured, not inferred from the API surface: this stack exposes **only `SYNC_FD` *binary* external
semaphores**. `OPAQUE_FD` and *timeline* external semaphores are absent — and absent identically on
lavapipe, so this is the normal state of these drivers, not a virtio regression. **Do not chase
"make `OPAQUE_FD` cross virtio"**; it is an architectural dead end and is not needed. The portable
path (kernel `drm_syncobj` timeline ⟷ `sync_file` ⟷ binary `SYNC_FD` VkSemaphore) uses only what is
present, and the guest DRM node does advertise `drm_syncobj` timeline support.

The practical consequence for anything new: **one fence object per handoff**, since there is no
exported timeline to advance.

`synoik-vk/src/sync_spike.rs` (`explicit_sync_bridge`) is the harness that established this, and it
doubles as a VMM health probe — it measured a `virtio_gpu` dma_fence as unsignalled-at-export with a
downstream wait blocking the full GPU busy-work duration, i.e. no early signal. Client-fence trust is
inferred from mechanism identity (same host-fence propagation), not directly measured; a busy-GPU
client-side poll spike would convert inferred → measured.

The `VkFence`→`SYNC_FD`→`drm_syncobj` bridge that spike de-risked is **now load-bearing**: it is what
`exported_scanout_fences` uses on the async scanout path. It was written before there was a caller.
