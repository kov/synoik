// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Scanout buffers allocated by **our own Vulkan device**, and framebuffers made from them without
//! going through gbm.
//!
//! ## Why this exists
//!
//! The tty backend used to allocate its KMS scanout buffers with gbm and import the exported dmabuf
//! into venus. That import only ever worked by a side effect: gbm's dri backend follows
//! `MESA_LOADER_DRIVER_OVERRIDE`, and while the session set `=zink`, gbm's buffers *were*
//! zink→venus blobs, so the host's venus renderer recognized them as its own. The moment GL was
//! moved to vrend (`=virtio_gpu`), gbm started handing out classic virgl resources and every frame
//! died with `vkGetMemoryFdPropertiesKHR: ERROR_INVALID_EXTERNAL_HANDLE`. A compositor's ability to
//! put a pixel on screen should not depend on a session-wide env var that selects a *GL* driver.
//!
//! Allocating on the renderer's own device removes the question. The dmabuf we hand KMS is a prime
//! export of the venus blob the renderer rendered into — the same kind of handle a venus WSI client
//! hands a compositor, which the whole downstream stack already trusts — and it also takes the
//! host's zero-copy `SET_SCANOUT_BLOB` present, which a gbm/virgl buffer never can.
//!
//! ## What replaced what
//!
//! - [`VulkanScanoutAllocator`] replaces `GbmAllocator` (and the `ScanoutAllocator` wrapper that
//!   existed only to recover gbm's implicitly-chosen modifier).
//! - [`PrimeFramebufferExporter`] replaces `GbmFramebufferExporter`. Smithay's exporter turns a
//!   buffer into a `framebuffer::Handle` by importing it *back into gbm* — the same driver mismatch
//!   in reverse. This one PRIME-imports the dmabuf on the KMS fd and calls `AddFB2` with
//!   `DRM_MODE_FB_MODIFIERS` directly, which is all gbm was ever doing on our behalf.
//!
//! Both are gated at construction on the capabilities the path needs; see
//! [`VulkanScanoutAllocator::new`] and `docs/fork/scanout-allocation.md`.

use std::sync::Arc;

use smithay::backend::allocator::dmabuf::{AsDmabuf, Dmabuf, DmabufFlags};
use smithay::backend::allocator::format::get_opaque;
use smithay::backend::allocator::{Allocator, Buffer, Format, Fourcc, Modifier};
use smithay::backend::drm::exporter::{ExportBuffer, ExportFramebuffer};
use smithay::backend::drm::{DrmDeviceFd, DrmNode, Framebuffer};
use smithay::reexports::drm::buffer::{Handle as DrmBufferHandle, PlanarBuffer};
use smithay::reexports::drm::control::{framebuffer, Device as ControlDevice, FbCmd2Flags};
use smithay::reexports::drm::{Device as BasicDevice, DriverCapability};
use smithay::utils::{Buffer as BufferCoords, Size};
use synoik_vk::gpu::Gpu;
use synoik_vk::texture::{ScanoutExport, Texture};
use tracing::{debug, warn};

use crate::render_helpers::vulkan::scanout_spec;

/// Allocates KMS scanout buffers as exportable images on the renderer's Vulkan device.
#[derive(Clone)]
pub struct VulkanScanoutAllocator {
    gpu: Arc<Gpu>,
    /// Stamped onto every exported [`Dmabuf`] so smithay knows which device can import it. This is
    /// not a hint here — the buffer was allocated *by* that device.
    node: Option<DrmNode>,
}

impl std::fmt::Debug for VulkanScanoutAllocator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `Gpu` is not `Debug` (it is a pile of raw Vulkan handles), and the node is the only part
        // of this worth printing anyway.
        f.debug_struct("VulkanScanoutAllocator")
            .field("node", &self.node)
            .finish_non_exhaustive()
    }
}

