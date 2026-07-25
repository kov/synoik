//! A host-visible staging buffer that can be created and **filled off the render thread**.
//!
//! The normal upload path ([`crate::texture::Texture::from_bytes_32bpp`]) takes a `&[u8]` the
//! caller already owns and copies it into a staging buffer it makes on the spot. That copy is a
//! host write into write-combined memory — measured at ~5.6 GB/s on this VM's Venus device against
//! 13.6 GB/s for a plain heap — and for a 4K wallpaper it is tens of megabytes, so it lands on the
//! compositor thread as a single 7–9 ms stall with no GPU work in it at all.
//!
//! Nothing about that copy needs the render thread. Buffer creation, memory allocation, mapping
//! and the write itself are all `VkDevice`-level calls the spec internally synchronizes; only
//! queue submission is externally synchronized, and this type never submits. So a worker that
//! already produces the pixels (the wallpaper decoder) can write them straight into device-visible
//! memory, and the render thread is left with just the image creation, the copy command and the
//! submit.
//!
//! What the type does **not** do is give up ownership of the device: it holds an `Arc<Gpu>`, so the
//! device outlives the buffer even if the renderer is torn down while a decode is in flight. What
//! that cannot fix is *relevance* — a staging buffer belonging to a device that has since been
//! replaced is useless, not unsafe, and the consumer is expected to check
//! [`HostStaging::belongs_to`] before uploading from it.

use std::sync::Arc;

use anyhow::{Context, Result};
use ash::vk;

use crate::gpu::Gpu;

/// A mapped, host-visible `TRANSFER_SRC` buffer. Created on any thread, written to on any thread,
/// consumed by an upload on the render thread.
pub struct HostStaging {
    gpu: Arc<Gpu>,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    /// Mapped for the buffer's whole life. Mapping is itself a host round trip on a virtualized
    /// driver, so it is done once, on the thread that fills the buffer, rather than again at
    /// upload time.
    ptr: *mut u8,
    size: usize,
}

// SAFETY: every handle here is owned exclusively by this value — the buffer and its memory are
// created for it and destroyed with it, and `ptr` is a mapping of that memory alone, so no other
// thread can observe them. The Vulkan calls this type makes (`vkCreateBuffer`,
// `vkAllocateMemory`, `vkBindBufferMemory`, `vkMapMemory`, `vkUnmapMemory`, `vkDestroyBuffer`,
// `vkFreeMemory`) all take `VkDevice` as their externally-synchronized-free parameter, so they are
// safe to make from a thread other than the renderer's. It deliberately provides no way to submit.
unsafe impl Send for HostStaging {}

impl HostStaging {
    /// Allocate `size` bytes of `HOST_VISIBLE | HOST_COHERENT` memory bound to a `TRANSFER_SRC`
    /// buffer, and map it. `size` must be non-zero.
    pub fn new(gpu: &Arc<Gpu>, size: usize) -> Result<Self> {
        assert!(size > 0, "zero-sized staging buffer");
        let device = &gpu.device;
        let ci = vk::BufferCreateInfo::default()
            .size(size as vk::DeviceSize)
            .usage(vk::BufferUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);

        // Hand-rolled rather than reusing `UploadGuard`: that guard is tied to the upload path's
        // six handles, and there are only two here, each unwound on the one `?` that can follow it.
        let buffer = unsafe { device.create_buffer(&ci, None) }.context("host staging buffer")?;
        let req = unsafe { device.get_buffer_memory_requirements(buffer) };
        let memory = match gpu.allocate(
            req,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        ) {
            Ok(memory) => memory,
            Err(err) => {
                unsafe { device.destroy_buffer(buffer, None) };
                return Err(err);
            }
        };
        let ptr = unsafe {
            match device
                .bind_buffer_memory(buffer, memory, 0)
                .context("bind host staging")
                .and_then(|()| {
                    device
                        .map_memory(
                            memory,
                            0,
                            size as vk::DeviceSize,
                            vk::MemoryMapFlags::empty(),
                        )
                        .context("map host staging")
                }) {
                Ok(ptr) => ptr as *mut u8,
                Err(err) => {
                    device.destroy_buffer(buffer, None);
                    device.free_memory(memory, None);
                    return Err(err);
                }
            }
        };

        Ok(Self {
            gpu: gpu.clone(),
            buffer,
            memory,
            ptr,
            size,
        })
    }

    /// The mapped bytes, to be written by whichever thread produced them. `HOST_COHERENT`, so no
    /// flush is needed before the GPU reads it.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: `ptr` maps exactly `size` bytes of memory this value owns, and `&mut self`
        // excludes any concurrent reader.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.size) }
    }

    pub fn len(&self) -> usize {
        self.size
    }

    pub fn is_empty(&self) -> bool {
        self.size == 0
    }

    pub(crate) fn buffer(&self) -> vk::Buffer {
        self.buffer
    }

    /// Whether this buffer was allocated on `gpu`. A device that has been replaced (device loss,
    /// a renderer rebuilt for a new node) leaves any staging made against the old one *valid but
    /// useless*: uploading from it would mean copying between two devices, which is not a thing.
    /// The `Arc<Gpu>` here keeps the old device alive, so the failure is a wasted decode rather
    /// than a use-after-free — but the caller still has to notice.
    pub fn belongs_to(&self, gpu: &Arc<Gpu>) -> bool {
        Arc::ptr_eq(&self.gpu, gpu)
    }
}

impl Drop for HostStaging {
    fn drop(&mut self) {
        unsafe {
            self.gpu.device.unmap_memory(self.memory);
            self.gpu.device.destroy_buffer(self.buffer, None);
            self.gpu.device.free_memory(self.memory, None);
        }
    }
}
