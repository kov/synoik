# Scanout allocation: our own Vulkan device, not gbm

**Status: landed 2026-08-05.** `src/backend/vulkan_scanout.rs`, replacing
`src/backend/scanout_allocator.rs` (deleted) and smithay's `GbmFramebufferExporter` on the scanout
path. There is no gbm fallback.

## The break this closes

The tty backend allocated its KMS scanout buffers with gbm and imported the exported dmabuf into
venus (`vkGetMemoryFdPropertiesKHR`). That import only ever worked by a side effect: gbm's dri
backend follows `MESA_LOADER_DRIVER_OVERRIDE`, and while the session set `=zink`, the buffers gbm
handed out *were* zink→venus blobs — so the host's venus renderer (vkr) recognized them as its own
allocations.

On 2026-08-04 the enhanced-tier default flipped the selector to `=virtio_gpu` (GL now rides vrend).
gbm silently started handing out classic virgl resources, and vkr refuses those:

```
vkr: failed to query resource props: invalid res_id 9
  (returning VK_ERROR_INVALID_EXTERNAL_HANDLE, ring stays alive)
→ error rendering frame: vkGetMemoryFdPropertiesKHR: ERROR_INVALID_EXTERNAL_HANDLE
```

Every frame, with the host window still showing Plymouth's last frame while the session was
otherwise alive. The stopgap was to put `MESA_LOADER_DRIVER_OVERRIDE=zink` back
(`/etc/environment.d/90-limina-zink.conf`) — which re-couples a Vulkan compositor's ability to put a
pixel on screen to a session-wide env var that selects a **GL** driver, and to guest-zink, which
limina no longer configures.

The generalisable part: **a compositor that renders in Vulkan should allocate what it renders into.**
Routing that allocation through a second driver stack makes the two agree only by coincidence, and
the coincidence is somebody else's default to change.

## What we do instead

Per scanout buffer, on the renderer's own `Gpu` (`Texture::allocate_scanout`, `synoik-vk`):

1. `vkCreateImage` with `tiling = DRM_FORMAT_MODIFIER_EXT`, a
   `VkImageDrmFormatModifierListCreateInfoEXT` carrying the candidates the *plane* offered, and
   `VkExternalMemoryImageCreateInfo { handleTypes = DMA_BUF }`. Candidates the device does not
   enumerate, or that lack the format features the bind path's commands need, are filtered out
   first — the list create-info gives no way to learn afterwards *why* creation failed.
2. Dedicated (`VkMemoryDedicatedAllocateInfo`) + exportable (`VkExportMemoryAllocateInfo`)
   allocation. No `vkGetMemoryFdPropertiesKHR`: that query answers "which heaps can hold this
   *foreign* handle", and there is no foreign handle — we are creating it.
3. `vkGetImageDrmFormatModifierPropertiesEXT` for the modifier the driver actually picked. That is
   what the exported dmabuf names, and therefore what both KMS and the renderer are told.
4. `vkGetImageSubresourceLayout(VK_IMAGE_ASPECT_MEMORY_PLANE_0_BIT_EXT)` for offset/rowPitch.
   **Never `width * 4`** — a driver may pad, and on this stack the value became truthful only with
   the modifier passthrough in mesa 26.1.5.
5. `vkGetMemoryFdKHR(DMA_BUF)` for the dmabuf fd. On virtio-gpu this is a prime export of the venus
   blob GEM — the same handle a venus WSI client hands a compositor, which the whole downstream
   stack already trusts.

The framebuffer then comes from `PrimeFramebufferExporter`: `drmPrimeFDToHandle` on the KMS fd plus
`AddFB2` with `DRM_MODE_FB_MODIFIERS`, and the imported GEM handles closed as soon as the FB holds
its own reference.

### The implicit-modifier plane

A **stock** virtio-gpu (no `DRM_CAP_ADDFB2_MODIFIERS`, no `IN_FORMATS` blob) advertises its plane
formats with the implicit/`INVALID` modifier, so a plain intersection against a LINEAR-only renderer
set is empty and `DrmCompositor::new` fails with *"No supported plane buffer format found"* — the
fourcc matched, only the modifier did not. That is a compositor that never starts, which reads as a
boot hang (gdm never takes the display, `plymouth-quit-wait` never completes).

