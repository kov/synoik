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

**A third, later finding lives next door:** timestamp queries are advertised in full and resolve
to zero — see [`../venus-timestamp-gap.md`](../venus-timestamp-gap.md), with its reproducer in
[`repro-vk-timestamp-query/`](./repro-vk-timestamp-query). It is written up separately because it
is a handoff for the VM-stack work rather than a dmabuf-import finding.

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
