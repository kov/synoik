//! A host-visible staging buffer that can be created and **filled off the render thread**.
//!
//! The normal upload path ([`crate::texture::Texture::from_bytes_32bpp`]) takes a `&[u8]` the
//! caller already owns and copies it into a staging buffer it makes on the spot. That copy is a
//! host write into a mapping whose pages have never been touched — measured at ~7 GB/s on this
//! VM's Venus device against ~58 GB/s into the same buffer once warm (`docs/fork/venus-cost.md`
//! §9.2) — and for a 4K wallpaper it is tens of megabytes, so it lands on the compositor thread as
//! a single 7–9 ms stall with no GPU work in it at all. TODO(perf): reusing buffers across uploads
//! would remove the fault cost entirely; each `HostStaging` is created and dropped per batch.
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
// SAFETY: shared access only ever reads `buffer` (a handle) and `size`; the mapping is written
// through `&mut self`, which a shared reference excludes. Needed because a staged upload holds the
// buffer in an `Arc` until its copy has been submitted, and that `Arc` travels with the frame.
unsafe impl Sync for HostStaging {}

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

    /// The device this buffer belongs to, for recording the copy that reads it.
    pub(crate) fn device(&self) -> &ash::Device {
        &self.gpu.device
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

/// Smallest chunk the pool allocates. A chunk holds a whole frame's uploads, so this is sized to
/// cover the ordinary case — a handful of shm surfaces at panel/menu sizes — in one allocation.
const MIN_CHUNK: vk::DeviceSize = 4 << 20;

/// Uploads larger than this get a chunk of their own, which dies with them instead of joining the
/// pool. Grow-only is the right shape for a per-frame arena and the wrong one for a 48 MiB
/// wallpaper: pooling that would pin the peak for the life of the session.
const MAX_POOLED_CHUNK: vk::DeviceSize = 16 << 20;

/// How many chunks the pool keeps. It only needs more than one while an earlier frame's submit is
/// still reading the previous chunk, so this is already generous; past it, free chunks are dropped
/// rather than kept.
const MAX_POOLED_CHUNKS: usize = 4;

/// One host-visible `TRANSFER_SRC` buffer that several staged uploads share, each at its own
/// offset. Mapped once, for its whole life.
///
/// Handed out as an `Arc` by [`StagingPool`], and that reference count *is* the recycling
/// mechanism: a chunk is safe to write from the start again exactly when nothing else holds
/// it — no staged upload, no in-flight submit — and there is no other way to know, since the pool
/// never sees a fence.
pub struct StagingChunk {
    gpu: Arc<Gpu>,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    /// Mapped for the chunk's whole life. On a virtualized driver `vkMapMemory` is where the host
    /// creates the bo (`docs/fork/venus-cost.md` §9.2) — a round trip the per-upload staging paid
    /// on every client commit.
    ptr: *mut u8,
    capacity: vk::DeviceSize,
}

// SAFETY: the same argument as `HostStaging` above — every handle is owned exclusively by this
// value and `ptr` maps only its own memory. Writes go through `&mut StagingPool`, so the pool's
// borrow is what serializes them; a shared `Arc<StagingChunk>` only ever reads `buffer`.
unsafe impl Send for StagingChunk {}
unsafe impl Sync for StagingChunk {}

impl StagingChunk {
    fn new(gpu: &Arc<Gpu>, capacity: vk::DeviceSize) -> Result<Self> {
        let device = &gpu.device;
        let _timed = crate::stats::creating();
        let ci = vk::BufferCreateInfo::default()
            .size(capacity)
            .usage(vk::BufferUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let buffer = unsafe { device.create_buffer(&ci, None) }.context("staging chunk")?;
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
                .context("bind staging chunk")
                .and_then(|()| {
                    device
                        .map_memory(memory, 0, capacity, vk::MemoryMapFlags::empty())
                        .context("map staging chunk")
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
            capacity,
        })
    }

    /// The buffer a `VkBufferImageCopy` reads from. Paired with the offset [`StagingPool::stage`]
    /// returned.
    pub fn buffer(&self) -> vk::Buffer {
        self.buffer
    }

    pub fn capacity(&self) -> vk::DeviceSize {
        self.capacity
    }

    /// The device the chunk belongs to, for recording the copy that reads it.
    pub fn device(&self) -> &ash::Device {
        &self.gpu.device
    }

    /// Copy `data` in at `offset`.
    ///
    /// # Safety
    /// `offset + data.len()` must be within `capacity`, and no GPU read of that range may be in
    /// flight. [`StagingPool::stage`] is the only caller and establishes both: it bump-allocates
    /// the range and only rewinds a chunk nothing else references.
    unsafe fn write_at(&self, offset: vk::DeviceSize, data: &[u8]) {
        let _timed = crate::stats::staging_write();
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), self.ptr.add(offset as usize), data.len());
        }
    }
}