Three places cooperate to accept it, and none of them guesses a layout:

- `scanout_render_formats` (`backend/tty.rs`) adds an `INVALID` twin per LINEAR entry, **for the
  compositor negotiation only** — `owned_vulkan_dmabuf_formats`, which is also what clients see,
  stays explicit.
- `VulkanScanoutAllocator::create_buffer` asks Vulkan for LINEAR when offered `INVALID`
  (`INVALID` has no encoding in `VkImageDrmFormatModifierListCreateInfoEXT`), and still reports back
  whatever modifier the driver picked.
- `framebuffer_from_dmabuf` drops the `DRM_MODE_FB_MODIFIERS` flag when the device has no
  `DRM_CAP_ADDFB2_MODIFIERS` — still `AddFB2`, just without naming a modifier to a device that
  cannot hear one. A non-LINEAR buffer is refused there rather than handed over unnamed.

This is safe **because of what this plane is**: virtio-gpu has no tiling, so its buffers are linear
by construction, and our scanout buffers are created by the host through venus, so the host already
knows their exact layout. On real hardware `INVALID` means "unknown, ask the allocator" and refusing
it stays correct.

#### …and pass-through scanout on it

The three above get the compositor *running*; a fourth thing is needed to let a **client** buffer
reach the plane. `DrmCompositor` gates every promotion on

```rust
if !plane.formats.contains(&element_config.properties.format)   // compositor/mod.rs
```

which on an implicit plane compares a buffer that names LINEAR against a plane that names nothing,
and refuses everything. That kills direct scan-out outright — for the **primary** plane too, so it
is not something the caller can steer.

It is also not reachable from here. The `planes: Option<Planes>` argument to `DrmCompositor::new`
only feeds *overlay* and *cursor* assignment; `try_assign_primary_plane` reads
`self.surface.plane_info()`, a `DrmSurface` field built at surface creation. So this one is fixed in
the smithay fork (`drm/compositor: allow scanout on a plane with only implicit modifiers`): when
**every** plane entry is `INVALID`, match on the fourcc alone. A plane that names some modifiers is
still matched exactly.

Colour safety is unchanged — the fourcc is still a requirement, and an element whose buffer is
opaque gets `get_opaque` applied by `PrimeFramebufferExporter` before the comparison, so an
`Argb8888` client still promotes onto an `XR24`-only plane while an RGBA-order one does not.

### Why the exporter had to be rewritten too

Smithay's `GbmFramebufferExporter` turns a buffer into a `framebuffer::Handle` by importing it
**back into gbm** (`framebuffer_from_dmabuf` → `dmabuf.import_to(gbm, SCANOUT)`). Handing a venus
blob to a vrend-gbm is the same driver mismatch in reverse — swapping the allocator alone would have
moved the failure, not removed it. Smithay ships only gbm and dumb exporters, so this one is ours.

### What still uses gbm

The **cursor plane**. `DrmCompositor` takes a `GbmDevice` purely to allocate CPU-written
`CURSOR | WRITE` buffers and frame them with `framebuffer_from_bo` on the same device; Vulkan never
sees them, so none of the above applies. (Today the cursor is software anyway — see the
cursor-plane-hotspot note.) `Tty::primary_gbm_device` and the headless backend's gbm are likewise
untouched.

## What we get

- **No env coupling.** Nothing in the scanout path consults `MESA_LOADER_DRIVER_OVERRIDE`, because
  no GL driver is involved. **This does not mean the zink drop-in can be retired** — see "The other
  half" below; it is still load-bearing for *client* buffers.
- **Zero-copy present.** A venus-blob framebuffer takes limina's `SET_SCANOUT_BLOB` path: the host
  resolves the blob's IOSurface and puts it on glass with no copy. A gbm/virgl buffer can never do
  that for a venus-rendered compositor.
- **One less import per scanout buffer.** Not yet realised — see "Left on the table".

## Requirements, and what happens when they are missing

`VulkanScanoutAllocator::new` fails compositor start-up if the render device lacks
`VK_EXT_image_drm_format_modifier`, `VK_EXT_external_memory_dma_buf` or `VK_KHR_external_memory_fd`.
Per allocation it fails if no offered modifier survives the feature check. Both are deliberate:
there is no fallback to fall back *to*, and a silent one would reintroduce exactly the class of bug
this replaces.

