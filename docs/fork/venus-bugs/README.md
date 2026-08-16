# Venus / virtio-gpu dmabuf-import findings — bug reports + reproducers

Two independent findings from bringing up a native-Vulkan (ash) renderer on **Mesa `venus`**
(virtio-gpu) in an Apple-Silicon-hosted VM. Each has a **self-contained reproducer crate** in
this directory (detached from any parent workspace — `cargo run` inside each just works). They
are in **different components**, so they're written up separately:

| # | Component | Severity | One line |
|---|---|---|---|
| **1** | Mesa `venus` (Vulkan) | **bug** | `vkGetMemoryFdPropertiesKHR` rejects a dmabuf that `vkAllocateMemory` then imports fine |
| **2** | Mesa gbm / virtio-gpu (Gallium) | **question** | `GBM_BO_USE_WRITE` (and the legacy non-modifier alloc paths) fail; is that expected? |

Both reproduce **deterministically** (verified across repeated runs) and each was
adversarially checked to rule out reproducer-side confounds (see the per-issue "Confounds
ruled out" notes).

**A third, later finding, now fixed:** timestamp queries were advertised in full and resolved to
zero — two stacked bugs in the host Vulkan driver, fixed host-side 2026-07-26. Its reproducer,
[`repro-vk-timestamp-query/`](./repro-vk-timestamp-query), **stays**: it is the discriminator, it is
cheap, and it has reported three different answers on three builds. See
[`foundation.md`](../foundation.md) §4.

**For the performance picture rather than individual defects**, see
[`foundation.md`](../foundation.md) §5: what this stack prices per round trip, measured on a live
compositor, and what would help most at the VMM level.

---

## Environment

| | |
|---|---|
| Vulkan driver | `driverID = DRIVER_ID_MESA_VENUS`, `driverName = venus`, `driverInfo = Mesa 26.1.3` |
| Vulkan device | `Virtio-GPU Venus (Apple M4 Pro)`, `INTEGRATED_GPU`, API `1.3.353` |
| libgbm | `26.1.3` |
| Guest kernel | `Linux 7.1.2-limina16k aarch64` |
| virtio-gpu | `virtio_gpu 0.1.0` over `virtio_mmio` (`a008000.virtio_mmio`), DRM features `+context_init` |
| DRM nodes | `card0`, `renderD128` (render node is world-rw) |
| VMM / host | `systemd-detect-virt = vm-other`; host GPU = Apple M4 Pro |
| Reproducer crates | Rust, `gbm = 0.18.0`, `ash = 0.38.0` |

All allocation is on `/dev/dri/renderD128`.

---

## Issue 1 — `vkGetMemoryFdPropertiesKHR` rejects a handle it can import  *(Mesa `venus`, bug)*

**Reproducer:** [`repro-vk-getmemfdprops/`](./repro-vk-getmemfdprops) — `cargo run` (Venus must be
the selected ICD).

For **the exact same** LINEAR `ARGB8888` dmabuf FD exported by gbm, `vkGetMemoryFdPropertiesKHR`
returns `VK_ERROR_INVALID_EXTERNAL_HANDLE` with `memoryTypeBits = 0x0`, yet the immediately
following `vkAllocateMemory` (with `VkImportMemoryFdInfoKHR`, `DMA_BUF_EXT`) and
`vkBindImageMemory` both **succeed**:

```
gbm: LINEAR ARGB8888 64x64, stride=256 offset=0 fd=7
vk: device="Virtio-GPU Venus (Apple M4 Pro)" driver="venus" queue_family=0
QUERY  vkGetMemoryFdPropertiesKHR -> Err(ERROR_INVALID_EXTERNAL_HANDLE)
IMPORT vkAllocateMemory(ImportMemoryFdInfoKHR) -> Ok("Ok")
BIND   vkBindImageMemory -> Ok(())
CONCLUSION: query=INVALID_EXTERNAL_HANDLE but import=SUCCESS → inconsistent
```

**Why this is a bug.** `vkGetMemoryFdPropertiesKHR` is defined to report the set of memory types
into which a given external handle *can* be imported. Here it declares a handle invalid
(`memoryTypeBits = 0x0`) that the driver then imports and binds without complaint — a
self-contradiction. The call is spec-legal: `DMA_BUF_EXT` is an allowed `handleType` for the
query (only `OPAQUE_FD` is forbidden), and both `VK_KHR_external_memory_fd` +
`VK_EXT_external_memory_dma_buf` are enabled.

**Concrete harm.** The image's own `memory_type_bits` is `0x1`; the query returns `0x0`. A
renderer that follows the usual pattern of masking the image requirements by the query result
(`image_bits & fd_props_bits`) is left with **zero** valid memory types and wrongly refuses an
importable buffer. Our importer had to treat the query as best-effort and fall back to the
image's `memory_type_bits`; that workaround shouldn't be necessary.

**Confounds ruled out (adversarial check):**
- **Real GPU, not llvmpipe** — loader trace loads only the virtio ICD; exactly one physical
  device enumerated: `Virtio-GPU Venus`, `INTEGRATED_GPU`.
- **Not both-fail / both-succeed** — deterministic over repeated runs: query `Err`, import+bind
  `Ok`.
- **Same kernel object** — the imported FD is a `dup` (fd 9) of the queried FD (fd 7); `fstat`
  shows identical `st_dev`/`st_ino`, so it is literally the same dmabuf.
- **Not an ordering artifact** — the query fails identically *before and after* the successful
  import.
- **Valid handle type** — `DMA_BUF_EXT` used in both query and import; the import proves it's the
  correct type. The query takes no image/format/layout input, so no image-side misconfiguration
  can explain its failure.

Relevant memory type on this device: a single type,
`DEVICE_LOCAL | HOST_VISIBLE | HOST_COHERENT | HOST_CACHED`; image `mem_req`: `size = 16384`
(= 64·64·4), `alignment = 16`, `memory_type_bits = 0x1`.

---

## Issue 2 — gbm allocation flag/format acceptance on virtio-gpu  *(Mesa gbm / Gallium, question)*

**Reproducer:** [`repro-gbm-alloc/`](./repro-gbm-alloc) — `cargo run` (gbm only, no Vulkan).

Only `create_buffer_object_with_modifiers2([LINEAR], empty_flags)` succeeds. Adding
`GBM_BO_USE_WRITE` to that otherwise-identical call flips it to `EINVAL`, and the legacy
non-modifier `gbm_bo_create` paths fail with `ENOENT`:

```
ERR   create_buffer_object ARGB8888 RENDERING|LINEAR  -> ... errno=Some(2)   # ENOENT
ERR   create_buffer_object ARGB8888 LINEAR            -> ... errno=Some(2)   # ENOENT
ERR   create_buffer_object ARGB8888 LINEAR|WRITE      -> ... errno=Some(22)  # EINVAL
OK    modifiers2 ARGB8888 [LINEAR] empty()            -> modifier=Linear stride=256 planes=1
ERR   modifiers2 ARGB8888 [LINEAR] WRITE              -> ... errno=Some(22)  # EINVAL
```

The last two lines are a clean control/test pair: identical arguments, only the flags differ
(`empty()` vs `WRITE`), so `GBM_BO_USE_WRITE` is unambiguously the differentiator.

**Framed as a question, not a defect.** `gbm.h` documents `GBM_BO_USE_WRITE` as only guaranteed
alongside `GBM_BO_USE_CURSOR` and says it "may not work for other combinations" — i.e. it's
optional/driver-specific. And a working CPU-writable path exists: allocate LINEAR *without*
`WRITE`, then `gbm_bo_map` (that's exactly what our real code does). So nothing is blocked. The
questions for maintainers are:

1. Is the `EINVAL` for `GBM_BO_USE_WRITE` on virtio-gpu **expected**, and is
   *"allocate LINEAR without `WRITE`, then `gbm_bo_map`"* the sanctioned way to get a
   CPU-writable linear buffer here?
2. Is it expected that the **legacy `gbm_bo_create`** paths (`LINEAR`, `RENDERING|LINEAR`)
   `ENOENT` for `ARGB8888`, so that `gbm_bo_create_with_modifiers2` with an explicit modifier
   list is effectively the *only* usable allocation entry point on this stack? (The `ENOENT`
   rather than `EINVAL` is itself a little surprising for an allocation call.)

**Confound ruled out:** it opens the real virtio-gpu render node (`renderD128` →
`driver = virtio-mmio`); the succeeding call returns a sane buffer (`modifier = Linear`,
`stride = 256 = 64·4`, `planes = 1`), so the format is supported and the metadata isn't garbage
— the failures are the driver rejecting specific flag combos, not reproducer artifacts. (This
reproducer allocates only; it doesn't itself `gbm_bo_map`, so the "map alternative works" claim
is from our real code, not demonstrated here.)

---

## Running the reproducers

```sh
# Issue 1 (needs Venus as the Vulkan ICD):
cd repro-vk-getmemfdprops && cargo run
# Issue 2 (gbm only):
cd repro-gbm-alloc && cargo run
```

Each crate is standalone (empty `[workspace]` table) and depends only on the crates listed
above. `Cargo.lock`/`target/` are gitignored; the crate versions that exhibit the behavior are
`gbm 0.18.0` and `ash 0.38.0`, but the behavior is in the driver, not the bindings.

---

## Resolution (2026-07-25, from the VM/host side)

Both issues were re-run on the **current** stack and investigated against Mesa source. The
environment table above is stale — it records mesa `26.1.3` / kernel `7.1.2`; these runs are on
mesa `26.1.4-3.limina.fc44` / kernel `7.1.4-limina16k`.

### Issue 1 — **FIXED**, already deployed

```
QUERY  vkGetMemoryFdPropertiesKHR -> Ok(())
IMPORT vkAllocateMemory(ImportMemoryFdInfoKHR) -> Ok("Ok")
BIND   vkBindImageMemory -> Ok(())
```

Fixed host-side in our `virglrenderer` fork on 2026-07-04 by
`patches/virglrenderer/0024-vkr-gkvm-vkGetMemoryResourcePropertiesMESA-answers-I.patch`, which
cites this issue by name. The diagnosis matched: venus routes the guest's
`vkGetMemoryFdPropertiesKHR(DMA_BUF, fd)` through `vkGetMemoryResourcePropertiesMESA` against the
fd's virtio-gpu resource, and that handler gated on `fd_type == VIRGL_RESOURCE_FD_DMABUF`. On
macOS our resources are **IOSurface / host-memory / shm backed and never Linux dmabufs**, so the
query rejected every resource with `memoryTypeBits = 0` — while the `vkAllocateMemory` path
imported the same resource as a host pointer without complaint. Exactly the inconsistency
reported. The fix mirrors the import in the query: resolve the resource's host pointer and report
the device's host-visible memory types.

The `image_bits & fd_props_bits` masking pattern is safe again; the best-effort fallback can be
dropped whenever convenient.

> **Reproducer nit:** `repro-vk-getmemfdprops` prints its `CONCLUSION: query=INVALID_EXTERNAL_HANDLE
> but import=SUCCESS → inconsistent` line unconditionally, so it now contradicts its own output.
> Worth making conditional — otherwise the next reader sees a passing run reported as a failure.

### Issue 2 — **answered; both questions resolved, and it is two separate things**

Still reproduces, and the two halves have different causes. Neither is a virtio-gpu property.

**(a) `GBM_BO_USE_WRITE` → `EINVAL` is generic Mesa gbm, not this stack.** In
`src/gbm/backends/dri/gbm_dri.c`, `gbm_dri_bo_create` opens with:

```c
if (usage & GBM_BO_USE_WRITE || !dri->has_dmabuf_export)
   return create_dumb(gbm, width, height, format, usage);
```

so *any* `USE_WRITE` request is routed to the KMS dumb-buffer path, which begins:

```c
is_cursor  = (usage & GBM_BO_USE_CURSOR)  != 0 && format == GBM_FORMAT_ARGB8888;
is_scanout = (usage & GBM_BO_USE_SCANOUT) != 0 && (format == XRGB8888 || format == XBGR8888);
if (!is_cursor && !is_scanout) { errno = EINVAL; return NULL; }
```

`LINEAR|WRITE` is neither, so it fails **before any driver or ioctl is involved**. This is
identical on every Mesa driver and matches what `gbm.h` documents — `GBM_BO_USE_WRITE` is only
guaranteed alongside `GBM_BO_USE_CURSOR`. (Even past that gate it would still fail here:
`create_dumb` issues `DRM_IOCTL_MODE_CREATE_DUMB`, and the reproducer opens `renderD128`, a render
node with no KMS.)

**Answer to question 1: yes, expected — and "allocate LINEAR without `WRITE`, then `gbm_bo_map`"
is the sanctioned path.** Confirmed by control: the same `LINEAR|WRITE` call fails identically
under both gallium drivers available here.

**(b) The legacy `gbm_bo_create` `ENOENT` is specific to zink, not to virtio-gpu.** Same binary,
same node, only the gallium driver differs:

```
--- MESA_LOADER_DRIVER_OVERRIDE=virtio_gpu (virgl) ---
OK    create_buffer_object ARGB8888 RENDERING|LINEAR  -> modifier=Linear stride=256 planes=1
OK    create_buffer_object ARGB8888 LINEAR            -> modifier=Linear stride=256 planes=1

--- MESA_LOADER_DRIVER_OVERRIDE=zink ---
ERR   create_buffer_object ARGB8888 RENDERING|LINEAR  -> errno=Some(2)   # ENOENT
ERR   create_buffer_object ARGB8888 LINEAR            -> errno=Some(2)   # ENOENT
```

virgl allocates modifier-less buffers fine. The enhanced tier selects **zink** for guest GL (via
`/etc/environment.d/90-limina-zink.conf`), which is why the legacy entry point looks broken "on
this stack". So `create_buffer_object_with_modifiers2` being the only usable entry point is a
**zink-on-venus gap**, not something inherent to virtio-gpu — and it is worth filing there, since
plenty of software still uses the legacy path.

**Answer to question 2, on the surprising errno: the `ENOENT` is stale and carries no
information.** `gbm_dri_bo_create`'s `failed:` label frees and returns `NULL` **without setting
`errno`**, so what the caller reads is whatever a previous unrelated syscall left behind (almost
certainly a probe `open()` during driver loading). Only some paths in that function set `errno`
deliberately (`EINVAL`, `ENOMEM`); the driver-allocation failure is not one of them. Don't read
meaning into it — that is also worth reporting upstream on its own.

The path down to the driver is otherwise unremarkable: `dri_create_image_with_modifiers` only
rejects a list that is *entirely* `DRM_FORMAT_MOD_INVALID` and otherwise forwards straight to
`dri_create_image`, including with `modifiers = NULL, count = 0`.

---

## What the guest did when it landed — DONE (2026-07-25)

Issue 1's fix reached the deployed VMM the same day this was queued, the gate below passed, and the
fallback is gone. Kept as the record of what was removed and why.

Dropping it against a VMM that predates `patches/virglrenderer/0024` would have turned every dmabuf
import into "no importable memory type for the dmabuf" — every client window black, and the
compositor's own scanout target failing to bind. Hence the gate.

**The gate**, which passed on the new VMM (`query and import agree → FIXED on this stack`). Run
the reproducer on the target stack:

```
cd docs/fork/venus-bugs/repro-vk-getmemfdprops && cargo run --release
```

Its verdict line is now conditional, so it answers the question directly — `→ FIXED on this stack`
means the masking pattern is safe, `→ inconsistent` means it is not. It must pass on **every**
machine the fork runs on, not just the dev VM. (On this dev VM, 2026-07-25: it already passes.)

**The fallback was at three import sites**, each starting from the image's own `memory_type_bits`
and only *narrowing* by the query when it happened to succeed:

| file | function |
|---|---|
| `synoik-vk/src/texture.rs` | `Texture::import_dmabuf_render_target` |
| `synoik-vk/src/texture.rs` | `Texture::import_dmabuf_sampled` |
| `synoik-vk/src/dmabuf.rs` | `ImportedImage::import` (the `ForeignBuffer` path) |

All three now call one helper, `Gpu::dmabuf_memory_type` — query, treat a failure as fatal, mask,
and fail loudly if nothing survives (reporting both masks, so a future disagreement says which side
rejected what). Three copies of the same fifteen lines became one; the rationale lives on the
helper, where the next reader of an import site will find it.

**Why it was worth removing at all**, given it was harmless today: the fallback silently accepts a
memory type the driver never blessed. It is harmless *here* only because this device exposes
exactly **one** memory type (`docs/fork/foundation.md` §5), so whenever the query succeeds its
mask can only be `0b1` and the masking is a no-op — there is nothing for the fallback to get wrong.
On a stack with several memory types — a different host driver, a real GPU passthrough — a query
that failed for some unrelated reason would leave `trailing_zeros()` picking the lowest bit of an
*unfiltered* mask, which need not be importable at all. It is a latent bug that this VM's single
memory type is hiding, the same shape as the `foundation.md` entries.

**Verified, not just compiled.** All three sites run in the suite with the query fatal: the client
sampled-import and scanout-target tests (`vulkan_dmabuf_import_cache_*`,
`vulkan_composites_a_scene_into_a_scanout_dmabuf`, …) pass without skipping, and `cargo run -p
synoik-vk` reports `OK — foreign dmabuf import (GBM, LINEAR modifier) verified` with correct quadrant
colours. 845 green, `SYNOIK_VK_VALIDATION=1` clean.

**Still open from the same pass:** `Gpu::check_modifier_features`' `Unlisted` best-effort path
(`synoik-vk/src/gpu.rs`) was added under the same "the query lies here" assumption and has *not* been
re-checked. It is a different query (`vkGetPhysicalDeviceFormatProperties2` modifier enumeration),
so issue 1's fix says nothing about it either way — it needs its own look.