impl VulkanScanoutAllocator {
    /// Build an allocator on `gpu`, or fail with the reason it cannot be used.
    ///
    /// Failing here fails compositor start-up, deliberately: there is no gbm fallback any more, and
    /// a stack that cannot do this cannot scan out what our renderer renders. The three extensions
    /// are the whole requirement — the *modifier* side is checked per allocation, against the
    /// candidates the plane actually offered, because that is where a mismatch is legible.
    pub fn new(gpu: Arc<Gpu>, node: Option<DrmNode>) -> anyhow::Result<Self> {
        for ext in [
            "VK_EXT_image_drm_format_modifier",
            "VK_EXT_external_memory_dma_buf",
            "VK_KHR_external_memory_fd",
        ] {
            anyhow::ensure!(
                gpu.supports(ext),
                "the render device does not support {ext}, so it cannot allocate scanout buffers \
                 KMS can display",
            );
        }
        Ok(Self { gpu, node })
    }
}

/// Error type for [`VulkanScanoutAllocator`]. `anyhow::Error` does not implement
/// [`std::error::Error`], which smithay's `Allocator` requires, so allocation failures are
/// flattened into a message.
#[derive(Debug)]
pub struct AllocateError(String);

impl std::fmt::Display for AllocateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AllocateError {}

impl Allocator for VulkanScanoutAllocator {
    type Buffer = VulkanScanoutBuffer;
    type Error = AllocateError;

    fn create_buffer(
        &mut self,
        width: u32,
        height: u32,
        fourcc: Fourcc,
        modifiers: &[Modifier],
    ) -> Result<Self::Buffer, Self::Error> {
        // `Modifier::Invalid` has no encoding in `VkImageDrmFormatModifierListCreateInfoEXT`, so it
        // is asked for as LINEAR. INVALID reaches us when the KMS plane publishes no `IN_FORMATS`
        // blob and therefore advertises its fourccs implicitly — stock virtio-gpu, which on this
        // stack is the only plane there is. That driver has no tiling, so its buffers are linear by
        // construction; asking Vulkan for LINEAR names the layout the plane would have assumed
        // anyway, which is the opposite of guessing. The buffer still reports whatever modifier the
        // driver actually picked, below.
        //
        // On real hardware INVALID means "unknown, ask the allocator" and this substitution would
        // be wrong; see `scanout_render_formats` in `backend::tty` for why this stack is different.
        let mut candidates: Vec<u64> = modifiers
            .iter()
            .map(|m| {
                if *m == Modifier::Invalid {
                    u64::from(Modifier::Linear)
                } else {
                    u64::from(*m)
                }
            })
            .collect();
        // INVALID may collapse onto an already-offered LINEAR; Vulkan wants a set, not a list.
        let mut seen = std::collections::HashSet::new();
        candidates.retain(|m| seen.insert(*m));
        if candidates.is_empty() {
            return Err(AllocateError(format!(
                "no DRM modifier offered for a {fourcc:?} scanout buffer"
            )));
        }

        let spec = scanout_spec(fourcc).ok_or_else(|| {
            AllocateError(format!("the renderer cannot scan out {fourcc:?} at all"))
        })?;

        let (texture, export) = Texture::allocate_scanout(
            &self.gpu,
            width,
            height,
            spec.format,
            &candidates,
            spec.features,
            spec.usage,
            ash::vk::Filter::LINEAR,
        )
        .map_err(|err| AllocateError(format!("{err:#}")))?;

        // The modifier the *driver* picked out of `candidates`, not the one we hoped for: it is
        // what the exported dmabuf names, and therefore what both KMS and the renderer's import
        // will be told.
        let format = Format {
            code: fourcc,
            modifier: Modifier::from(export.modifier),
        };

        Ok(VulkanScanoutBuffer(Arc::new(ScanoutBuffer {
            gpu: self.gpu.clone(),
            texture,
            export,
            size: (width as i32, height as i32).into(),
            format,
            node: self.node,
        })))
    }
}

/// A scanout buffer: one exportable `VkImage` plus the dmabuf handle KMS and the renderer both see.
#[derive(Debug, Clone)]
pub struct VulkanScanoutBuffer(Arc<ScanoutBuffer>);

struct ScanoutBuffer {
    gpu: Arc<Gpu>,
    texture: Texture,
    export: ScanoutExport,
    size: Size<i32, BufferCoords>,
    format: Format,
    node: Option<DrmNode>,
}

impl std::fmt::Debug for ScanoutBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScanoutBuffer")
            .field("size", &self.size)
            .field("format", &self.format)
            .field("stride", &self.export.stride)
            .field("node", &self.node)
            .finish()
    }
}