Verified present on this stack (2026-08-05, `vulkaninfo` + `drm_info /dev/dri/card0`):

| Requirement | Value |
| --- | --- |
| `VK_EXT_image_drm_format_modifier` | rev 2 (real host passthrough), venus / Mesa 26.1.5 |
| `VK_EXT_external_memory_dma_buf`, `VK_KHR_external_memory_fd` | present |
| `DRM_CAP_ADDFB2_MODIFIERS` | 1 |
| primary + cursor plane `IN_FORMATS` | `DRM_FORMAT_MOD_LINEAR` for XRGB/ARGB/XBGR/ABGR8888 |

Those last two rows were a **limina kernel patch**, not upstream: guest kernel 7.1.8 dropped it, so
the cap is `0` and the primary plane advertises `XR24` alone with `INVALID`. See *The
implicit-modifier plane* above — the compositor handles both shapes now, and must keep doing so.
| host KosmicKrisp modifiers | LINEAR only |

**LINEAR only** — do not negotiate other modifiers on this stack; the host advertises nothing else.
KosmicKrisp also wants `rowPitch ≥ 16` and 16-byte alignment, which only bites 1–4 px-wide images;
real scanouts are fine, and the pitch comes from the query regardless.

## Testing

`vulkan_renders_into_its_own_scanout_dmabuf` (`render_helpers/vulkan/tests.rs`) allocates through
`VulkanScanoutAllocator`, checks the driver reported its chosen modifier back and that the queried
pitch covers a row, exports the dmabuf, binds it through the ordinary `Bind<Dmabuf>`, renders the
solid scene and reads it back. It is the twin of `vulkan_renders_into_a_gbm_dmabuf` and, unlike it,
does not care which GL driver is selected.

What the headless suite **cannot** cover: `drmModeAddFB2WithModifiers` needs DRM master, so
`PrimeFramebufferExporter` is only exercised on a real seat.

**Seat-validated 2026-08-05.** gsrs, `MESA_LOADER_DRIVER_OVERRIDE=virtio_gpu` +
`GALLIUM_DRIVER=virtio_gpu` (so gbm inside the compositor resolves to vrend — the configuration that
killed every frame on 2026-08-04), `SYNOIK_VK_VALIDATION=1`, eleven minutes with gnome-terminal,
Firefox, Epiphany and vkmark: zero `VULKAN ERROR`, zero `legacy fbadd`, zero framebuffer or
page-flip failures. The absence of `legacy fbadd` is the positive signal for
`PrimeFramebufferExporter` specifically — `AddFB2` with `DRM_MODE_FB_MODIFIERS` was accepted every
time.

Setting that up has one trap worth knowing: **`environment.d` did not carry it.** A
`~/.config/environment.d/95-*.conf` beats `/etc/environment.d/90-limina-zink.conf` lexicographically
and the generator does resolve it that way — verified by running
`30-systemd-environment-d-generator` by hand, which emitted `virtio_gpu` — yet the running session
still saw `zink`. Use the unit's `Environment=` instead; that worked first try.

Why it lost is **not** established. Ruled out by measurement: no other file on disk sets the
variable; the user manager does not inherit it from PAM (its own `/proc/<pid>/environ` has no
`MESA`); the generator finds the user file even from a bare environment; and the manager restarts on
every login, so it was not stale. What *is* certain is that something uploads a session environment
over the generator's output — `WAYLAND_DISPLAY`, `DISPLAY`, `GDMSESSION` and
`XDG_SESSION_TYPE=wayland` all appear in `systemctl --user show-environment` while being absent from
what the manager was exec'd with. That would also explain the observed split, where the *new*
`SYNOIK_VK_VALIDATION` came through while the *pre-existing* `MESA_LOADER_DRIVER_OVERRIDE` did not.
Unproven, and a probe file plus one login after a clean boot would settle it.

Note also that `Environment=` on the compositor's unit is *not* compositor-scoped in practice: apps
launched from the shell are spawned by the compositor and inherit it, which is how the client
failure below was found.

## The screen decayed between damages (fixed 2026-08-05)