impl Drop for StagingChunk {
    fn drop(&mut self) {
        unsafe {
            self.gpu.device.unmap_memory(self.memory);
            self.gpu.device.destroy_buffer(self.buffer, None);
            self.gpu.device.free_memory(self.memory, None);
        }
    }
}

/// The renderer's arena for staged uploads: **one** grow-only buffer, N offsets, rewound per frame.
///
/// Deferring a copy into the frame's command buffer means the staging it reads from must live
/// until that submit retires, so the obvious implementation gives every upload a buffer of its
/// own. On Venus a `HOST_VISIBLE` buffer is a virtio-gpu blob, and an shm re-upload happens on
/// *every commit of every shm surface* — that is a fresh mappable blob per client frame, forever.
/// It exhausted the host's blob pool two minutes into a live session, after which every
/// `vkAllocateMemory` returned `ERROR_OUT_OF_HOST_MEMORY` and the session did not recover.
///
/// So the buffer is shared and reused instead, which also drops the per-upload
/// create/allocate/bind/map/unmap — five host round trips — to a `memcpy` into a mapping that is
/// already warm.
///
/// Reuse is decided by the reference count, not by a fence: an upload holds its chunk from staging
/// until its command buffer retires, so a chunk nobody else holds cannot be one the GPU is reading.
/// Sharing means the pool cannot know when an individual upload is done — only when *all* of them
/// are — which is exactly what rewinding a whole chunk needs.
pub struct StagingPool {
    /// Every chunk the pool owns. Small: one in the steady state, more only while an in-flight
    /// submit still holds the previous one.
    chunks: Vec<Arc<StagingChunk>>,
    /// Index into `chunks` of the chunk being filled, and how much of it is spoken for.
    current: Option<usize>,
    used: vk::DeviceSize,
    align: vk::DeviceSize,
}

impl StagingPool {
    pub fn new(gpu: &Gpu) -> Self {
        Self {
            chunks: Vec::new(),
            current: None,
            used: 0,
            align: gpu.buffer_copy_offset_alignment,
        }
    }

    /// Copy `data` into the pool and return the chunk it landed in, plus the offset to hand a
    /// `VkBufferImageCopy`. The chunk must be held until the copy's submit retires — that is what
    /// keeps the bytes alive, and what tells the pool the space is still spoken for.
    pub fn stage(
        &mut self,
        gpu: &Arc<Gpu>,
        data: &[u8],
    ) -> Result<(Arc<StagingChunk>, vk::DeviceSize)> {
        let len = data.len() as vk::DeviceSize;
        assert!(len > 0, "staging an empty upload");

        // Too big to pool: a dedicated chunk, owned entirely by its upload and freed with it.
        if len > MAX_POOLED_CHUNK {
            let chunk = Arc::new(StagingChunk::new(gpu, len)?);
            unsafe { chunk.write_at(0, data) };
            return Ok((chunk, 0));
        }

        // Rewind: the chunk we were filling is free the moment nothing else references it — no
        // queued upload, no in-flight record — so the GPU cannot be reading it either. This is the
        // steady state, and it is why one chunk serves a whole session.
        if let Some(index) = self.current {
            if Arc::strong_count(&self.chunks[index]) == 1 {
                self.used = 0;
            }
        }

        let index = match self.current {
            Some(index) if self.used + len <= self.chunks[index].capacity() => index,
            // The current chunk is full (or there isn't one) while an earlier frame still holds
            // it: take a free chunk that fits, or make one.
            _ => {
                let free = self
                    .chunks
                    .iter()
                    .position(|chunk| Arc::strong_count(chunk) == 1 && chunk.capacity() >= len);
                let index = match free {
                    Some(index) => index,
                    None => {
                        self.sweep();
                        let chunk = StagingChunk::new(gpu, len.max(MIN_CHUNK))?;
                        self.chunks.push(Arc::new(chunk));
                        self.chunks.len() - 1
                    }
                };
                self.used = 0;
                self.current = Some(index);
                index
            }
        };

        let offset = self.used;
        unsafe { self.chunks[index].write_at(offset, data) };
        // Bump past this upload, aligned for the next `bufferOffset`. Saturating at the capacity
        // keeps the arithmetic honest when the last upload ends flush with the end of the chunk.
        self.used = (offset + len)
            .next_multiple_of(self.align)
            .min(self.chunks[index].capacity());
        Ok((self.chunks[index].clone(), offset))
    }