impl Drop for ScanoutBuffer {
    fn drop(&mut self) {
        self.texture.destroy(&self.gpu);
    }
}

impl Buffer for VulkanScanoutBuffer {
    fn size(&self) -> Size<i32, BufferCoords> {
        self.0.size
    }

    fn format(&self) -> Format {
        self.0.format
    }
}

impl AsDmabuf for VulkanScanoutBuffer {
    type Error = std::io::Error;

    fn export(&self) -> Result<Dmabuf, Self::Error> {
        let inner = &self.0;
        // Every call mints a fresh `Dmabuf` identity, and the renderer keys its scanout import (and
        // therefore the tracked image layout the present blit preserves content from) on that
        // identity. `Slot::export` caches the result in slot userdata, so in the swapchain path
        // this runs once per slot — count it anyway, because "once per slot" is the assumption, not
        // a guarantee, and a re-export silently costs the buffer its content.
        static EXPORTS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = EXPORTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        debug!(
            n,
            size = ?inner.size,
            "exporting a scanout buffer as a fresh dmabuf identity"
        );
        // A dup, not the original: the `Dmabuf` owns whatever fd it is given, and this buffer may
        // be exported more than once (smithay caches per swapchain slot, but nothing guarantees
        // it).
        let fd = inner.export.fd.try_clone()?;
        let mut builder = Dmabuf::builder(
            inner.size,
            inner.format.code,
            inner.format.modifier,
            DmabufFlags::empty(),
        );
        if let Some(node) = inner.node {
            builder.set_node(node);
        }
        builder.add_plane(fd, 0, inner.export.offset, inner.export.stride);
        builder.build().ok_or_else(|| {
            std::io::Error::other("a scanout buffer exported with no planes".to_string())
        })
    }
}

// --- framebuffer export ------------------------------------------------------------------------

/// A `framebuffer::Handle` made by PRIME-importing a dmabuf on the KMS fd.
///
/// The GEM handles the import produced are closed as soon as the framebuffer exists — the FB holds
/// its own reference to the underlying object, so keeping them would just leak a handle per buffer.
#[derive(Debug)]
pub struct PrimeFramebuffer {
    fb: framebuffer::Handle,
    format: Format,
    drm: DrmDeviceFd,
}

impl Drop for PrimeFramebuffer {
    fn drop(&mut self) {
        if let Err(err) = self.drm.destroy_framebuffer(self.fb) {
            warn!(fb = ?self.fb, ?err, "failed to destroy framebuffer");
        }
    }
}

impl AsRef<framebuffer::Handle> for PrimeFramebuffer {
    fn as_ref(&self) -> &framebuffer::Handle {
        &self.fb
    }
}

impl Framebuffer for PrimeFramebuffer {
    fn format(&self) -> Format {
        self.format
    }
}

/// Turns dmabufs into KMS framebuffers by PRIME import + `AddFB2WithModifiers`, with no gbm in the
/// path.
///
/// `import_node` filters which *client* buffers may be considered for direct scanout, matching
/// `GbmFramebufferExporter`'s parameter of the same name: a client buffer allocated on another
/// device is not ours to put on a plane.
#[derive(Debug, Clone)]
pub struct PrimeFramebufferExporter {
    import_node: Option<DrmNode>,
}

impl PrimeFramebufferExporter {
    pub fn new(import_node: Option<DrmNode>) -> Self {
        Self { import_node }
    }
}

#[derive(Debug)]
pub enum ExportError {
    Export(std::io::Error),
    Prime(std::io::Error),
    AddFramebuffer(std::io::Error),
    NoModifier,
    UnnameableModifier(Modifier),
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Export(err) => {
                write!(f, "exporting the scanout buffer as a dmabuf failed: {err}")
            }
            Self::Prime(err) => write!(
                f,
                "PRIME-importing a dmabuf plane onto the KMS device failed: {err}"
            ),
            Self::AddFramebuffer(err) => {
                write!(f, "adding a framebuffer for a dmabuf failed: {err}")
            }
            Self::NoModifier => f.write_str("a dmabuf with modifier INVALID cannot be scanned out"),
            Self::UnnameableModifier(modifier) => write!(
                f,
                "the KMS device does not support DRM_CAP_ADDFB2_MODIFIERS, so a buffer with \
                 modifier {modifier:?} cannot be described to it"
            ),
        }
    }
}

