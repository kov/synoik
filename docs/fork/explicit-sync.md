# Client-buffer producer sync (implicit + explicit)

**Status:** implicit sync always on; explicit sync (`linux-drm-syncobj-v1`) wired 2026-07-11.
Renderer-agnostic — applies to both the GLES oracle and the owned Vulkan renderer.

## The model (how a client buffer is synchronized)

A client's buffer must not be *sampled* before its producing GPU work finishes (acquire), and
must not be *released* back to the client while the compositor is still reading it (release).
niri handles both **at the protocol / commit layer**, not in the renderer — the same design
mutter and smithay's anvil use.

### Acquire — a commit-time blocker (renderer-agnostic)

Every surface that can attach a dmabuf gets a pre-commit hook
(`State::add_default_dmabuf_pre_commit_hook` in `handlers/compositor.rs`, and the
mapped-toplevel hook `add_mapped_toplevel_pre_commit_hook` in `handlers/xdg_shell.rs`). On a
commit that attaches a new dmabuf the hook installs a **blocker** that holds the surface's
commit transaction until the buffer is producer-complete:

- **Explicit sync:** if the client set a `linux-drm-syncobj-v1` acquire timeline point this
  commit (`DrmSyncobjCachedState::pending().acquire_point`), the hook builds a
  `DrmSyncPoint::generate_blocker()` (a `drmSyncobjEventfd` source) and blocks on it.
- **Implicit sync (fallback):** otherwise it builds `Dmabuf::generate_blocker(Interest::READ)`,
  which `poll(2)`s the dmabuf plane fds for `POLLIN` — i.e. waits on the buffer's most recent
  write/exclusive fence.

Because the blocker gates the *commit*, the buffer is never made "current" (never imported or
sampled by any renderer) until the producing fence has signalled. The renderer therefore does
**no** producer wait of its own — its dmabuf-import barrier is an ownership acquire (FOREIGN
queue family → ours), not a readiness wait. See the note in
`render_helpers/vulkan/renderer.rs::import_dmabuf_as_texture`.

### Release — signalled when the buffer's last reference drops

niri never sends `wl_buffer.release` explicitly. smithay signals it (and, for explicit-sync
clients, the release timeline point) from `InnerBuffer::drop`
(`smithay backend/renderer/utils/wayland.rs`) when the last `Arc` reference to the buffer is
dropped. So the explicit-sync release point signals from the *exact same site*, at the *exact
same time*, as `wl_buffer.release` — it inherits niri's existing buffer-release discipline
verbatim, for **both** renderers. Because implicit-sync clients ship tear-free today on that
same discipline (GLES daily-drives it), explicit sync is correct by construction.

The load-bearing invariant that discipline rests on: **niri never emits a frame callback (nor
lets a buffer be released/replaced) before the frame that sampled it has been presented.** Two
mechanisms enforce it:

- **The read is drained before the frame is presented.** On Vulkan, `VulkanFrame::finish()`
  does a synchronous `wait_for_fences(u64::MAX)`, so the GPU read is complete before the frame
  is even queued. On GLES, `GlesFrame::finish()` only flushes (it does *not* block), so the read
  completes asynchronously — but the render's `SyncPoint` is handed to the `DrmCompositor` as the
  page-flip **KMS in-fence**, so the flip (and thus presentation) does not happen until the GPU
  read has finished.
- **The buffer stays referenced until presentation, and the client isn't told to reuse it until
  then.** niri sends frame callbacks only from the vblank / estimated-vblank path (`Tty::on_vblank`
  / `on_estimated_vblank_timer`), i.e. *after* `frame_submitted()` — never right after
  `queue_frame`. A well-behaved client replaces its buffer (dropping the previous `Buffer` `Arc`
  → `wl_buffer.release` + release-point signal) only after that callback, by which point the read
  is done. Directly-scanned-out buffers (primary-plane scanout) are additionally held by the
  `DrmCompositor` in its `current_frame` and dropped in `frame_submitted()` only after the next
  flip retires them.

**Do not move frame-callback emission before the page flip** (e.g. to right after `queue_frame`)
without re-establishing release-after-read another way — on GLES that would signal a client's
release point while our sampling of the buffer is still in flight, and unlike an implicit-sync
client an explicit-sync client does not additionally wait on the buffer's implicit read fence, so
it would overwrite a buffer we're still reading.

## Enabling explicit sync

`Tty::device_added` creates `DrmSyncobjState` from the primary GPU's `DrmDeviceFd` once the
primary renderer is up, gated on `supports_syncobj_eventfd`. It is `None` on winit/headless
(no DRM device) — clients there transparently use implicit sync. Handler + delegate live in
`handlers/mod.rs` (`DrmSyncobjHandler` / `delegate_drm_syncobj!`).

**No renderer change was needed.** Acquire is a commit blocker; release is smithay's
CPU-side signal on buffer drop. The renderer produces no completion fence.

## Venus trust and the future-work trigger

The commit blocker is a *sound* producer wait on this Venus VM: `synoik-vk/src/sync_spike.rs`
measured a `virtio_gpu` dma_fence as unsignalled-at-export with a downstream wait blocking the
full GPU busy-work duration (no early signal), and GLES daily-drives tear-free on the identical
blocker. Client-fence trust is inferred from mechanism identity (same host-fence propagation),
not directly measured — a busy-GPU client-side poll spike would convert inferred → measured.

The `VkFence`→`SYNC_FD`→`drm_syncobj` bridge the sync spike de-risked becomes load-bearing only
if niri later **drops the synchronous per-frame `finish()` to pipeline present** (async). At
that point acquire would move to a wait-semaphore on the compositing submit and release would
signal an exported `VkFence` (created exportable *before* submit, exported while still pending —
one coupled decision with dropping the CPU-wait). mutter's model already imports a real
GPU-completion fence for release (`cogl_renderer_get_latest_sync_fd`); niri-on-smithay does not
need it until it goes async.