    /// Drop free chunks once the pool has grown past [`MAX_POOLED_CHUNKS`], before adding another.
    /// A chunk still referenced by an in-flight upload is never touched — dropping it is not the
    /// pool's call, and cannot be: `Arc` decides.
    fn sweep(&mut self) {
        if self.chunks.len() < MAX_POOLED_CHUNKS {
            return;
        }
        let current = self.current.map(|index| Arc::as_ptr(&self.chunks[index]));
        self.chunks
            .retain(|chunk| Arc::strong_count(chunk) > 1 || Some(Arc::as_ptr(chunk)) == current);
        // The indices just moved.
        self.current = current.and_then(|ptr| {
            self.chunks
                .iter()
                .position(|chunk| Arc::as_ptr(chunk) == ptr)
        });
        if self.current.is_none() {
            self.used = 0;
        }
    }

    /// Chunks currently owned by the pool. For the tests that pin "N uploads, one allocation".
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A whole session's uploads must live in **one** buffer.
    ///
    /// This is the invariant the first deferred-upload attempt broke: it gave every staged upload
    /// a staging buffer of its own, which on Venus is a mappable virtio-gpu blob, and an shm
    /// re-upload happens on every commit of every shm surface. The host ran out of blobs two
    /// minutes into a live session and never recovered. Dropping each staged upload before the
    /// next is what a frame does when its submit retires, and it is what lets the pool rewind.
    #[test]
    fn a_pool_rewinds_instead_of_allocating() {
        let Ok(gpu) = Gpu::new() else {
            eprintln!("skipping a_pool_rewinds_instead_of_allocating: no Vulkan device");
            return;
        };
        let gpu = Arc::new(gpu);
        let mut pool = StagingPool::new(&gpu);

        // 500 x 64 KiB is 32 MiB — eight times a chunk — so a pool that never rewound would have
        // to allocate, and it does not matter which of its growth paths it took.
        let data = vec![0u8; 64 << 10];
        for _ in 0..500 {
            let (chunk, offset) = pool.stage(&gpu, &data).expect("stage");
            assert_eq!(
                offset, 0,
                "with nothing else live, every upload starts the chunk over"
            );
            drop(chunk);
        }
        assert_eq!(pool.chunk_count(), 1, "one buffer served every upload");
    }

    /// Uploads that are alive *at the same time* — a frame's worth — must not land on top of each
    /// other. Rewinding is safe only once nothing references the chunk, and the reference count is
    /// what says so, since the pool never sees a fence.
    #[test]
    fn a_pool_packs_live_uploads_side_by_side() {
        let Ok(gpu) = Gpu::new() else {
            eprintln!("skipping a_pool_packs_live_uploads_side_by_side: no Vulkan device");
            return;
        };
        let gpu = Arc::new(gpu);
        let mut pool = StagingPool::new(&gpu);

        let data = vec![0u8; 4096];
        let live: Vec<_> = (0..8)
            .map(|_| pool.stage(&gpu, &data).expect("stage"))
            .collect();

        assert_eq!(pool.chunk_count(), 1, "a frame's uploads share one buffer");
        let mut seen: Vec<vk::DeviceSize> = Vec::new();
        for (chunk, offset) in &live {
            assert!(
                seen.iter()
                    .all(|other| offset.abs_diff(*other) >= data.len() as vk::DeviceSize),
                "two live uploads overlap in the staging: {offset} against {seen:?}",
            );
            assert!(offset + data.len() as vk::DeviceSize <= chunk.capacity());
            seen.push(*offset);
        }

        // The frame retired: the chunk is free, and the next upload starts from the top again.
        drop(live);
        let (_chunk, offset) = pool.stage(&gpu, &data).expect("stage");
        assert_eq!(offset, 0, "a chunk nothing references any more must rewind");
        assert_eq!(pool.chunk_count(), 1);
    }

    /// An upload too big to pool gets a chunk of its own, and that chunk leaves with it.
    ///
    /// Grow-only is right for a per-frame arena and wrong for a wallpaper: pooling a 48 MiB upload
    /// would pin its peak for the life of the session, for a buffer nothing that size will use
    /// again.
    #[test]
    fn a_pool_does_not_keep_oversized_chunks() {
        let Ok(gpu) = Gpu::new() else {
            eprintln!("skipping a_pool_does_not_keep_oversized_chunks: no Vulkan device");
            return;
        };
        let gpu = Arc::new(gpu);
        let mut pool = StagingPool::new(&gpu);

        let huge = vec![0u8; (MAX_POOLED_CHUNK + 4096) as usize];
        let (chunk, offset) = pool.stage(&gpu, &huge).expect("stage");
        assert_eq!(offset, 0);
        assert_eq!(
            chunk.capacity(),
            huge.len() as vk::DeviceSize,
            "sized to the upload"
        );
        assert_eq!(pool.chunk_count(), 0, "the pool must not retain it");
        drop(chunk);

        // And an ordinary upload afterwards still gets a pooled chunk.
        let (_chunk, _) = pool.stage(&gpu, &[0u8; 4096]).expect("stage");
        assert_eq!(pool.chunk_count(), 1);
    }
}