Hours after this landed, the desktop started accumulating trails: black rings at every size a
window had been, stripes at a terminal's text-line pitch running off past its edge, growing
wherever the scene *stopped* repainting. A VT switch (which forces a full redraw) wiped it clean and
it grew straight back. Our own `screenshot-screen` never saw it. Two changes had landed together —
this one and a new VMM — so attribution was the whole problem.

**The bug was ours, and older than this commit.** `matches_render_order` (`11cb699b`, 2026-07-31)
gave `Argb8888`/`Xrgb8888` scanout buffers a **direct** render path: the imported dmabuf *is* the
render-pass attachment, no present-blit shadow. But `VulkanFrame::begin` still decided whether to
preserve the target with `fb.present.is_some()` — a test for the *shadow* arm. So the direct arm
took the `DONT_CARE` base pass and discarded the whole scanout buffer every frame, while the tty
backend redrew only `DrmCompositor`'s buffer-age damage. Everything outside the damage was, by the
spec, undefined. The fix is one condition (`!fb.offscreen`, i.e. *any* scanout target that already
holds a valid frame), because a cycled dmabuf holds exactly its own age-N-ago presentation — the
frame that damage was computed against. Preserving it is not an approximation.

Worth keeping from how it was found:

- **The screenshot exonerating the scene is the clue, not a dead end.** A capture that re-renders
  everything cannot see a partial-repaint bug. "Invisible to a full redraw" localised it to the
  presented framebuffer before a single line of code was read.
- **An instrument that never fires can be the answer.** `SYNOIK_VK_FULL_PRESENT_BLIT=1` (a
  temporary arm) produced *zero* log lines. That was not a null result: it proved the present-blit
  path never runs for KMS on this stack, which is what pointed at the direct arm.
- **The pixels cannot test this.** `DONT_CARE` leaves contents *undefined*, and venus happens to
  keep them for a LINEAR image — a pixel-level test passes on the broken code here and would fail
  on some other driver, some other day. `vulkan_partial_redraw_into_a_scanout_dmabuf_preserves_the_rest`
  asserts the **pass choice** (`VulkanFrame::preserves_target`), which fails on the old code.
- `SYNOIK_VK_FULL_DAMAGE=1` (kept, `backend::tty`) turns the whole partial-damage chain off: the
  diagnostic that separates "we are drawing the wrong thing" from "what we drew did not survive",
  and a stopgap while that is decided.

## The other half: client buffers, still open

Our own scanout buffers are fixed. A **client's** are not ours to allocate, and the same failure
lives there: with GL on vrend a client's dmabufs are classic virgl resources, and
`vkGetMemoryFdPropertiesKHR` refuses them, so `Tty::import_dmabuf` returns `false` and we decline the
buffer. That is the right call — the alternative is garbage on screen — but Firefox and Epiphany both
*hang* rather than falling back, so in practice it is a dead window.

Observed 2026-08-05 in the run above: exactly two
`error importing dmabuf into the Vulkan renderer: ERROR_INVALID_EXTERNAL_HANDLE`, one per browser,
at the moment each tried to map its window, and only for browsers launched **from the shell** (which
inherited the compositor's `virtio_gpu`). The same browsers launched from a terminal — still on
zink — ran clean for the rest of the session.

Mutter does not hit this: its renderer is GL, so it imports client buffers through the same driver
that allocated them. A Vulkan compositor is cross-driver by construction. The fix is host-side —
vkr importing classic virgl resources into venus, which limina has booked (and may already be
deployed; unconfirmed as of 2026-08-05). **Until that is confirmed, `/etc/environment.d/90-limina-zink.conf`
stays.** It is no longer protecting our scanout; it is keeping GL clients importable.

## Left on the table

**Skip the import on our own buffers.** `DrmCompositor` hands the renderer a `Dmabuf` and
`import_dmabuf_target` builds a *second* `VkImage` around it, complete with the
`vkGetMemoryFdPropertiesKHR` this doc says is gone from the allocation path. It is correct — the
buffer is ours and re-imports fine — but it is redundant: the allocator already holds the `VkImage`
that memory belongs to. Registering it against the exported dmabuf so `bind()` reuses it would drop
one image, one memory import and one query per swapchain buffer. Worth doing after the seat
validates the current shape, not before.
