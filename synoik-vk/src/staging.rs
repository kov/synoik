// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Staging for uploads: a shared per-frame arena ([`StagingPool`]) for everything the render
//! thread uploads, and a one-shot buffer ([`HostStaging`]) that can be created and **filled off
//! the render thread**.
//!
//! Both exist because of the same measurement. A copy into a mapping whose pages have never been
//! touched runs at ~7 GB/s on this VM's Venus device against ~58 GB/s into the same buffer once
//! warm (`docs/fork/foundation.md` §5), and on Venus a `HOST_VISIBLE` buffer is a host blob whose
//! creation and mapping are round trips. So the render thread's uploads share one warm buffer and
//! rewind it per frame rather than each making its own — see [`StagingPool`], which also records
//! what happened when they did not.
//!
//! [`HostStaging`] is the exception, and deliberately un-pooled: it serves the wallpaper, which is
//! tens of megabytes, decoded on a worker, and loaded once in a session rather than once a frame.
//! Pooling a buffer that size would pin the peak forever to save a fault cost that is not on the
//! compositor thread to begin with.
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
                    crate::devmem::untrack(memory);
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
            crate::devmem::untrack(self.memory);
            self.gpu.device.free_memory(self.memory, None);
        }
    }
}

/// Smallest chunk the pool allocates. A chunk holds a whole frame's uploads, so this is sized to
/// cover the ordinary case — a handful of shm surfaces at panel/menu sizes — in one allocation.
const MIN_CHUNK: vk::DeviceSize = 4 << 20;

/// Above this an upload gets a chunk sized to itself rather than sharing the ordinary arena, and
/// that chunk is retired once it goes unused for [`OVERSIZED_IDLE_FRAMES`].
///
/// It used to mean something stronger — such a chunk was created, used once and freed, never
/// joining the pool — on the reasoning that grow-only is the wrong shape for a 48 MiB wallpaper,
/// which would pin the peak for the life of the session. True for a wallpaper, and wrong for the
/// case that turned up in practice: a HiDPI **shm** client re-uploading tens of MiB on *every*
/// commit re-created its chunk every frame. That is the same per-commit mappable-blob churn this
/// whole type exists to stop (see [`StagingPool`], and `texture.rs`'s note on the session it
/// killed), and it cost twice over — the create/allocate/map round trips, and then a memcpy into
/// pages the host had never touched, which this VM serves at ~5 GB/s against ~56 GB/s warm.
///
/// So size decides the chunk's *shape*, and idleness decides its *lifetime*. A streaming client
/// keeps its chunk warm; a wallpaper's is handed back a second later.
const MAX_POOLED_CHUNK: vk::DeviceSize = 16 << 20;

/// How long an oversized chunk survives without being used, in frames. At 60 Hz this is about a
/// second — long enough that a client which merely paused between commits keeps its warm mapping,
/// short enough that a one-off upload's peak is not still held when the next one arrives.
const OVERSIZED_IDLE_FRAMES: u32 = 60;

/// Oversized chunk capacities are rounded up to a multiple of this, so a client whose buffer grows
/// by a few rows — a window being resized — reuses its chunk instead of allocating a new one and
/// leaving the old to idle out.
const OVERSIZED_GRANULARITY: vk::DeviceSize = 4 << 20;

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
    /// creates the bo (`docs/fork/foundation.md` §5) — a round trip the per-upload staging paid
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
                    crate::devmem::untrack(memory);
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

    /// Hand `fill` the mapped bytes at `offset..offset + len` to write in place.
    ///
    /// This exists so a producer whose pixels are not already a contiguous slice — an shm pool with
    /// a stride, rows to be repacked — writes them **once**, straight into the mapping, instead of
    /// building a `Vec` first and copying that in. On a client shipping tens of MiB per commit the
    /// intermediate was the larger half of the cost: a fresh allocation and a full copy into
    /// never-touched pages, on the compositor thread, per frame.
    ///
    /// # Safety
    /// `offset + len` must be within `capacity` and no GPU read of that range may be in flight —
    /// [`StagingPool::stage_with`] is the only caller and establishes both, bump-allocating the
    /// range and only rewinding a chunk nothing else references. Additionally, `fill` must write
    /// **every** byte of the slice and must not read from it. The mapping is device memory whose
    /// contents are whatever the last upload left (or nothing at all, for a fresh chunk), so a
    /// byte `fill` skips is uploaded as garbage rather than as zero.
    unsafe fn fill_at(&self, offset: vk::DeviceSize, len: usize, fill: impl FnOnce(&mut [u8])) {
        let _timed = crate::stats::staging_write();
        // SAFETY: the caller guarantees the range is inside the mapping and unread by the GPU; the
        // mapping outlives the borrow, which does not escape `fill`.
        let dst = unsafe { std::slice::from_raw_parts_mut(self.ptr.add(offset as usize), len) };
        fill(dst);
    }
}

