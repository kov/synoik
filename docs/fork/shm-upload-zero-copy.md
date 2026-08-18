# Avoiding the CPU copy on an shm upload

Every `wl_shm` buffer a client commits costs us one **guest-CPU memcpy** before the GPU ever sees
it. This doc records what that copy is, who owns the memory on each side, and the three routes that
could remove it. **None is actionable today** — each is blocked on something outside this repo. It
exists so the next person does not re-derive the blockers.

The staging path itself is in [`foundation.md`](./foundation.md); the submit rules the upload obeys
are in [`frame-submit-discipline.md`](./frame-submit-discipline.md).

## The copy, and who owns each end

| | owner | what it is |
|---|---|---|
| source | **the client** | a `memfd` passed as `wl_shm.create_pool(fd, len)`, which smithay `mmap`s; the pointer is valid only inside `with_buffer_contents` |
| destination | **us** | a `HOST_VISIBLE \| HOST_COHERENT` `TRANSFER_SRC` buffer from `StagingPool` — on Venus, a virtio-gpu blob |

The GPU then does `vkCmdCopyBufferToImage` from the staging buffer, recorded into the frame's own
command buffer. That half is nearly free and is not what this doc is about.

The memcpy is guest-CPU, between two regions with **different owners**, and either end can be cold.
Pooling (`783ff7b2`) warms our end and only our end; the client has just finished writing its pool
pages, so the source is cold no matter what we do. Measured 2026-08-18 after the pool landed:
**10.4 GB/s average, 13.9 GB/s best**, against ~56 GB/s for a fully warm same-process memcpy on this
VM. Treat ~14 GB/s as the practical ceiling for pool→mapping here.

Because the source is client memory that may be rewritten the moment `with_buffer_contents` returns,
**a copy of some kind is mandatory**. Every route below turns the *guest-CPU* copy into a copy done
by someone else, none removes copying altogether.

## What it costs, so the payoff stays bounded

The dominant real-world case is a client whose toplevel is shm. Firefox is one: its web content is
dmabuf, but its CSD chrome surface is a full-window `wl_shm` buffer repainted on every
`wl_keyboard.enter`/`leave` — so **one whole-window upload per focus change**, and a workspace
switch is a focus change. On kov's seat that is 37.8 MiB, i.e. ~3.8 ms at the measured 10.4 GB/s.

That is the size of the prize: a few milliseconds on frames where focus changes, not a per-frame
stream. It is not nothing — it is most of a 16.67 ms budget — but it is bounded and it only fires on
those frames. Weigh any of the routes below against that, not against a steady-state cost.

## Route 1 — `VK_EXT_external_memory_host`

Import the already-`mmap`ed client pointer directly as `VkDeviceMemory`, bind a `TRANSFER_SRC`
buffer to it, and let `vkCmdCopyBufferToImage` read the client's pages. The guest CPU touches
nothing.

**Blocked: Venus does not expose the extension.** The virtio ICD here advertises only
`VK_KHR_external_memory_fd` and `VK_EXT_external_memory_dma_buf`.

Even with the extension it carries real constraints, worth knowing before anyone lobbies for it:
- the host pointer must be page-aligned and its size a multiple of `minImportedHostPointerAlignment`;
  a `wl_shm` buffer at a non-zero pool offset is not aligned by construction
- the memory type is dictated by `VkMemoryHostPointerPropertiesEXT`, not chosen by us
- we must hold `wl_buffer.release` until the GPU copy retires — the frame already defers this, so
  no new machinery, but the client stalls longer than it does today

## Route 2 — udmabuf + `VK_EXT_external_memory_dma_buf`

Wrap the client's `memfd` in a dma-buf via `/dev/udmabuf`, then import *that* with the extension
Venus **does** advertise. `/dev/udmabuf` exists on this VM and an ACL grants the seat user `rw`, so
the guest-side half is reachable today.

**Blocked: the VMM does not support importing a guest dma-buf into a Venus resource.** Guest-side
reachability is not the constraint; the import path on the other side of virtio-gpu is.

Its own constraints:
- udmabuf requires the memfd to carry `F_SEAL_SHRINK`. Not all clients seal, so this can never be
  the only path — the memcpy stays as the fallback, and we would be maintaining both
- it is per-pool setup, so it must be cached against the pool's lifetime, not done per buffer
- mutter does not do this. There is no upstream implementation to inherit failure modes from

## Route 3 — move the copy host-side

The most promising direction, and the one that fits the architecture rather than fighting it.

The client's pool is **guest** memory the VMM can already address. Our staging buffer is a virtio-gpu
blob backed by **host** memory. So the copy crosses the guest/host boundary in a place the VMM is
already standing — and a guest-CPU memcpy into a host blob is the expensive way to express something
virtio-gpu has a verb for. A guest-backed resource plus a transfer-to-host is the classic shape.

This is VMM work, not compositor work. What would have to be true:
- a way to name a guest memory range (the client's pool, at an offset) as the source of a transfer
  into a Venus-owned resource
- the transfer must be orderable against the frame's submit so it can be *recorded* rather than
  waited on — otherwise it buys a round trip and we have traded a memcpy for a stall, which
  `frame-submit-discipline.md` exists to prevent
- a fallback when the client's memory is not in a form the VMM can map

## Decision

**Not now.** The payoff is bounded (above), routes 1 and 2 are blocked outside this repo, and route 3
is a VMM feature we do not control. The staging-pool fix already took this from a per-frame
allocation storm to a bounded per-focus-change copy.

Revisit if any of these becomes true:
- the VMM gains guest-dma-buf import, which unblocks route 2
- virtio-gpu grows a guest-range→resource transfer that can be recorded, which is route 3
- a client appears whose shm toplevel repaints **per frame** rather than per focus change, which
  would move this from a few milliseconds on some frames to a permanent tax

## A cheaper mitigation that is not zero-copy

The upload lands on frames tagged `animating workspace-switch` — we send `wl_keyboard.leave` when
the workspace starts leaving, so the client's full-window repaint arrives *during* the animation,
which is the worst moment for it.

Measured 2026-08-18: exactly one `leave` and one `enter` per switch, one `wl_shm.create_pool` per
focus transition. **We are not flip-flopping focus** — the cost is inherent to focus changing at
all, not to churn.

So a candidate that costs nothing on the GPU side: **move the focus transition to the end of the
switch animation** rather than its start, so the repaint lands on a settled frame. Before anyone
builds it, check what mutter actually does (`js/ui/`, `src/core/`) — per the fork tenet the
reference decides, not what reads well here — and weigh that input routing during the animation
would go to the old window for the animation's duration.

## Traps for whoever picks this up

- **An open render node, or a dmabuf content surface, is not evidence that a client's toplevel is
  dmabuf.** Firefox is two surfaces and only one of them is. Read the `wl_shm.create_pool` calls in a
  `WAYLAND_DEBUG=1` trace, not the process's open fds.
- **A window that is occluded, unfocused, or on an unshown workspace does not render**, so it uploads
  nothing. Absence of an upload never distinguishes "on dmabuf" from "not drawing". Confirm the
  window actually committed before concluding anything from a zero.