impl std::error::Error for ExportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Export(err) | Self::Prime(err) | Self::AddFramebuffer(err) => Some(err),
            Self::NoModifier | Self::UnnameableModifier(_) => None,
        }
    }
}

impl ExportFramebuffer<VulkanScanoutBuffer> for PrimeFramebufferExporter {
    type Framebuffer = PrimeFramebuffer;
    type Error = ExportError;

    fn add_framebuffer(
        &self,
        drm: &DrmDeviceFd,
        buffer: ExportBuffer<'_, VulkanScanoutBuffer>,
        use_opaque: bool,
    ) -> Result<Option<Self::Framebuffer>, Self::Error> {
        let dmabuf = match buffer {
            ExportBuffer::Wayland(wl_buffer) => {
                let Ok(dmabuf) = smithay::wayland::dmabuf::get_dmabuf(wl_buffer) else {
                    // shm and friends: not scanout-able, and not an error.
                    return Ok(None);
                };
                // A *client* buffer with no named layout declines direct scanout; it does not fail
                // the frame. Our own buffers cannot get here with INVALID (allocation refuses it),
                // which is why the same case below is an error rather than a `None`.
                if dmabuf.format().modifier == Modifier::Invalid {
                    return Ok(None);
                }
                // Likewise a client buffer whose layout this device has no way to be told about
                // declines scanout rather than failing the frame.
                if !device_names_modifiers(drm) && dmabuf.format().modifier != Modifier::Linear {
                    return Ok(None);
                }
                dmabuf.clone()
            }
            ExportBuffer::Allocator(buffer) => buffer.export().map_err(ExportError::Export)?,
        };
        framebuffer_from_dmabuf(drm, &dmabuf, use_opaque).map(Some)
    }

    fn can_add_framebuffer(&self, buffer: &ExportBuffer<'_, VulkanScanoutBuffer>) -> bool {
        match buffer {
            // Our own buffers, always.
            ExportBuffer::Allocator(_) => true,
            // A client buffer only if it is a dmabuf from the device we scan out from. Anything
            // else would have to be copied first, which is what compositing already does.
            ExportBuffer::Wayland(buffer) => smithay::wayland::dmabuf::get_dmabuf(buffer)
                .is_ok_and(|buf| self.import_node.is_some() && buf.node() == self.import_node),
        }
    }
}

/// Whether `drm` accepts `DRM_MODE_FB_MODIFIERS` on `AddFB2` at all.
///
/// A device without the capability rejects the flag outright, so a buffer's layout can only be
/// implied there, never named. Stock virtio-gpu is such a device — it advertises no `IN_FORMATS`
/// blob either, which is the same fact seen from the plane side.
fn device_names_modifiers(drm: &DrmDeviceFd) -> bool {
    matches!(
        drm.get_driver_capability(DriverCapability::AddFB2Modifiers),
        Ok(1)
    )
}