impl Drop for StagingChunk {
    fn drop(&mut self) {
        unsafe {
            self.gpu.device.unmap_memory(self.memory);
            self.gpu.device.destroy_buffer(self.buffer, None);
            crate::devmem::untrack(self.memory);
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
    /// submit still holds the previous one, or while a client streams buffers too big to share.
    chunks: Vec<PooledChunk>,
    /// Index into `chunks` of the chunk being filled, and how much of it is spoken for.
    current: Option<usize>,
    used: vk::DeviceSize,
    align: vk::DeviceSize,
}

/// A chunk and how long it has gone unused, which is what [`StagingPool::end_frame`] retires
/// oversized chunks on.
struct PooledChunk {
    chunk: Arc<StagingChunk>,
    /// Frames since this chunk last served an upload. Ordinary chunks ignore it — they are small,
    /// and keeping one costs less than deciding not to.
    idle_frames: u32,
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
        self.stage_with(gpu, data.len() as vk::DeviceSize, |dst| {
            dst.copy_from_slice(data)
        })
    }

    /// Reserve `len` bytes and let `fill` write them straight into the mapping.
    ///
    /// The general form of [`Self::stage`], for a producer whose bytes are not already contiguous:
    /// an shm pool with a stride repacks its rows directly here rather than into a `Vec` that is
    /// then copied in. `fill` must write every byte and must not read them — see
    /// [`StagingChunk::fill_at`].
    pub fn stage_with(
        &mut self,
        gpu: &Arc<Gpu>,
        len: vk::DeviceSize,
        fill: impl FnOnce(&mut [u8]),
    ) -> Result<(Arc<StagingChunk>, vk::DeviceSize)> {
        assert!(len > 0, "staging an empty upload");

        // Rewind: the chunk we were filling is free the moment nothing else references it — no
        // queued upload, no in-flight record — so the GPU cannot be reading it either. This is the
        // steady state, and it is why one chunk serves a whole session.
        if let Some(index) = self.current {
            if Arc::strong_count(&self.chunks[index].chunk) == 1 {
                self.used = 0;
            }
        }

        // An upload too big to share sits alone in a chunk sized to it, so it never partly fills
        // one the next upload then has to grow past. It still joins the pool: a client that ships
        // one of these per commit needs its mapping *kept*, and `end_frame` is what hands it back
        // when the client stops.
        let oversized = len > MAX_POOLED_CHUNK;
        let fits = |chunk: &PooledChunk, used: vk::DeviceSize| {
            if oversized {
                chunk.chunk.capacity() >= len
            } else {
                used + len <= chunk.chunk.capacity()
            }
        };

        let index = match self.current {
            Some(index) if !oversized && fits(&self.chunks[index], self.used) => index,
            // The current chunk is full (or there isn't one, or this upload wants one to itself)
            // while an earlier frame still holds it: take a free chunk that fits, or make one.
            _ => {
                let free = self
                    .chunks
                    .iter()
                    .position(|chunk| Arc::strong_count(&chunk.chunk) == 1 && fits(chunk, 0));
                let index = match free {
                    Some(index) => index,
                    None => {
                        self.sweep();
                        let capacity = if oversized {
                            len.next_multiple_of(OVERSIZED_GRANULARITY)
                        } else {
                            len.max(MIN_CHUNK)
                        };
                        let chunk = StagingChunk::new(gpu, capacity)?;
                        self.chunks.push(PooledChunk {
                            chunk: Arc::new(chunk),
                            idle_frames: 0,
                        });
                        self.chunks.len() - 1
                    }
                };
                self.used = 0;
                self.current = Some(index);
                index
            }
        };

        let offset = self.used;
        let entry = &mut self.chunks[index];
        entry.idle_frames = 0;
        // SAFETY: `offset + len` is within capacity — either bump-allocated inside the current
        // chunk or offset 0 of one sized to fit — and the chunk is either freshly created or one
        // nothing else references, so no GPU read of the range is in flight.
        unsafe { entry.chunk.fill_at(offset, len as usize, fill) };
        // Bump past this upload, aligned for the next `bufferOffset`. Saturating at the capacity
        // keeps the arithmetic honest when the last upload ends flush with the end of the chunk.
        self.used = (offset + len)
            .next_multiple_of(self.align)
            .min(entry.chunk.capacity());
        Ok((entry.chunk.clone(), offset))
    }

    /// Age the pool by one frame, retiring oversized chunks nothing has wanted for
    /// [`OVERSIZED_IDLE_FRAMES`].
    ///
    /// Called once per frame from the render path. Without it an oversized chunk would be pinned
    /// for the session — the very thing that made these dedicated-and-freed in the first place —
    /// and with it a client that streams them keeps its mapping warm for as long as it is
    /// streaming, which is the whole point.
    pub fn end_frame(&mut self) {
        let mut retired = false;
        for entry in &mut self.chunks {
            entry.idle_frames = entry.idle_frames.saturating_add(1);
        }
        let current = self
            .current
            .map(|index| Arc::as_ptr(&self.chunks[index].chunk));
        self.chunks.retain(|entry| {
            let keep = entry.chunk.capacity() <= MAX_POOLED_CHUNK
                || entry.idle_frames <= OVERSIZED_IDLE_FRAMES
                || Arc::strong_count(&entry.chunk) > 1;
            retired |= !keep;
            keep
        });
        if retired {
            self.reindex(current);
        }
    }

    /// Drop free chunks once the pool has grown past [`MAX_POOLED_CHUNKS`], before adding another.
    /// A chunk still referenced by an in-flight upload is never touched — dropping it is not the
    /// pool's call, and cannot be: `Arc` decides.
    fn sweep(&mut self) {
        if self.chunks.len() < MAX_POOLED_CHUNKS {
            return;
        }
        let current = self
            .current
            .map(|index| Arc::as_ptr(&self.chunks[index].chunk));
        self.chunks.retain(|entry| {
            Arc::strong_count(&entry.chunk) > 1 || Some(Arc::as_ptr(&entry.chunk)) == current
        });
        self.reindex(current);
    }

    /// Re-find `current` after a retain moved the indices, and forget the fill offset if the chunk
    /// it referred to is gone — a stale `used` against a different chunk would hand out an offset
    /// into the middle of someone else's bytes.
    fn reindex(&mut self, current: Option<*const StagingChunk>) {
        self.current = current.and_then(|ptr| {
            self.chunks
                .iter()
                .position(|entry| Arc::as_ptr(&entry.chunk) == ptr)
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

    /// An upload too big to share gets a chunk of its own, and keeps it while it is still being
    /// used — a streaming client's mapping must stay warm.
    ///
    /// This replaces a rule that said the opposite: such a chunk used to be created, used once and
    /// freed, so that a 48 MiB wallpaper could not pin its peak for the session. That was right
    /// about the wallpaper and wrong about a HiDPI shm client, which re-uploads tens of MiB on
    /// *every* commit and so re-created its chunk every frame — the per-commit mappable-blob churn
    /// [`a_pool_rewinds_instead_of_allocating`] exists to forbid, plus a memcpy into cold pages at
    /// a tenth of the warm rate. Lifetime is now decided by idleness instead; see
    /// [`a_pool_retires_an_oversized_chunk_that_goes_idle`] for the wallpaper half.
    #[test]
    fn a_pool_keeps_an_oversized_chunk_a_client_keeps_using() {
        let Ok(gpu) = Gpu::new() else {
            eprintln!(
                "skipping a_pool_keeps_an_oversized_chunk_a_client_keeps_using: no Vulkan device"
            );
            return;
        };
        let gpu = Arc::new(gpu);
        let mut pool = StagingPool::new(&gpu);

        let huge = vec![0u8; (MAX_POOLED_CHUNK + 4096) as usize];
        let (chunk, offset) = pool.stage(&gpu, &huge).expect("stage");
        assert_eq!(offset, 0);
        assert!(
            chunk.capacity() >= huge.len() as vk::DeviceSize,
            "sized to the upload",
        );
        assert_eq!(pool.chunk_count(), 1, "the pool must retain it");
        let first = chunk.buffer();
        drop(chunk);

        // A client committing every frame must land in the same buffer every time — that is what
        // keeps the mapping warm, and what stops the per-commit blob churn.
        for frame in 0..8 {
            pool.end_frame();
            let (chunk, offset) = pool.stage(&gpu, &huge).expect("stage");
            assert_eq!(offset, 0);
            assert_eq!(chunk.buffer(), first, "frame {frame} allocated a new chunk");
            assert_eq!(pool.chunk_count(), 1, "frame {frame} grew the pool");
        }
    }

    /// ...and gives it back once nothing wants it, so a one-off upload does not pin its peak.
    #[test]
    fn a_pool_retires_an_oversized_chunk_that_goes_idle() {
        let Ok(gpu) = Gpu::new() else {
            eprintln!(
                "skipping a_pool_retires_an_oversized_chunk_that_goes_idle: no Vulkan device"
            );
            return;
        };
        let gpu = Arc::new(gpu);
        let mut pool = StagingPool::new(&gpu);

        let huge = vec![0u8; (MAX_POOLED_CHUNK + 4096) as usize];
        let (chunk, _) = pool.stage(&gpu, &huge).expect("stage");
        assert_eq!(pool.chunk_count(), 1);

        // While the upload is still in flight the chunk is not the pool's to drop, however idle.
        for _ in 0..=OVERSIZED_IDLE_FRAMES + 1 {
            pool.end_frame();
        }
        assert_eq!(
            pool.chunk_count(),
            1,
            "a chunk an in-flight upload still reads must survive its idle window",
        );

        drop(chunk);
        for _ in 0..=OVERSIZED_IDLE_FRAMES {
            pool.end_frame();
        }
        assert_eq!(
            pool.chunk_count(),
            0,
            "an idle oversized chunk must be retired"
        );

        // An ordinary upload afterwards still gets a pooled chunk, and keeps it: only oversized
        // chunks are worth retiring.
        let (_chunk, _) = pool.stage(&gpu, &[0u8; 4096]).expect("stage");
        assert_eq!(pool.chunk_count(), 1);
        for _ in 0..=OVERSIZED_IDLE_FRAMES * 2 {
            pool.end_frame();
        }
        assert_eq!(
            pool.chunk_count(),
            1,
            "an ordinary chunk is never retired on idleness"
        );
    }

    /// `stage_with` writes through to the same bytes `stage` would have, and does it in place.
    ///
    /// The shm path's whole reason for existing: the pixels reach the mapping without a `Vec` in
    /// between. If the fill were handed a scratch buffer that was then copied, this would still
    /// pass — so it also pins that a *partial* fill leaves the rest of the range alone, which is
    /// what makes "the callback must write every byte" a real contract rather than a wish.
    #[test]
    fn stage_with_fills_the_mapping_in_place() {
        let Ok(gpu) = Gpu::new() else {
            eprintln!("skipping stage_with_fills_the_mapping_in_place: no Vulkan device");
            return;
        };
        let gpu = Arc::new(gpu);
        let mut pool = StagingPool::new(&gpu);

        let (chunk, offset) = pool
            .stage_with(&gpu, 8, |dst| {
                assert_eq!(dst.len(), 8, "the fill sees exactly what it reserved");
                dst.copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
            })
            .expect("stage_with");
        // SAFETY: the mapping is live for the chunk's life and this range was just written.
        let written = unsafe { std::slice::from_raw_parts(chunk.ptr.add(offset as usize), 8) };
        assert_eq!(written, &[1, 2, 3, 4, 5, 6, 7, 8]);
    }
}
