//! Scanout buffer allocation for KMS drivers that expose **no DRM format modifiers**.
//!
//! A driver that sets `fb_modifiers_not_supported` (upstream virtio-gpu does, in
//! `virtgpu_display.c`) reports `DRM_CAP_ADDFB2_MODIFIERS = 0`, attaches no `IN_FORMATS` blob to
//! any plane, and rejects `drmModeAddFB2` calls that carry `DRM_MODE_FB_MODIFIERS`. Smithay then
//! synthesizes `Modifier::Invalid` for every plane format (`backend::drm::plane_formats`), so the
//! `DrmCompositor` negotiates `Invalid` and the swapchain allocates through gbm's *implicit* path.
//!
//! KMS is happy with that — implicit means gbm and the kernel driver of the same device agree out
//! of band, and we ask for `SCANOUT`, so whatever gbm picks is by construction something the
//! display engine can scan out. The renderer is not: it imports through
//! `VK_EXT_image_drm_format_modifier`, which has no encoding for "implicit". The layout has to be
//! named.
//!
//! Smithay masks the modifier to `Invalid` for implicit allocations, because gbm's answer is not
//! trustworthy on every driver. This allocator reads it anyway — [`GbmBuffer`] keeps the
//! underlying `BufferObject` reachable — and keeps it on the buffer, so the exported
//! [`Dmabuf`](smithay::backend::allocator::dmabuf::Dmabuf) names the layout Vulkan needs.
//!
//! Two consequences, both deliberate:
//!
//! - KMS sees an explicit modifier and so takes `AddFB2` *with* `MODIFIERS`, which such a driver
//!   rejects — smithay then falls back to the legacy `AddFB` (one "using legacy fbadd" warning per
//!   session, one failed ioctl per swapchain buffer, not per frame). That fallback *is* the
//!   implicit path, and it is correct for the reason above.
//! - Whether the renderer can actually use the layout gbm picked is Vulkan's question to answer,
//!   not ours to pre-empt: `check_modifier` queries the driver's features for that exact modifier
//!   and fails closed. Refusing anything non-linear here instead would turn a working tiled/tiled
//!   pipeline into a compositor that will not start — and would not catch the failure that actually
//!   hurts, a buffer *misreported* as linear.
//!
//! The one thing we do refuse is gbm reporting `Invalid` back at us: there is then no layout to
//! name in the import, and guessing one is how a buffer scans out as garbage.

use std::io;
use std::ops::Deref;
use std::sync::Mutex;

use smithay::backend::allocator::gbm::{GbmAllocator, GbmBuffer, GbmBufferFlags, GbmDevice};
use smithay::backend::allocator::{Allocator, Fourcc, Modifier};
use smithay::backend::drm::DrmDeviceFd;
use tracing::warn;

/// The layout we expect an implicit allocation to come back with on the stacks we run on.
///
/// Nothing enforces it — it is what the warning in [`ScanoutAllocator::create_buffer`] is measured
/// against, so an unexpected layout shows up in the journal instead of silently becoming the new
/// normal.
pub const EXPECTED_IMPLICIT_MODIFIER: Modifier = Modifier::Linear;

/// Wraps [`GbmAllocator`] so implicit allocations keep the modifier gbm actually chose.
///
/// Explicit-modifier allocations pass straight through — they already carry their modifier.
#[derive(Debug)]
pub struct ScanoutAllocator {
    inner: GbmAllocator<DrmDeviceFd>,
    device: GbmDevice<DrmDeviceFd>,
    flags: GbmBufferFlags,
    /// Modifiers we have already reported as unexpected, so a swapchain of N buffers logs once.
    warned: Mutex<Vec<Modifier>>,
}

impl Clone for ScanoutAllocator {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            device: self.device.clone(),
            flags: self.flags,
            warned: Mutex::new(Vec::new()),
        }
    }
}

impl ScanoutAllocator {
    pub fn new(device: GbmDevice<DrmDeviceFd>, flags: GbmBufferFlags) -> Self {
        Self {
            inner: GbmAllocator::new(device.clone(), flags),
            device,
            flags,
            warned: Mutex::new(Vec::new()),
        }
    }

    fn warn_once(&self, modifier: Modifier, fourcc: Fourcc) {
        let mut warned = self.warned.lock().unwrap();
        if warned.contains(&modifier) {
            return;
        }
        warned.push(modifier);

        warn!(
            "gbm chose {modifier:?} for an implicit {fourcc:?} scanout buffer, not \
             {EXPECTED_IMPLICIT_MODIFIER:?}; importing under that layout — if the display is \
             garbled, this is the first thing to suspect"
        );
    }
}

impl Allocator for ScanoutAllocator {
    type Buffer = GbmBuffer;
    type Error = io::Error;

    fn create_buffer(
        &mut self,
        width: u32,
        height: u32,
        fourcc: Fourcc,
        modifiers: &[Modifier],
    ) -> Result<Self::Buffer, Self::Error> {
        if !wants_implicit(modifiers) {
            return self.inner.create_buffer(width, height, fourcc, modifiers);
        }

        // gbm's pre-modifier entry point: the driver picks the layout, the same way it does for
        // any client that allocates without `EGL_EXT_image_dma_buf_import_modifiers`.
        let bo = self
            .device
            .create_buffer_object::<()>(width, height, fourcc, self.flags)?;

        // Deliberately `implicit = false`: smithay would otherwise overwrite the modifier with
        // `Invalid`, and the exported dmabuf is the renderer's only view of this buffer.
        let buffer = GbmBuffer::from_bo(bo, false);

        // `Buffer::format` now reports what gbm chose, so read the object through the deref to
        // keep the two straight while checking.
        let real = Deref::deref(&buffer).modifier();
        if real == Modifier::Invalid {
            return Err(io::Error::other(format!(
                "gbm allocated an implicit {fourcc:?} scanout buffer and reports no modifier for \
                 it, leaving nothing to name in the Vulkan import"
            )));
        }
        if real != EXPECTED_IMPLICIT_MODIFIER {
            self.warn_once(real, fourcc);
        }

        Ok(buffer)
    }
}

/// Whether this request is for an implicit allocation — i.e. the only thing the plane and the
/// renderer could agree on was `Invalid`.
fn wants_implicit(modifiers: &[Modifier]) -> bool {
    !modifiers.is_empty() && modifiers.iter().all(|m| *m == Modifier::Invalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_only_requests_are_implicit() {
        assert!(wants_implicit(&[Modifier::Invalid]));
    }

    #[test]
    fn explicit_requests_are_not_implicit() {
        assert!(!wants_implicit(&[Modifier::Linear]));
        // A mixed list still has something explicit to try, and `GbmAllocator` handles the
        // fallback ordering; taking the implicit path here would throw that away.
        assert!(!wants_implicit(&[
            Modifier::Invalid,
            Modifier::I915_x_tiled
        ]));
    }

    #[test]
    fn empty_requests_are_not_implicit() {
        // An empty list is not a request for implicit; let gbm reject it as it always would.
        assert!(!wants_implicit(&[]));
    }
}
