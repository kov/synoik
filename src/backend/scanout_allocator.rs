//! Scanout buffer allocation for KMS drivers that expose **no DRM format modifiers**.
//!
//! A driver that sets `fb_modifiers_not_supported` (upstream virtio-gpu does, in
//! `virtgpu_display.c`) reports `DRM_CAP_ADDFB2_MODIFIERS = 0`, attaches no `IN_FORMATS` blob to
//! any plane, and rejects `drmModeAddFB2` calls that carry `DRM_MODE_FB_MODIFIERS`. Smithay then
//! synthesizes `Modifier::Invalid` for every plane format (`backend::drm::plane_formats`), so the
//! `DrmCompositor` picks `Invalid` and the swapchain allocates through gbm's *implicit* path.
//!
//! That is fine for KMS — smithay maps `Invalid` back to "no modifiers" and emits a plain
//! `AddFB2` — but it is not fine for us: the owned Vulkan renderer imports scanout buffers with
//! `VK_EXT_image_drm_format_modifier`, which has no encoding for "implicit". The layout has to be
//! named.
//!
//! `DRM_FORMAT_MOD_INVALID` does **not** mean linear; it means "the two sides agree out of band".
//! So rather than assume, this allocator asks: gbm knows the layout it actually picked, and
//! [`GbmBuffer`] keeps the underlying `BufferObject` reachable even after smithay masks the
//! modifier to `Invalid` for the KMS path. We read the real one and refuse the buffer if it is
//! anything but linear — a tiled implicit buffer imported as linear would scan out as garbage,
//! and a loud failure at allocation beats that.
//!
//! The masked `Invalid` is what reaches KMS; [`implicit_scanout_modifier`] is what the renderer
//! imports with. See `src/render_helpers/vulkan/renderer.rs::import_dmabuf_target`.

use std::io;
use std::ops::Deref;

use smithay::backend::allocator::gbm::{GbmAllocator, GbmBuffer};
use smithay::backend::allocator::{Allocator, Buffer as _, Fourcc, Modifier};
use smithay::backend::drm::DrmDeviceFd;

/// The layout an implicitly-allocated scanout buffer must have for us to import it.
///
/// Every gbm driver we can drive this way allocates linear when asked without modifiers; this
/// constant is the single place that assumption is written down, and
/// [`ScanoutAllocator::create_buffer`] proves it per buffer instead of trusting it.
pub const IMPLICIT_SCANOUT_MODIFIER: Modifier = Modifier::Linear;

/// Wraps [`GbmAllocator`] to validate the layout of implicit-modifier allocations.
///
/// Explicit-modifier allocations pass straight through: the modifier survives into the exported
/// [`Dmabuf`](smithay::backend::allocator::dmabuf::Dmabuf) and the renderer imports it as-is.
#[derive(Debug, Clone)]
pub struct ScanoutAllocator {
    inner: GbmAllocator<DrmDeviceFd>,
}

impl ScanoutAllocator {
    pub fn new(inner: GbmAllocator<DrmDeviceFd>) -> Self {
        Self { inner }
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
        let buffer = self.inner.create_buffer(width, height, fourcc, modifiers)?;

        // `GbmAllocator` reports `Invalid` for anything it allocated through gbm's pre-modifier
        // API — either because we asked for `Invalid` (implicit-only KMS) or because an explicit
        // allocation failed and it retried legacy. Both land here, and both need the check.
        if buffer.format().modifier == Modifier::Invalid {
            // `GbmBuffer` derefs to the `gbm::BufferObject`, whose `modifier()` is the layout gbm
            // really chose — not the masked value `Buffer::format` reports.
            check_implicit_layout(fourcc, Deref::deref(&buffer).modifier())?;
        }

        Ok(buffer)
    }
}

/// Whether a buffer gbm allocated implicitly is one the renderer can import as
/// [`IMPLICIT_SCANOUT_MODIFIER`].
///
/// Split out from [`ScanoutAllocator::create_buffer`] so the rule is testable without a gbm
/// device: everything else in that path needs real hardware.
fn check_implicit_layout(fourcc: Fourcc, real: Modifier) -> Result<(), io::Error> {
    if real == IMPLICIT_SCANOUT_MODIFIER {
        return Ok(());
    }

    Err(io::Error::other(format!(
        "gbm allocated an implicit {fourcc:?} scanout buffer with modifier {real:?}, but only \
         {IMPLICIT_SCANOUT_MODIFIER:?} can be imported into Vulkan when KMS names no modifier"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn implicit_linear_is_importable() {
        assert!(check_implicit_layout(Fourcc::Xrgb8888, Modifier::Linear).is_ok());
    }

    #[test]
    fn implicit_tiled_is_refused() {
        // The failure this whole module exists to prevent: gbm picked a tiled layout for an
        // implicit allocation, and importing it as linear would scan out garbage.
        let err = check_implicit_layout(Fourcc::Xrgb8888, Modifier::I915_x_tiled).unwrap_err();
        assert!(err.to_string().contains("I915_x_tiled"), "{err}");
    }

    #[test]
    fn implicit_staying_invalid_is_refused() {
        // gbm reporting INVALID back means it has nothing to tell us; there is no layout to name
        // in the Vulkan import, so this must not be waved through as linear.
        assert!(check_implicit_layout(Fourcc::Xrgb8888, Modifier::Invalid).is_err());
    }
}
