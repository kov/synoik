//! A submit's completion, as something the rest of the stack can wait on or hand to KMS.
//!
//! Smithay's [`Fence`] is the interface a
//! [`SyncPoint`](smithay::backend::renderer::sync::SyncPoint) carries, and `DrmCompositor` already
//! knows what to do with one: if the plane has `IN_FENCE_FD` and the sync point can export, it
//! exports the fence and hands the FD to the atomic commit instead of asking the compositor to
//! block (`backend/drm/compositor/mod.rs` `build_planes`). Returning a real one here is what turns
//! our scanout submit from "the CPU parks for 12–14 ms" into "the kernel waits for the GPU while we
//! get on with the next frame". See `docs/fork/renderer-synchronous-submits.md`.
//!
//! The `VkFence` is created exportable (`Gpu::create_exportable_fence`) *before* the submit,
//! because a `SYNC_FD` fence's handle types are fixed at creation. Ownership is shared: the sync
//! point handed to KMS and the renderer's in-flight record both hold one, and the fence is
//! destroyed when the last of them lets go — which is never before the work completes, since the
//! in-flight record is only dropped once the queue timeline has passed the submit.

use std::os::unix::io::OwnedFd;
use std::sync::Arc;

use ash::vk;
use niri_vk::gpu::Gpu;
use smithay::backend::renderer::sync::{Fence, Interrupted};

/// The completion of one `vkQueueSubmit`. See the [module docs](self).
///
/// Cheap to clone, and shared on purpose: the same completion is both the sync point KMS holds
/// and the renderer's proof that a command buffer is still busy. The fence dies with the last
/// clone.
#[derive(Clone, Debug)]
pub struct VkSubmitFence {
    inner: Arc<Inner>,
}

struct Inner {
    /// Keeps the device alive: a sync point can outlive the renderer that made it (KMS holds one
    /// across frames), and destroying the fence needs the device it came from.
    gpu: Arc<Gpu>,
    fence: vk::Fence,
}

impl VkSubmitFence {
    /// Takes ownership of `fence`, which must have been created by
    /// [`Gpu::create_exportable_fence`] and submitted.
    pub fn new(gpu: Arc<Gpu>, fence: vk::Fence) -> Self {
        Self {
            inner: Arc::new(Inner { gpu, fence }),
        }
    }
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VkSubmitFence")
            .field("fence", &self.fence)
            .finish()
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Safe without a wait: the renderer's in-flight record holds a clone until the queue
        // timeline has passed this submit, so by the time the last one goes the work is complete.
        // Dropping early would otherwise be a `vkDestroyFence` on a pending submit.
        unsafe { self.gpu.device.destroy_fence(self.fence, None) };
    }
}

impl Fence for VkSubmitFence {
    fn is_signaled(&self) -> bool {
        // On error, say "not yet": a caller reading this is deciding whether it still has to
        // wait, and claiming completion we cannot confirm is the answer that corrupts pixels.
        unsafe { self.inner.gpu.device.get_fence_status(self.inner.fence) }.unwrap_or(false)
    }

    fn wait(&self) -> Result<(), Interrupted> {
        unsafe {
            self.inner.gpu.device.wait_for_fences(
                std::slice::from_ref(&self.inner.fence),
                true,
                u64::MAX,
            )
        }
        .map_err(|_| Interrupted)
    }

    fn is_exportable(&self) -> bool {
        // By construction — the fence only exists if `create_exportable_fence` made one.
        true
    }

    fn export(&self) -> Option<OwnedFd> {
        // Once per fence: a `SYNC_FD` export has copy transference and resets the fence. Smithay
        // memoizes the FD in the plane config, so it asks once.
        match self.inner.gpu.export_fence_sync_fd(self.inner.fence) {
            Ok(fd) => Some(fd),
            Err(err) => {
                tracing::warn!("could not export the frame's fence, KMS will have to block: {err}");
                None
            }
        }
    }
}