/// PRIME-import every plane of `dmabuf` on `drm` and add a framebuffer for it.
///
/// `use_opaque` substitutes the alpha-less twin of the fourcc (`Argb8888` → `Xrgb8888`), which is
/// what tells KMS it may ignore the alpha channel on the primary plane.
fn framebuffer_from_dmabuf(
    drm: &DrmDeviceFd,
    dmabuf: &Dmabuf,
    use_opaque: bool,
) -> Result<PrimeFramebuffer, ExportError> {
    // Weston's rule, and smithay's: a buffer with no named layout must not go to KMS. The driver
    // and the allocator agreed out of band on something neither we nor KMS can see, and displaying
    // it is a coin flip between a picture and garbage.
    if dmabuf.format().modifier == Modifier::Invalid {
        return Err(ExportError::NoModifier);
    }

    // A device without `DRM_CAP_ADDFB2_MODIFIERS` rejects `AddFB2` with `DRM_MODE_FB_MODIFIERS`
    // outright, so a modifier cannot be named to it at all — it assumes the driver's implicit
    // layout. That is only the same buffer we have if the buffer is LINEAR, which on the one
    // driver that lands here (stock virtio-gpu, no tiling) is what every buffer is. Anything else
    // is refused rather than handed over unnamed, which is the garbage case below.
    let name_modifier = device_names_modifiers(drm);
    if !name_modifier && dmabuf.format().modifier != Modifier::Linear {
        return Err(ExportError::UnnameableModifier(dmabuf.format().modifier));
    }

    let mut handles = [None; 4];
    let mut pitches = [0u32; 4];
    let mut offsets = [0u32; 4];
    // Imported GEM handles, closed once the framebuffer holds its own reference. Kept in a guard
    // so an error partway through the loop still closes what was imported.
    let mut imported = GemHandles::new(drm);
    for (idx, ((fd, offset), stride)) in dmabuf
        .handles()
        .zip(dmabuf.offsets())
        .zip(dmabuf.strides())
        .enumerate()
    {
        let handle = drm.prime_fd_to_buffer(fd).map_err(ExportError::Prime)?;
        imported.0.push(handle);
        handles[idx] = Some(handle);
        offsets[idx] = offset;
        pitches[idx] = stride;
    }

    let code = dmabuf.format().code;
    let code = if use_opaque {
        get_opaque(code).unwrap_or(code)
    } else {
        code
    };
    let planar = DmabufPlanes {
        size: (dmabuf.width(), dmabuf.height()),
        code,
        // `add_planar_framebuffer` *asserts* that the flag and this agree, so it is not enough to
        // drop the flag.
        modifier: name_modifier.then_some(dmabuf.format().modifier),
        handles,
        pitches,
        offsets,
    };

    // Still no legacy `AddFB` fallback: this is `AddFB2` either way, only without naming a modifier
    // on a device that has no way to hear one. If it fails, the layout really is unacceptable to
    // the display engine and retrying differently would be the garbage case above.
    let flags = if name_modifier {
        FbCmd2Flags::MODIFIERS
    } else {
        FbCmd2Flags::empty()
    };
    let fb = drm
        .add_planar_framebuffer(&planar, flags)
        .map_err(ExportError::AddFramebuffer)?;

    Ok(PrimeFramebuffer {
        fb,
        format: Format {
            code,
            modifier: dmabuf.format().modifier,
        },
        drm: drm.clone(),
    })
}

/// Closes PRIME-imported GEM handles on drop. The framebuffer, once created, holds its own
/// reference to the underlying buffer object, so the handles are ours to release either way —
/// including on the error path, which is why this is a guard and not a loop at the end.
struct GemHandles<'a>(Vec<DrmBufferHandle>, &'a DrmDeviceFd);

impl<'a> GemHandles<'a> {
    fn new(drm: &'a DrmDeviceFd) -> Self {
        Self(Vec::new(), drm)
    }
}

impl Drop for GemHandles<'_> {
    fn drop(&mut self) {
        for handle in self.0.drain(..) {
            if let Err(err) = self.1.close_buffer(handle) {
                warn!(?handle, ?err, "failed to close a PRIME-imported GEM handle");
            }
        }
    }
}

/// The planar view `drmModeAddFB2WithModifiers` wants, built from a dmabuf's own metadata rather
/// than from a re-imported buffer object (which is where gbm's version had to second-guess the
/// pitches it got back).
struct DmabufPlanes {
    size: (u32, u32),
    code: Fourcc,
    /// `None` when the device cannot be told a modifier at all (no `DRM_CAP_ADDFB2_MODIFIERS`).
    modifier: Option<Modifier>,
    handles: [Option<DrmBufferHandle>; 4],
    pitches: [u32; 4],
    offsets: [u32; 4],
}

impl PlanarBuffer for DmabufPlanes {
    fn size(&self) -> (u32, u32) {
        self.size
    }

    fn format(&self) -> Fourcc {
        self.code
    }

    fn modifier(&self) -> Option<Modifier> {
        self.modifier
    }

    fn pitches(&self) -> [u32; 4] {
        self.pitches
    }

    fn handles(&self) -> [Option<DrmBufferHandle>; 4] {
        self.handles
    }

    fn offsets(&self) -> [u32; 4] {
        self.offsets
    }
}
