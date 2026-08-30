// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! A sampled texture: device-local image uploaded from host RGBA via a staging buffer, plus a
//! view and sampler. This is the infrastructure both the blur passes and the glyph atlas reuse.

use std::os::fd::{BorrowedFd, FromRawFd, IntoRawFd, OwnedFd};
use std::sync::Arc;

use anyhow::{Context, Result};
use ash::vk;

use crate::gpu::Gpu;
use crate::render::{COLOR_RANGE, FORMAT, RENDER_FORMAT};
use crate::staging::{HostStaging, StagingChunk, StagingPool};

pub struct Texture {
    pub image: vk::Image,
    pub view: vk::ImageView,
    pub sampler: vk::Sampler,
    memory: vk::DeviceMemory,
    // Used by the blur (ping-pong sizing) and glyph-atlas milestones.
    #[allow(dead_code)]
    pub width: u32,
    #[allow(dead_code)]
    pub height: u32,
}

/// The dmabuf side of a [`Texture::allocate_scanout`] image: the exported handle plus the layout
/// KMS needs to be told about.
///
/// Single plane, because every format this compositor scans out is. The fd is owned — dropping it
/// closes it, which does **not** free the image's memory (the `VkDeviceMemory` owns that); it only
/// drops this process's reference to the exported handle.
#[derive(Debug)]
pub struct ScanoutExport {
    pub fd: OwnedFd,
    /// The modifier the *driver* chose out of the candidate list, read back with
    /// `vkGetImageDrmFormatModifierPropertiesEXT`.
    pub modifier: u64,
    pub offset: u32,
    pub stride: u32,
}

/// Unwind guard for [`Texture::upload`]: destroys every handle created so far if a later step
/// fails, so a mid-build error (e.g. a HOST_VISIBLE staging allocation failing under the Venus
/// mappable-blob pressure the shm cache targets) doesn't orphan the resource. Each field starts
/// null (`vkDestroy*`/`vkFree*` no-op on null) and is filled as `upload` progresses;
/// `staging`/`smem` are always freed (they never outlive `upload`), while the
/// image/memory/view/sampler that move into the finished `Texture` are nulled out of the guard
/// first so it leaves them alone. Mirrors the [`crate::blur::BlurChain`] `NewGuard` pattern.
struct UploadGuard<'a> {
    device: &'a ash::Device,
    staging: vk::Buffer,
    smem: vk::DeviceMemory,
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    sampler: vk::Sampler,
}

impl<'a> UploadGuard<'a> {
    fn new(device: &'a ash::Device) -> Self {
        Self {
            device,
            staging: vk::Buffer::null(),
            smem: vk::DeviceMemory::null(),
            image: vk::Image::null(),
            memory: vk::DeviceMemory::null(),
            view: vk::ImageView::null(),
            sampler: vk::Sampler::null(),
        }
    }
}

impl Drop for UploadGuard<'_> {
    fn drop(&mut self) {
        let d = self.device;
        unsafe {
            if self.sampler != vk::Sampler::null() {
                d.destroy_sampler(self.sampler, None);
            }
            if self.view != vk::ImageView::null() {
                d.destroy_image_view(self.view, None);
            }
            if self.image != vk::Image::null() {
                d.destroy_image(self.image, None);
            }
            if self.memory != vk::DeviceMemory::null() {
                crate::devmem::untrack(self.memory);
                d.free_memory(self.memory, None);
            }
            if self.staging != vk::Buffer::null() {
                d.destroy_buffer(self.staging, None);
            }
            if self.smem != vk::DeviceMemory::null() {
                crate::devmem::untrack(self.smem);
                d.free_memory(self.smem, None);
            }
        }
    }
}

/// Unwind guard for the image-building constructors ([`Texture::new_color_target`] and the two
/// dmabuf imports): frees the image/memory/view/sampler created so far if a later step fails —
/// `vkAllocateMemory` under the Venus mappable-blob pressure these paths hit, the FOREIGN-acquire
/// submit, or a view/sampler create — so a mid-build error doesn't orphan them. Each field starts
/// null (`vkDestroy*`/`vkFree*` no-op on null) and is filled as the constructor progresses; on
/// success [`Self::disarm`] nulls the four handles that move into the finished `Texture` so the
/// guard leaves them alone. Free order matches [`Texture::destroy`] (sampler→view→image→memory).
/// The sibling [`UploadGuard`] is the same guard plus the host staging buffer only
/// `Texture::upload` builds; kept separate so neither carries the other's irrelevant fields.
struct TextureGuard<'a> {
    device: &'a ash::Device,
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    sampler: vk::Sampler,
}

impl<'a> TextureGuard<'a> {
    fn new(device: &'a ash::Device) -> Self {
        Self {
            device,
            image: vk::Image::null(),
            memory: vk::DeviceMemory::null(),
            view: vk::ImageView::null(),
            sampler: vk::Sampler::null(),
        }
    }

    /// Disarm on success: the four handles now belong to the returned `Texture`, so the guard must
    /// not free them.
    fn disarm(&mut self) {
        self.image = vk::Image::null();
        self.memory = vk::DeviceMemory::null();
        self.view = vk::ImageView::null();
        self.sampler = vk::Sampler::null();
    }
}

impl Drop for TextureGuard<'_> {
    fn drop(&mut self) {
        let d = self.device;
        unsafe {
            if self.sampler != vk::Sampler::null() {
                d.destroy_sampler(self.sampler, None);
            }
            if self.view != vk::ImageView::null() {
                d.destroy_image_view(self.view, None);
            }
            if self.image != vk::Image::null() {
                d.destroy_image(self.image, None);
            }
            if self.memory != vk::DeviceMemory::null() {
                crate::devmem::untrack(self.memory);
                d.free_memory(self.memory, None);
            }
        }
    }
}

/// A texture built up to the point of its GPU copy: the device-local [`Texture`] plus the host
/// staging buffer holding its pixels, with the staging→image copy not yet recorded. Produced by
/// [`Texture::build_pending`] and consumed once the copy has been submitted (the caller frees the
/// staging then). Lets the single and batched upload paths share resource creation.
struct PendingUpload {
    staging: vk::Buffer,
    smem: vk::DeviceMemory,
    texture: Texture,
}

/// A texture whose pixels are staged on the host but whose GPU copy has **not been recorded
/// yet** — produced by [`Texture::stage_32bpp`], recorded by [`StagedTexture::record`] into a
/// command buffer the caller is already submitting.
///
/// Its pixels live in a slice of a shared [`StagingChunk`] ([`StagingPool`]), which it keeps alive
/// by holding a reference — deliberately the same shape as [`GlyphStaging`], so a frame can decide
/// at submit time whether this lives on in an in-flight record or dies with a fence wait, and no
/// retirement path has to know it exists.
///
/// **It must outlive the submit of the command buffer its copy was recorded into.** Dropping it
/// earlier lets the pool rewind the chunk and write over bytes the GPU has not read yet — which no
/// validation layer will call, and which shows up as a texture of garbage.
///
/// It carries **only** the staging half: the [`Texture`] went to the caller of
/// [`Texture::stage_32bpp`] and is destroyed the usual way. So a staged upload dropped without
/// ever being recorded costs nothing but the pixels' journey — the image is blank, not invalid,
/// which is what makes it safe for a frame that never happens to simply drop the queue. Its
/// destination image, by contrast, is named by raw handle and *not* kept alive here; whoever
/// queues one is responsible for that (see `PendingTextureUpload` in the compositor).
pub struct StagedTexture {
    source: StagedSource,
    /// The destination. Borrowed by handle, not owned — see the type docs.
    image: vk::Image,
    width: u32,
    height: u32,
}

/// Where a staged upload's pixels sit, and what keeps them there until the copy has been
/// submitted.
enum StagedSource {
    /// A slice of the renderer's shared arena, at an offset ([`StagingPool`]). Every ordinary
    /// import and shm re-upload.
    Pool(Arc<StagingChunk>, vk::DeviceSize),
    /// A whole buffer filled off the render thread by whoever produced the pixels — the wallpaper
    /// decoder, whose bytes are tens of megabytes and never wanted a copy through the pool at all
    /// ([`HostStaging`]).
    Host(Arc<HostStaging>),
}

impl StagedSource {
    fn buffer_and_offset(&self) -> (vk::Buffer, vk::DeviceSize) {
        match self {
            StagedSource::Pool(chunk, offset) => (chunk.buffer(), *offset),
            StagedSource::Host(staging) => (staging.buffer(), 0),
        }
    }

    fn device(&self) -> &ash::Device {
        match self {
            StagedSource::Pool(chunk, _) => chunk.device(),
            StagedSource::Host(staging) => staging.device(),
        }
    }
}

impl StagedTexture {
    /// Record the staging→image copy into `cbuf`, with the barriers that leave the image
    /// `SHADER_READ_ONLY_OPTIMAL` and order the copy before any later draw in the same command
    /// buffer.
    ///
    /// Must be called **outside a render pass** (copies and layout transitions both are), which is
    /// the same slot the glyph uploads and the deferred dmabuf acquires use.
    pub fn record(&self, cbuf: vk::CommandBuffer) {
        let (buffer, offset) = self.source.buffer_and_offset();
        unsafe {
            record_upload_copy_at(
                self.source.device(),
                cbuf,
                self.image,
                buffer,
                offset,
                self.width,
                self.height,
            );
        }
    }
}

/// A `TRANSFER_SRC` staging buffer holding `data`, mapped and written and unmapped. Frees both
/// handles if anything after the create fails. Shared by [`Texture::build_pending`], which pairs it
/// with a freshly created image, and [`StagedTexture::reupload_32bpp`], which points it at one that
/// already exists.
fn create_filled_staging(gpu: &Gpu, data: &[u8]) -> Result<(vk::Buffer, vk::DeviceMemory)> {
    let device = &gpu.device;
    let size = data.len() as vk::DeviceSize;
    let staging_ci = vk::BufferCreateInfo::default()
        .size(size)
        .usage(vk::BufferUsageFlags::TRANSFER_SRC)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let staging = unsafe { device.create_buffer(&staging_ci, None) }.context("staging buffer")?;
    let mut guard = UploadGuard::new(device);
    guard.staging = staging;
    let sreq = unsafe { device.get_buffer_memory_requirements(staging) };
    let smem = gpu.allocate(
        sreq,
        vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
    )?;
    guard.smem = smem;
    unsafe {
        device.bind_buffer_memory(staging, smem, 0)?;
        let ptr = device
            .map_memory(smem, 0, size, vk::MemoryMapFlags::empty())
            .context("map staging")? as *mut u8;
        {
            // Timed apart from creation: this is a host write into a never-touched mapping,
            // not a round trip, and it is the entire cost of a wallpaper upload. Conflating the
            // two made `created` read as 9.96ms on a frame that created almost nothing.
            let _timed = crate::stats::staging_write();
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
        }
        device.unmap_memory(smem);
    }
    // Success: both handles belong to the caller now.
    guard.staging = vk::Buffer::null();
    guard.smem = vk::DeviceMemory::null();
    Ok((staging, smem))
}

impl StagedTexture {
    /// Stage a full-extent 32bpp re-upload into an image that **already exists** — the shm cache
    /// hit, where a client recommits new pixels for a texture we keep.
    ///
    /// The sibling of [`Texture::stage_32bpp`], which stages into an image it also creates. Both
    /// exist so the copy can ride a frame's command buffer instead of costing a submit and a fence
    /// wait of its own: a live seat frame showed `11 shm in 19.38ms` moving 4.5 MiB, where the
    /// bytes were 0.33 ms and the rest was eleven round trips.
    ///
    /// The pixels go into the renderer's shared [`StagingPool`] rather than a buffer of their own.
    /// A buffer per re-upload is what the deferral first shipped, and on Venus that is a fresh
    /// mappable blob on every commit of every shm surface: it ran the host out of them two minutes
    /// into a live session. The pool keeps one buffer and rewinds it per frame.
    ///
    /// `image` must be a `TRANSFER_DST` image of `width`×`height`; `data` must be its tightly
    /// packed `width*height*4` extent. As with any [`StagedTexture`], it must outlive the submit
    /// of the command buffer its copy is recorded into.
    pub fn reupload_32bpp(
        gpu: &Arc<Gpu>,
        pool: &mut StagingPool,
        image: vk::Image,
        width: u32,
        height: u32,
        data: &[u8],
    ) -> Result<Self> {
        let size = (width as vk::DeviceSize) * (height as vk::DeviceSize) * 4;
        anyhow::ensure!(
            data.len() as vk::DeviceSize == size,
            "re-upload data is {} bytes for {width}x{height}, need {size}",
            data.len(),
        );
        Self::reupload_32bpp_with(gpu, pool, image, width, height, |dst| {
            dst.copy_from_slice(data)
        })
    }

    /// [`Self::reupload_32bpp`] for a producer whose pixels are not one contiguous slice.
    ///
    /// `fill` is handed the `width*height*4` staging bytes to write in place — an shm pool with a
    /// stride repacks its rows straight in, rather than into a `Vec` that is then copied. It must
    /// write every byte and must not read them; see [`crate::staging::StagingPool::stage_with`].
    pub fn reupload_32bpp_with(
        gpu: &Arc<Gpu>,
        pool: &mut StagingPool,
        image: vk::Image,
        width: u32,
        height: u32,
        fill: impl FnOnce(&mut [u8]),
    ) -> Result<Self> {
        let size = (width as vk::DeviceSize) * (height as vk::DeviceSize) * 4;
        crate::stats::uploaded(size);
        let (chunk, offset) = pool.stage_with(gpu, size, fill)?;
        Ok(StagedTexture {
            source: StagedSource::Pool(chunk, offset),
            image,
            width,
            height,
        })
    }
}

impl Texture {
    /// Upload tight `width*height` RGBA8 pixels into a shader-readable texture.
    pub fn from_rgba(
        gpu: &Gpu,
        pool: vk::CommandPool,
        width: u32,
        height: u32,
        rgba: &[u8],
        filter: vk::Filter,
    ) -> Result<Self> {
        Self::upload(
            gpu,
            pool,
            width,
            height,
            rgba,
            FORMAT,
            4,
            vk::ComponentMapping::default(),
            filter,
        )
    }

    /// Stage a texture whose pixels are **already in device-visible memory**, written there by
    /// whichever thread produced them ([`HostStaging`]) — the wallpaper. The render thread is left
    /// with the image creation and a copy for the next frame to record: the multi-megabyte host
    /// write, which is the whole cost of a wallpaper upload and has no GPU work in it, happened on
    /// the decode worker, and the copy itself no longer costs a submit and a fence wait (a live
    /// frame carried `first upload 18.62ms` for 48 MiB).
    ///
    /// The staging is `Arc`-held by the returned [`StagedTexture`] for the usual reason: a
    /// wallpaper can change the moment after it is staged, and the copy reads that buffer long
    /// after this returns.
    ///
    /// `staging` must hold exactly `width * height * 4` bytes, and must belong to `gpu` — see
    /// [`HostStaging::belongs_to`], which the caller checks, because a device mismatch is a wasted
    /// decode rather than something this can recover from.
    #[allow(clippy::too_many_arguments)]
    pub fn stage_from_host_staging(
        gpu: &Gpu,
        staging: &Arc<HostStaging>,
        width: u32,
        height: u32,
        format: vk::Format,
        alpha_one: bool,
        filter: vk::Filter,
    ) -> Result<(Self, StagedTexture)> {
        let size = (width as usize) * (height as usize) * 4;
        anyhow::ensure!(
            staging.len() == size,
            "staged wallpaper is {} bytes for {width}x{height}, need {size}",
            staging.len()
        );
        crate::stats::uploaded(size as vk::DeviceSize);

        let components = if alpha_one {
            vk::ComponentMapping::default().a(vk::ComponentSwizzle::ONE)
        } else {
            vk::ComponentMapping::default()
        };
        let texture = Self::new_sampled_image_raw(gpu, width, height, format, components, filter)?;
        let staged = StagedTexture {
            source: StagedSource::Host(staging.clone()),
            image: texture.image,
            width,
            height,
        };
        Ok((texture, staged))
    }

    /// Upload tight `width*height` single-channel coverage (R8) — the glyph atlas format.
    pub fn from_coverage(
        gpu: &Gpu,
        pool: vk::CommandPool,
        width: u32,
        height: u32,
        coverage: &[u8],
        filter: vk::Filter,
    ) -> Result<Self> {
        Self::upload(
            gpu,
            pool,
            width,
            height,
            coverage,
            vk::Format::R8_UNORM,
            1,
            vk::ComponentMapping::default(),
            filter,
        )
    }

    /// A blank transfer-only image in an arbitrary `format`: `TRANSFER_DST | TRANSFER_SRC`,
    /// device-local, `OPTIMAL` tiling, and **no view or sampler** (it is never rendered into nor
    /// sampled — only blitted into and copied out of).
    ///
    /// This is the readback conversion staging image: blit an `R8G8B8A8` frame into a `B8G8R8A8`
    /// one and `vkCmdBlitImage` reorders the channels for us, so a BGRA consumer (an `Xrgb8888` shm
    /// pool, say) needs no CPU swizzle. `OPTIMAL` tiling matters: the spec's
    /// required-format-support tables mandate `BLIT_SRC | BLIT_DST` for both formats there, so
    /// the blit is guaranteed on any conformant driver rather than resting on what Venus
    /// happens to allow.
    ///
    /// `Texture::destroy` runs `vkDestroySampler`/`vkDestroyImageView` on the null handles, which
    /// is a defined no-op.
    pub fn new_transfer_image(
        gpu: &Gpu,
        width: u32,
        height: u32,
        format: vk::Format,
    ) -> Result<Self> {
        let _timed = crate::stats::creating();
        let device = &gpu.device;

        let image_ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);

        let mut guard = TextureGuard::new(device);
        let image =
            unsafe { device.create_image(&image_ci, None) }.context("transfer-image image")?;
        guard.image = image;
        let ireq = unsafe { device.get_image_memory_requirements(image) };
        let memory = gpu.allocate(ireq, vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
        guard.memory = memory;
        unsafe { device.bind_image_memory(image, memory, 0)? };

        guard.disarm();
        Ok(Self {
            image,
            view: vk::ImageView::null(),
            sampler: vk::Sampler::null(),
            memory,
            width,
            height,
        })
    }

    /// Create a blank `width*height` color target that is **both** renderable and sampleable: a
    /// device-local `COLOR_ATTACHMENT | SAMPLED | TRANSFER_SRC` image plus a view and sampler, with
    /// no upload. The image is left in `UNDEFINED` layout — the caller's render pass performs the
    /// first transition (its `initial_layout` is `UNDEFINED`, discarding the blank contents), and a
    /// later barrier moves it to `SHADER_READ_ONLY_OPTIMAL` when it is re-sampled. This is the
    /// offscreen-snapshot / blur / clipped-surface bridge: render into it, then sample it.
    pub fn new_color_target(
        gpu: &Gpu,
        width: u32,
        height: u32,
        filter: vk::Filter,
    ) -> Result<Self> {
        let _timed = crate::stats::creating();
        let device = &gpu.device;

        let image_ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(RENDER_FORMAT)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            // TRANSFER_SRC for readback; TRANSFER_DST so it can also receive a blit/copy (e.g. the
            // blurred output lifted out of the blur chain).
            .usage(
                vk::ImageUsageFlags::COLOR_ATTACHMENT
                    | vk::ImageUsageFlags::SAMPLED
                    | vk::ImageUsageFlags::TRANSFER_SRC
                    | vk::ImageUsageFlags::TRANSFER_DST,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let mut guard = TextureGuard::new(device);
        let image =
            unsafe { device.create_image(&image_ci, None) }.context("color-target image")?;
        guard.image = image;
        let ireq = unsafe { device.get_image_memory_requirements(image) };
        let memory = gpu.allocate(ireq, vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
        guard.memory = memory;
        unsafe { device.bind_image_memory(image, memory, 0)? };

        let view_ci = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(RENDER_FORMAT)
            .subresource_range(COLOR_RANGE);
        let view =
            unsafe { device.create_image_view(&view_ci, None) }.context("color-target view")?;
        guard.view = view;

        let sampler_ci = vk::SamplerCreateInfo::default()
            .mag_filter(filter)
            .min_filter(filter)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE);
        let sampler = unsafe { device.create_sampler(&sampler_ci, None) }.context("sampler")?;
        guard.sampler = sampler;

        guard.disarm();
        Ok(Texture {
            image,
            view,
            sampler,
            memory,
            width,
            height,
        })
    }

    /// Allocate a **scanout** color target in Vulkan and export it as a dmabuf.
    ///
    /// The inverse of [`Self::import_dmabuf_render_target`], and the reason it exists: on a virtio
    /// stack the *importing* direction only works when whoever allocated the buffer happens to be
    /// the same driver the Vulkan device is, which for gbm is decided by a session-wide env var
    /// (`MESA_LOADER_DRIVER_OVERRIDE`) nobody here controls. Allocating on our own device removes
    /// the question: the memory is ours, and the dmabuf we hand KMS is a prime export of the very
    /// blob venus rendered into. See `docs/fork/foundation.md`.
    ///
    /// `modifiers` is the candidate list handed to `VkImageDrmFormatModifierListCreateInfoEXT`; the
    /// driver picks one and [`ScanoutExport::modifier`] reports which. Candidates the device does
    /// not enumerate — or that lack `required` — are dropped first, because the list create-info
    /// gives no way to find out afterwards *why* creation failed, and a modifier whose features
    /// don't cover the commands we record against it is undefined behavior rather than an error
    /// (same reasoning as [`Gpu::check_modifier_features`], which this calls).
    ///
    /// The returned pitch/offset come from `vkGetImageSubresourceLayout` on the memory plane, never
    /// from `width * 4`: a driver is free to pad, and on this stack the value became truthful only
    /// with the modifier passthrough in mesa 26.1.5.
    #[allow(clippy::too_many_arguments)]
    pub fn allocate_scanout(
        gpu: &Gpu,
        width: u32,
        height: u32,
        format: vk::Format,
        modifiers: &[u64],
        required: vk::FormatFeatureFlags,
        usage: vk::ImageUsageFlags,
        filter: vk::Filter,
    ) -> Result<(Texture, ScanoutExport)> {
        // Same accounting as the import: a DRM-modifier `vkCreateImage` is the expensive one.
        let _timed = crate::stats::creating();
        let device = &gpu.device;

        anyhow::ensure!(
            gpu.supports("VK_EXT_image_drm_format_modifier")
                && gpu.supports("VK_EXT_external_memory_dma_buf")
                && gpu.supports("VK_KHR_external_memory_fd"),
            "this device cannot allocate exportable scanout images: it lacks one of \
             VK_EXT_image_drm_format_modifier / VK_EXT_external_memory_dma_buf / \
             VK_KHR_external_memory_fd",
        );

        let usable: Vec<u64> = modifiers
            .iter()
            .copied()
            .filter(|m| gpu.check_modifier_features(format, *m, required).is_ok())
            .collect();
        anyhow::ensure!(
            !usable.is_empty(),
            "no candidate DRM modifier for {format:?} supports {required:?} on this device \
             (asked about {modifiers:x?})",
        );

        let mut mod_info =
            vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(&usable);
        let mut ext_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let image_ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut ext_info)
            .push_next(&mut mod_info);
        let mut guard = TextureGuard::new(device);
        let image =
            unsafe { device.create_image(&image_ci, None) }.context("create scanout image")?;
        guard.image = image;

        let mem_req = unsafe { device.get_image_memory_requirements(image) };
        // No `vkGetMemoryFdPropertiesKHR` narrowing here — that query answers "which heaps can hold
        // *this foreign handle*", and there is no foreign handle: we are about to create the
        // handle. The image's own requirements are the whole constraint.
        let mem_type =
            gpu.find_memory_type(mem_req.memory_type_bits, vk::MemoryPropertyFlags::empty())?;

        // Dedicated + exportable. Dedicated is effectively required for an exported image on this
        // stack, and it is also what makes the exported fd name exactly this image's storage rather
        // than a suballocation of something larger.
        let mut export_info = vk::ExportMemoryAllocateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_req.size)
            .memory_type_index(mem_type)
            .push_next(&mut export_info)
            .push_next(&mut dedicated);
        let memory = unsafe { device.allocate_memory(&alloc_info, None) }
            .context("allocate exportable scanout memory")?;
        crate::devmem::track(
            memory,
            mem_req.size,
            crate::devmem::Site::Explicit("scanout-export"),
        );
        guard.memory = memory;
        unsafe { device.bind_image_memory(image, memory, 0) }
            .context("bind scanout image memory")?;

        // Which of `usable` the driver actually picked. There is no other way to learn it, and the
        // dmabuf we hand KMS has to name it.
        let ext_mod = gpu.drm_format_modifier()?;
        let mut props = vk::ImageDrmFormatModifierPropertiesEXT::default();
        unsafe { ext_mod.get_image_drm_format_modifier_properties(image, &mut props) }
            .context("vkGetImageDrmFormatModifierPropertiesEXT")?;
        let modifier = props.drm_format_modifier;

        // Memory plane 0, not colour plane 0: for `DRM_FORMAT_MODIFIER_EXT` tiling the layout is
        // queried per *memory* plane, and the aspect mask is the only thing that says which.
        let layout = unsafe {
            device.get_image_subresource_layout(
                image,
                vk::ImageSubresource {
                    aspect_mask: vk::ImageAspectFlags::MEMORY_PLANE_0_EXT,
                    mip_level: 0,
                    array_layer: 0,
                },
            )
        };

        let ext_fd = gpu.external_memory()?;
        let get_fd = vk::MemoryGetFdInfoKHR::default()
            .memory(memory)
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        // Ownership transfers to us: `vkGetMemoryFdKHR` creates a new fd per call, and closing it
        // does not free the memory (the `VkDeviceMemory` still owns that).
        let fd = unsafe { ext_fd.get_memory_fd(&get_fd) }.context("vkGetMemoryFdKHR")?;
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };

        const VIEWABLE: vk::ImageUsageFlags = vk::ImageUsageFlags::from_raw(
            vk::ImageUsageFlags::COLOR_ATTACHMENT.as_raw()
                | vk::ImageUsageFlags::SAMPLED.as_raw()
                | vk::ImageUsageFlags::STORAGE.as_raw()
                | vk::ImageUsageFlags::INPUT_ATTACHMENT.as_raw(),
        );
        let view = if usage.intersects(VIEWABLE) {
            let view_ci = vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(format)
                .subresource_range(COLOR_RANGE);
            let view = unsafe { device.create_image_view(&view_ci, None) }
                .context("scanout image view")?;
            guard.view = view;
            view
        } else {
            vk::ImageView::null()
        };

        // Unused (a scanout target is never sampled), kept so this fits `Texture`.
        let sampler_ci = vk::SamplerCreateInfo::default()
            .mag_filter(filter)
            .min_filter(filter)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE);
        let sampler = unsafe { device.create_sampler(&sampler_ci, None) }.context("sampler")?;
        guard.sampler = sampler;

        guard.disarm();
        Ok((
            Texture {
                image,
                view,
                sampler,
                memory,
                width,
                height,
            },
            ScanoutExport {
                fd,
                modifier,
                offset: layout.offset as u32,
                stride: layout.row_pitch as u32,
            },
        ))
    }

    /// Import a foreign (GBM-allocated) dmabuf as a **renderable** color target: a
    /// `COLOR_ATTACHMENT | TRANSFER_SRC` `VkImage` whose memory is the dmabuf, with an explicit DRM
    /// format modifier + plane layout. This is the KMS-scanout dual of [`crate::dmabuf`]'s sampled
    /// import (Stage 1) — render a frame into it and the pixels land in the dmabuf for scanout (or,
    /// in tests, a CPU map of the LINEAR buffer). Single plane only (all our formats are); the
    /// render pass performs the `UNDEFINED`→attachment transition, so no pre-acquire barrier is
    /// needed for the LINEAR modifier (nothing to detile). `TRANSFER_SRC` lets a test read it back.
    ///
    /// `fd` is duplicated internally (Vulkan consumes the dup on a successful import; the caller
    /// keeps ownership of the original). `format` must match `fourcc`'s byte order (e.g.
    /// `R8G8B8A8_UNORM` for `Abgr8888`/`Xbgr8888`, `B8G8R8A8_UNORM` for `Argb8888`/`Xrgb8888`).
    /// `usage` selects the role: `COLOR_ATTACHMENT | TRANSFER_SRC` to render straight into the
    /// dmabuf (RGBA-order scanout), or `COLOR_ATTACHMENT | TRANSFER_DST` for a present-blit target
    /// that receives a byte-reordering blit (the `Argb8888`/`Xrgb8888` KMS path).
    #[allow(clippy::too_many_arguments)]
    pub fn import_dmabuf_render_target(
        gpu: &Gpu,
        width: u32,
        height: u32,
        fd: BorrowedFd<'_>,
        offset: u32,
        stride: u32,
        modifier: u64,
        format: vk::Format,
        usage: vk::ImageUsageFlags,
        filter: vk::Filter,
    ) -> Result<Self> {
        // Counted like any other create, and it is the *expensive* one: a dmabuf/DRM-modifier
        // `vkCreateImage` that misses venus's requirements cache costs 0.06-0.7 ms against 3 us
        // for a plain image (`docs/fork/foundation.md` §5).
        let _timed = crate::stats::creating();
        let device = &gpu.device;

        let plane_layout = vk::SubresourceLayout {
            offset: offset as u64,
            size: 0,
            row_pitch: stride as u64,
            array_pitch: 0,
            depth_pitch: 0,
        };
        let mut mod_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(modifier)
            .plane_layouts(std::slice::from_ref(&plane_layout));
        let mut ext_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let image_ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut ext_info)
            .push_next(&mut mod_info);
        let mut guard = TextureGuard::new(device);
        let image = unsafe { device.create_image(&image_ci, None) }
            .context("create dmabuf render-target image")?;
        guard.image = image;

        let mem_req = unsafe { device.get_image_memory_requirements(image) };

        let mem_type = gpu.dmabuf_memory_type(fd, mem_req)?;

        // Import consumes the fd on success; hand Vulkan a fresh dup and let the caller keep
        // theirs.
        let raw_fd = fd
            .try_clone_to_owned()
            .context("dup dmabuf fd for import")?
            .into_raw_fd();
        let mut import_info = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(raw_fd);
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_req.size)
            .memory_type_index(mem_type)
            .push_next(&mut import_info)
            .push_next(&mut dedicated);
        let memory = unsafe { device.allocate_memory(&alloc_info, None) }.map_err(|e| {
            // On failure Vulkan did not take the fd; close our dup so it doesn't leak.
            drop(unsafe { OwnedFd::from_raw_fd(raw_fd) });
            anyhow::anyhow!("import dmabuf render-target memory (vkAllocateMemory): {e}")
        })?;
        crate::devmem::track(
            memory,
            mem_req.size,
            crate::devmem::Site::Explicit("dmabuf-import-render-target"),
        );
        guard.memory = memory;
        unsafe { device.bind_image_memory(image, memory, 0) }
            .context("bind imported render-target memory")?;

        // Only an image whose usage has a view-capable bit may have a view at all
        // (VUID-VkImageViewCreateInfo-image-04441). The present-blit target is imported
        // transfer-only — it is blitted into and read back, never attached or sampled (the shadow
        // is the attachment) — so a view on it is both invalid and useless: a blit takes images,
        // not views. Leave it null, exactly as `new_transfer_image` does; `Texture::destroy` treats
        // a null view as a defined no-op.
        const VIEWABLE: vk::ImageUsageFlags = vk::ImageUsageFlags::from_raw(
            vk::ImageUsageFlags::COLOR_ATTACHMENT.as_raw()
                | vk::ImageUsageFlags::SAMPLED.as_raw()
                | vk::ImageUsageFlags::STORAGE.as_raw()
                | vk::ImageUsageFlags::INPUT_ATTACHMENT.as_raw(),
        );
        let view = if usage.intersects(VIEWABLE) {
            let view_ci = vk::ImageViewCreateInfo::default()
                .image(image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(format)
                .subresource_range(COLOR_RANGE);
            let view = unsafe { device.create_image_view(&view_ci, None) }
                .context("dmabuf render-target view")?;
            guard.view = view;
            view
        } else {
            vk::ImageView::null()
        };

        // Sampler is unused (a scanout target is never sampled) but kept so this fits `Texture`.
        let sampler_ci = vk::SamplerCreateInfo::default()
            .mag_filter(filter)
            .min_filter(filter)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE);
        let sampler = unsafe { device.create_sampler(&sampler_ci, None) }.context("sampler")?;
        guard.sampler = sampler;

        guard.disarm();
        Ok(Texture {
            image,
            view,
            sampler,
            memory,
            width,
            height,
        })
    }

    /// Import a foreign (client-allocated) dmabuf as a **sampled** texture: a `SAMPLED` `VkImage`
    /// whose memory is the dmabuf, with an explicit DRM format modifier + single-plane layout, a
    /// view (optionally forcing sampled alpha to 1.0 for the `X`-formats via a component swizzle),
    /// and a sampler. This is the client-buffer dual of [`Self::import_dmabuf_render_target`]: the
    /// compositor samples it when compositing the window.
    ///
    /// Unlike the render-target import, the producer is outside Vulkan, so the image needs an
    /// explicit acquire barrier — `UNDEFINED`→`SHADER_READ_ONLY_OPTIMAL`, transferring ownership
    /// from the `FOREIGN` queue family when `VK_EXT_queue_family_foreign` is present — before
    /// anything samples it. For the LINEAR modifier the bytes survive the `UNDEFINED` old layout
    /// (nothing to detile). Single plane only.
    ///
    /// **This does not perform that acquire**, and the returned image is left in `UNDEFINED`. It
    /// used to, on a command buffer and fence-wait of its own, which a live overview frame showed
    /// costing ~3 ms for a single pipeline barrier. The caller records it instead — with
    /// [`Self::record_reacquire_dmabuf_sampled`] and an `old_layout` of `UNDEFINED`, which emits
    /// exactly the same barrier — so it rides a submit that was happening anyway. A cached import
    /// already went through that path on every recommit; this makes the first commit take it too.
    ///
    /// `fd` is duplicated internally (Vulkan consumes the dup on success; the caller keeps the
    /// original). `format` must match `fourcc`'s byte order (e.g. `B8G8R8A8_UNORM` for
    /// `Argb8888`/`Xrgb8888`, `R8G8B8A8_UNORM` for `Abgr8888`/`Xbgr8888`).
    #[allow(clippy::too_many_arguments)]
    pub fn import_dmabuf_sampled(
        gpu: &Gpu,
        width: u32,
        height: u32,
        fd: BorrowedFd<'_>,
        offset: u32,
        stride: u32,
        modifier: u64,
        format: vk::Format,
        alpha_one: bool,
        filter: vk::Filter,
    ) -> Result<Self> {
        // Counted like any other create, and it is the *expensive* one: a dmabuf/DRM-modifier
        // `vkCreateImage` that misses venus's requirements cache costs 0.06-0.7 ms against 3 us
        // for a plain image (`docs/fork/foundation.md` §5).
        let _timed = crate::stats::creating();
        let device = &gpu.device;

        let plane_layout = vk::SubresourceLayout {
            offset: offset as u64,
            size: 0,
            row_pitch: stride as u64,
            array_pitch: 0,
            depth_pitch: 0,
        };
        let mut mod_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(modifier)
            .plane_layouts(std::slice::from_ref(&plane_layout));
        let mut ext_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let image_ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut ext_info)
            .push_next(&mut mod_info);
        let mut guard = TextureGuard::new(device);
        let image = unsafe { device.create_image(&image_ci, None) }
            .context("create sampled dmabuf image")?;
        guard.image = image;

        let mem_req = unsafe { device.get_image_memory_requirements(image) };

        let mem_type = gpu.dmabuf_memory_type(fd, mem_req)?;

        // Import consumes the fd on success; hand Vulkan a fresh dup and let the caller keep
        // theirs.
        let raw_fd = fd
            .try_clone_to_owned()
            .context("dup dmabuf fd for import")?
            .into_raw_fd();
        let mut import_info = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(raw_fd);
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_req.size)
            .memory_type_index(mem_type)
            .push_next(&mut import_info)
            .push_next(&mut dedicated);
        let memory = unsafe { device.allocate_memory(&alloc_info, None) }.map_err(|e| {
            // On failure Vulkan did not take the fd; close our dup so it doesn't leak.
            drop(unsafe { OwnedFd::from_raw_fd(raw_fd) });
            anyhow::anyhow!("import sampled dmabuf memory (vkAllocateMemory): {e}")
        })?;
        crate::devmem::track(
            memory,
            mem_req.size,
            crate::devmem::Site::Explicit("dmabuf-import-sampled"),
        );
        guard.memory = memory;
        unsafe { device.bind_image_memory(image, memory, 0) }
            .context("bind imported sampled memory")?;

        // The acquire barrier is deliberately NOT recorded here — see this function's docs. The
        // image stays `UNDEFINED` and the caller records the acquire into a command buffer it was
        // already submitting.

        let components = if alpha_one {
            vk::ComponentMapping::default().a(vk::ComponentSwizzle::ONE)
        } else {
            vk::ComponentMapping::default()
        };
        let view_ci = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .components(components)
            .subresource_range(COLOR_RANGE);
        let view =
            unsafe { device.create_image_view(&view_ci, None) }.context("sampled dmabuf view")?;
        guard.view = view;

        let sampler_ci = vk::SamplerCreateInfo::default()
            .mag_filter(filter)
            .min_filter(filter)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE);
        let sampler = unsafe { device.create_sampler(&sampler_ci, None) }
            .context("sampled dmabuf sampler")?;
        guard.sampler = sampler;

        guard.disarm();
        Ok(Texture {
            image,
            view,
            sampler,
            memory,
            width,
            height,
        })
    }

    /// Record the acquire barrier for an imported sampled dmabuf into an existing `cbuf` — the
    /// caller owns the submit, and records it before that command buffer's render pass. The
    /// acquire therefore rides a submit that was happening anyway, instead of costing a standalone
    /// command buffer, submit and fence-wait per commit.
    ///
    /// Serves both the first acquire after [`Self::import_dmabuf_sampled`] (pass `UNDEFINED`) and
    /// every recommit's re-acquire (pass the tracked layout) — the same barrier either way.
    ///
    /// The client renders new content into the *same* underlying dmabuf every frame (this image's
    /// memory IS that shared dmabuf), so a cache that reuses the [`Self::import_dmabuf_sampled`]
    /// image across commits must, before sampling it again, (a) invalidate our sampler's caches so
    /// it re-reads the shared memory and (b) re-take queue-family ownership from `FOREIGN`. This is
    /// an ownership *acquire* + cache-visibility barrier ONLY: it does **not** synchronize with the
    /// producer's GPU writes (a pipeline barrier cannot observe a foreign GPU context). The
    /// producer's writes are guaranteed to have already landed in the shared memory UPSTREAM, by
    /// the commit-time acquire blocker (`linux-drm-syncobj-v1` timeline point / implicit-fence
    /// poll); see the renderer's `import_dmabuf_as_texture`.
    ///
    /// When `VK_EXT_queue_family_foreign` is present we keep the image's *current* tracked
    /// `old_layout` (`SHADER_READ_ONLY_OPTIMAL`) rather than `UNDEFINED`: `UNDEFINED` licenses the
    /// driver to discard/re-lay-out contents, harmless on this VM's LINEAR-only path but corrupting
    /// for a tiled/CCS modifier. Without the extension there is no `FOREIGN` ownership transfer, so
    /// a same-layout barrier with an empty source scope would be a cache-invalidation no-op and
    /// sample stale content; we instead force an `UNDEFINED`→`SHADER_READ_ONLY_OPTIMAL` transition,
    /// which does invalidate the read caches and is safe because only LINEAR buffers ever reach the
    /// cache (their bytes survive `UNDEFINED`). Either way we deliberately skip the paired
    /// ours→`FOREIGN` *release* after the previous frame's last sample: a full ownership round-trip
    /// is spec-correct but tolerated to skip for LINEAR (revisit if tiled modifiers are ever
    /// imported). The image ends in `SHADER_READ_ONLY_OPTIMAL`. Callers pass the tracked current
    /// layout as `old_layout`.
    pub fn record_reacquire_dmabuf_sampled(
        &self,
        gpu: &Gpu,
        cbuf: vk::CommandBuffer,
        old_layout: vk::ImageLayout,
    ) {
        let device = &gpu.device;
        let (src_qf, dst_qf, old_layout) = if gpu.supports("VK_EXT_queue_family_foreign") {
            (vk::QUEUE_FAMILY_FOREIGN_EXT, gpu.queue_family, old_layout)
        } else {
            (
                vk::QUEUE_FAMILY_IGNORED,
                vk::QUEUE_FAMILY_IGNORED,
                vk::ImageLayout::UNDEFINED,
            )
        };
        unsafe {
            let barrier = vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .old_layout(old_layout)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_queue_family_index(src_qf)
                .dst_queue_family_index(dst_qf)
                .image(self.image)
                .subresource_range(COLOR_RANGE);
            crate::stats::barriers(1);
            device.cmd_pipeline_barrier(
                cbuf,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier],
            );
        }
    }

    /// Upload `data` (tight `width*height*bpp` bytes) into a shader-readable `format` texture.
    /// One image, one submit, one fence-wait.
    ///
    /// The compositor does not use this path — it stages into the renderer's pool and lets a frame
    /// record the copy ([`stage_32bpp`](Self::stage_32bpp)), which costs no submit at all. This is
    /// for the demo binary and the benchmarks, where a self-contained upload is the point.
    #[allow(clippy::too_many_arguments)]
    fn upload(
        gpu: &Gpu,
        pool: vk::CommandPool,
        width: u32,
        height: u32,
        data: &[u8],
        format: vk::Format,
        bpp: vk::DeviceSize,
        components: vk::ComponentMapping,
        filter: vk::Filter,
    ) -> Result<Self> {
        let PendingUpload {
            staging,
            smem,
            texture,
        } = Self::build_pending(gpu, width, height, data, format, bpp, components, filter)?;
        let device = &gpu.device;
        let result = gpu.run_commands(pool, crate::stats::SubmitSite::Upload, |cbuf| unsafe {
            record_upload_copy(device, cbuf, texture.image, staging, width, height);
        });
        // The staging buffer has served its purpose either way; on a submit/wait error also
        // free the half-built texture (its copy never ran). `run_commands` already drains the
        // device on a wait error, so freeing here can't race an in-flight submission.
        unsafe {
            device.destroy_buffer(staging, None);
            crate::devmem::untrack(smem);
            device.free_memory(smem, None);
        }
        match result {
            Ok(()) => Ok(texture),
            Err(err) => {
                texture.destroy(gpu);
                Err(err)
            }
        }
    }

    /// Build a texture and stage its pixels, but **do not record or submit the copy** — the
    /// caller folds it into a command buffer it is already going to submit
    /// ([`StagedTexture::record`]).
    ///
    /// This is [`upload`](Self::upload) with the round trip taken out, and it is the same trick as
    /// [`stage_coverage_regions`](Self::stage_coverage_regions) for glyphs. It exists because the
    /// per-texture path costs a full submit *and a blocking fence wait* each, and a live seat frame
    /// was measured at `9 upload in 16.22ms` for 1.0 MiB of pixels — the bytes were 0.24ms of that.
    /// Worse, each wait re-parks the host ring (`docs/fork/foundation.md` §5), so every upload
    /// also taxes the *next* submit with a ~1ms wake.
    ///
    /// The returned texture is complete except for its contents — view and sampler are valid, and
    /// it belongs to the caller like any other — but nothing may **sample** it until the copy
    /// carried by the returned [`StagedTexture`] has been recorded and submitted.
    #[allow(clippy::too_many_arguments)]
    pub fn stage_32bpp(
        gpu: &Arc<Gpu>,
        pool: &mut StagingPool,
        width: u32,
        height: u32,
        data: &[u8],
        format: vk::Format,
        alpha_one: bool,
        filter: vk::Filter,
    ) -> Result<(Self, StagedTexture)> {
        let components = if alpha_one {
            vk::ComponentMapping::default().a(vk::ComponentSwizzle::ONE)
        } else {
            vk::ComponentMapping::default()
        };
        let size = (width as vk::DeviceSize) * (height as vk::DeviceSize) * 4;
        assert_eq!(
            data.len() as vk::DeviceSize,
            size,
            "texture data size mismatch"
        );
        crate::stats::uploaded(size);
        // The pixels first, so a staging failure costs no image: the pool shares one buffer across
        // every import and re-upload in the frame, instead of a mappable blob per upload.
        let (chunk, offset) = pool.stage(gpu, data)?;
        let texture = Self::new_sampled_image_raw(gpu, width, height, format, components, filter)?;
        let staged = StagedTexture {
            source: StagedSource::Pool(chunk, offset),
            image: texture.image,
            width,
            height,
        };
        Ok((texture, staged))
    }

    /// The device-local half of an upload: a sampled image with its memory, view and sampler, and
    /// no staging buffer or commands at all. The pixels are somebody else's problem — a slice
    /// staged into the shared pool ([`stage_32bpp`](Self::stage_32bpp)) or a
    /// [`HostStaging`] filled off-thread ([`from_host_staging`](Self::from_host_staging)).
    fn new_sampled_image_raw(
        gpu: &Gpu,
        width: u32,
        height: u32,
        format: vk::Format,
        components: vk::ComponentMapping,
        filter: vk::Filter,
    ) -> Result<Texture> {
        let _timed = crate::stats::creating();
        let device = &gpu.device;
        let mut guard = UploadGuard::new(device);

        let image_ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { device.create_image(&image_ci, None) }.context("texture image")?;
        guard.image = image;
        let ireq = unsafe { device.get_image_memory_requirements(image) };
        let memory = gpu.allocate(ireq, vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
        guard.memory = memory;
        unsafe { device.bind_image_memory(image, memory, 0)? };

        let view_ci = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .components(components)
            .subresource_range(COLOR_RANGE);
        let view = unsafe { device.create_image_view(&view_ci, None) }.context("texture view")?;
        guard.view = view;

        let sampler_ci = vk::SamplerCreateInfo::default()
            .mag_filter(filter)
            .min_filter(filter)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE);
        let sampler = unsafe { device.create_sampler(&sampler_ci, None) }.context("sampler")?;
        guard.sampler = sampler;

        guard.image = vk::Image::null();
        guard.memory = vk::DeviceMemory::null();
        guard.view = vk::ImageView::null();
        guard.sampler = vk::Sampler::null();
        Ok(Texture {
            image,
            view,
            sampler,
            memory,
            width,
            height,
        })
    }

    /// Build everything `upload` needs before the GPU copy — the host staging buffer (already
    /// holding the pixels) and the device-local `Texture` (image/view/sampler), with the copy
    /// still un-recorded. Shared by the single [`upload`](Self::upload) and the batched
    /// [`TextureBatch`] so their resource creation stays identical; the caller records the copy
    /// (via [`record_upload_copy`]) and frees the staging once it has been submitted.
    #[allow(clippy::too_many_arguments)]
    fn build_pending(
        gpu: &Gpu,
        width: u32,
        height: u32,
        data: &[u8],
        format: vk::Format,
        bpp: vk::DeviceSize,
        components: vk::ComponentMapping,
        filter: vk::Filter,
    ) -> Result<PendingUpload> {
        let _timed = crate::stats::creating();
        let device = &gpu.device;
        let size = (width as vk::DeviceSize) * (height as vk::DeviceSize) * bpp;
        crate::stats::uploaded(size);
        assert_eq!(
            data.len() as vk::DeviceSize,
            size,
            "texture data size mismatch"
        );

        // Frees any handle created below if a later `?` fails partway (so a failed allocate/bind/
        // create doesn't orphan a resource); on success all six handles are nulled out of it
        // before they move into the returned `PendingUpload`.
        let mut guard = UploadGuard::new(device);

        // --- staging buffer with the pixel data ---
        let (staging, smem) = create_filled_staging(gpu, data)?;
        guard.staging = staging;
        guard.smem = smem;

        // --- device-local sampled image ---
        let image_ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { device.create_image(&image_ci, None) }.context("texture image")?;
        guard.image = image;
        let ireq = unsafe { device.get_image_memory_requirements(image) };
        let memory = gpu.allocate(ireq, vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
        guard.memory = memory;
        unsafe { device.bind_image_memory(image, memory, 0)? };

        // The view/sampler only reference the image handle, so they are valid before the copy
        // runs (nothing samples the texture until its batch is submitted).
        let view_ci = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .components(components)
            .subresource_range(COLOR_RANGE);
        let view = unsafe { device.create_image_view(&view_ci, None) }.context("texture view")?;
        guard.view = view;

        let sampler_ci = vk::SamplerCreateInfo::default()
            .mag_filter(filter)
            .min_filter(filter)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE);
        let sampler = unsafe { device.create_sampler(&sampler_ci, None) }.context("sampler")?;
        guard.sampler = sampler;

        // Success: the handles now belong to `PendingUpload`; disarm the guard for all of them.
        guard.staging = vk::Buffer::null();
        guard.smem = vk::DeviceMemory::null();
        guard.image = vk::Image::null();
        guard.memory = vk::DeviceMemory::null();
        guard.view = vk::ImageView::null();
        guard.sampler = vk::Sampler::null();
        Ok(PendingUpload {
            staging,
            smem,
            texture: Texture {
                image,
                view,
                sampler,
                memory,
                width,
                height,
            },
        })
    }

    pub fn destroy(&self, gpu: &Gpu) {
        unsafe {
            let d = &gpu.device;
            d.destroy_sampler(self.sampler, None);
            d.destroy_image_view(self.view, None);
            d.destroy_image(self.image, None);
            crate::devmem::untrack(self.memory);
            d.free_memory(self.memory, None);
        }
    }

    /// A blank R8 coverage image of `side` × `side` — the persistent glyph atlas.
    ///
    /// See [`new_glyph_atlas`](Self::new_glyph_atlas), of which this is the mask half.
    pub fn new_coverage_atlas(gpu: &Gpu, pool: vk::CommandPool, side: u32) -> Result<Self> {
        Self::new_glyph_atlas(gpu, pool, side, vk::Format::R8_UNORM)
    }

    /// A blank premultiplied-RGBA image of `side` × `side` — the colour half of the glyph atlas,
    /// where COLRv1 emoji land ([`crate::colr`]). Four times the bytes per texel, so it is created
    /// only once a colour glyph actually appears.
    pub fn new_color_atlas(gpu: &Gpu, pool: vk::CommandPool, side: u32) -> Result<Self> {
        Self::new_glyph_atlas(gpu, pool, side, vk::Format::R8G8B8A8_UNORM)
    }

    /// A blank `side` × `side` glyph atlas in `format`, `SAMPLED | TRANSFER_DST` with a NEAREST
    /// sampler, left in `SHADER_READ_ONLY_OPTIMAL` and zero-filled.
    ///
    /// Unlike [`from_coverage`](Self::from_coverage) this uploads nothing: glyphs are copied in
    /// afterwards, region by region, by [`upload_coverage_regions`](Self::upload_coverage_regions).
    /// It is immediately safe to sample (all zero coverage = fully transparent), so a run whose
    /// glyphs are all already resident needs no GPU work at all.
    ///
    /// Zeroing is done with a clear rather than a host copy so no `side`-squared staging buffer is
    /// allocated (2048² would be a 4 MiB mappable blob, the allocation type that pressures the
    /// Venus host).
    fn new_glyph_atlas(
        gpu: &Gpu,
        pool: vk::CommandPool,
        side: u32,
        format: vk::Format,
    ) -> Result<Self> {
        let _timed = crate::stats::creating();
        let device = &gpu.device;
        let mut guard = UploadGuard::new(device);

        let image_ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(format)
            .extent(vk::Extent3D {
                width: side,
                height: side,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(vk::ImageUsageFlags::SAMPLED | vk::ImageUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { device.create_image(&image_ci, None) }.context("atlas image")?;
        guard.image = image;
        let ireq = unsafe { device.get_image_memory_requirements(image) };
        let memory = gpu.allocate(ireq, vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
        guard.memory = memory;
        unsafe { device.bind_image_memory(image, memory, 0)? };

        let view_ci = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format)
            .components(vk::ComponentMapping::default())
            .subresource_range(COLOR_RANGE);
        let view = unsafe { device.create_image_view(&view_ci, None) }.context("atlas view")?;
        guard.view = view;

        // NEAREST: glyph coverage is placed at whole pixels and sampled 1:1, so filtering would
        // only blur it.
        let sampler_ci = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::NEAREST)
            .min_filter(vk::Filter::NEAREST)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE);
        let sampler = unsafe { device.create_sampler(&sampler_ci, None) }.context("sampler")?;
        guard.sampler = sampler;

        // Creation ends here. What follows is a submit + fence wait, which the `Transition` site
        // already reports — leaving the timer running would count the same milliseconds twice, in
        // two clauses of the same log line.
        drop(_timed);

        let result = gpu.run_commands(pool, crate::stats::SubmitSite::Transition, |cbuf| unsafe {
            transition(
                device,
                cbuf,
                image,
                vk::ImageLayout::UNDEFINED,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::AccessFlags::empty(),
                vk::AccessFlags::TRANSFER_WRITE,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
            );
            device.cmd_clear_color_image(
                cbuf,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &vk::ClearColorValue { float32: [0.; 4] },
                std::slice::from_ref(&COLOR_RANGE),
            );
            transition(
                device,
                cbuf,
                image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
                vk::AccessFlags::TRANSFER_WRITE,
                vk::AccessFlags::SHADER_READ,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
            );
        });

        let texture = Texture {
            image,
            view,
            sampler,
            memory,
            width: side,
            height: side,
        };
        // Past the fallible steps: the handles belong to `texture` now.
        guard.image = vk::Image::null();
        guard.memory = vk::DeviceMemory::null();
        guard.view = vk::ImageView::null();
        guard.sampler = vk::Sampler::null();

        match result {
            Ok(()) => Ok(texture),
            Err(err) => {
                texture.destroy(gpu);
                Err(err)
            }
        }
    }

    /// Copy freshly rasterized glyphs into this atlas: one `(x, y, w, h, coverage)` region each,
    /// all in a single command buffer, so a run that missed on several glyphs still costs one GPU
    /// round trip. No-op for an empty `regions`, which is the common case once the alphabet in use
    /// is resident — that is the whole point of a persistent atlas.
    ///
    /// The regions must not overlap and must lie inside the image; the atlas allocator guarantees
    /// both. Everything outside them keeps its contents (`SHADER_READ_ONLY_OPTIMAL` in, same out),
    /// so glyphs uploaded by earlier calls survive.
    pub fn upload_coverage_regions(
        &self,
        gpu: &Arc<Gpu>,
        pool: vk::CommandPool,
        regions: &[CoverageRegion<'_>],
    ) -> Result<()> {
        let Some(staged) = self.stage_coverage_regions(gpu, regions)? else {
            return Ok(());
        };
        let image = self.image;
        gpu.run_commands(pool, crate::stats::SubmitSite::UploadGlyphs, |cbuf| {
            record_coverage_copy(&gpu.device, cbuf, image, &staged)
        })
        // `staged` frees itself here; `run_commands` already waited (and drained the device on a
        // wait error), so no copy can still be reading it.
    }

    /// The host half of [`upload_coverage_regions`](Self::upload_coverage_regions): allocate the
    /// staging buffer and write every region's coverage into it, recording **no** commands. The
    /// returned [`GlyphStaging`] carries what a later [`record_coverage_copy`] needs, and frees
    /// itself when dropped — so whoever ends up owning it (a one-shot submit, a frame's in-flight
    /// record) does not have to remember to.
    ///
    /// `Ok(None)` for an empty `regions` — the common case once the alphabet in use is resident.
    pub fn stage_coverage_regions(
        &self,
        gpu: &Arc<Gpu>,
        regions: &[CoverageRegion<'_>],
    ) -> Result<Option<GlyphStaging>> {
        if regions.is_empty() {
            return Ok(None);
        }
        let device = &gpu.device;

        // One staging buffer for every region, concatenated; `buffer_offset` picks each out.
        let total: vk::DeviceSize = regions.iter().map(|r| r.coverage.len() as u64).sum();
        crate::stats::uploaded(total);
        let mut guard = UploadGuard::new(device);
        let staging_ci = vk::BufferCreateInfo::default()
            .size(total)
            .usage(vk::BufferUsageFlags::TRANSFER_SRC)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let staging =
            unsafe { device.create_buffer(&staging_ci, None) }.context("atlas staging")?;
        guard.staging = staging;
        let sreq = unsafe { device.get_buffer_memory_requirements(staging) };
        let smem = gpu.allocate(
            sreq,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        )?;
        guard.smem = smem;

        let mut copies = Vec::with_capacity(regions.len());
        unsafe {
            device.bind_buffer_memory(staging, smem, 0)?;
            let base = device
                .map_memory(smem, 0, total, vk::MemoryMapFlags::empty())
                .context("map atlas staging")? as *mut u8;
            let mut offset: vk::DeviceSize = 0;
            for r in regions {
                debug_assert_eq!(
                    r.coverage.len(),
                    (r.w * r.h * r.texel_bytes) as usize,
                    "bitmap size does not match the region"
                );
                std::ptr::copy_nonoverlapping(
                    r.coverage.as_ptr(),
                    base.add(offset as usize),
                    r.coverage.len(),
                );
                copies.push(
                    vk::BufferImageCopy::default()
                        .buffer_offset(offset)
                        .buffer_row_length(r.w)
                        .buffer_image_height(r.h)
                        .image_subresource(vk::ImageSubresourceLayers {
                            aspect_mask: vk::ImageAspectFlags::COLOR,
                            mip_level: 0,
                            base_array_layer: 0,
                            layer_count: 1,
                        })
                        .image_offset(vk::Offset3D {
                            x: r.x as i32,
                            y: r.y as i32,
                            z: 0,
                        })
                        .image_extent(vk::Extent3D {
                            width: r.w,
                            height: r.h,
                            depth: 1,
                        }),
                );
                offset += r.coverage.len() as u64;
            }
            device.unmap_memory(smem);
        }

        // The handles belong to `GlyphStaging` now; disarm the guard.
        guard.staging = vk::Buffer::null();
        guard.smem = vk::DeviceMemory::null();
        Ok(Some(GlyphStaging {
            gpu: gpu.clone(),
            buffer: staging,
            memory: smem,
            copies,
        }))
    }
}

/// One glyph's bitmap and where it goes in the atlas, for
/// [`Texture::upload_coverage_regions`].
pub struct CoverageRegion<'a> {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    /// Tightly packed `w * h * texel_bytes`: R8 coverage for a mask glyph, premultiplied RGBA for
    /// a colour one.
    pub coverage: &'a [u8],
    /// Bytes per texel of the atlas this region targets — 1 or 4. The copy itself counts in
    /// texels, so this only has to agree with the image's format; it is what makes a region
    /// destined for the wrong atlas fail here rather than upload garbage.
    pub texel_bytes: u32,
}

/// A reusable `HOST_VISIBLE | HOST_COHERENT` staging buffer for repeated transfers, in either
/// direction. Grown on demand and never shrunk; the buffer + memory persist, so re-uploading a
/// client's shm buffer every commit — or reading a frame back every commit — doesn't churn a fresh
/// mappable allocation each time (the mappable-blob type that pressures the Venus host). Not
/// internally synchronized — the renderer serializes access via `&mut self`, and each transfer is
/// fence-waited before the next, so overwriting the mapped bytes is safe. Call [`Staging::destroy`]
/// before dropping (it holds raw Vulkan handles).
pub struct Staging {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    capacity: vk::DeviceSize,
    usage: vk::BufferUsageFlags,
}

impl Staging {
    /// An upload staging buffer (`TRANSFER_SRC`): host writes, GPU reads.
    pub const fn new() -> Self {
        Self::with_usage(vk::BufferUsageFlags::TRANSFER_SRC)
    }

    /// A readback staging buffer (`TRANSFER_DST`): GPU writes, host reads.
    pub const fn new_readback() -> Self {
        Self::with_usage(vk::BufferUsageFlags::TRANSFER_DST)
    }

    const fn with_usage(usage: vk::BufferUsageFlags) -> Self {
        Self {
            buffer: vk::Buffer::null(),
            memory: vk::DeviceMemory::null(),
            capacity: 0,
            usage,
        }
    }

    /// The underlying buffer handle. Null until the first [`Staging::ensure`].
    pub fn buffer(&self) -> vk::Buffer {
        self.buffer
    }

    /// Bytes currently allocated. `ensure` only reallocates when it must grow past this.
    pub fn capacity(&self) -> vk::DeviceSize {
        self.capacity
    }

    /// Ensure the staging holds at least `size` bytes, (re)allocating grow-only if needed. Safe to
    /// realloc synchronously: the renderer fence-waits every upload, so nothing references the old
    /// buffer.
    pub fn ensure(&mut self, gpu: &Gpu, size: vk::DeviceSize) -> Result<()> {
        if size <= self.capacity {
            return Ok(());
        }
        unsafe { self.destroy(&gpu.device) };
        let device = &gpu.device;
        let ci = vk::BufferCreateInfo::default()
            .size(size)
            .usage(self.usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        let buffer = unsafe { device.create_buffer(&ci, None) }.context("staging buffer")?;
        // `buffer` is live but not yet owned by `self`, so destroy it on any error before
        // returning: a failed allocate/bind (host mappable-memory exhaustion — the very
        // pressure this cache targets) must not orphan the handle, or a failing grow would
        // leak one per commit.
        let req = unsafe { device.get_buffer_memory_requirements(buffer) };
        let memory = match gpu.allocate(
            req,
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
        ) {
            Ok(memory) => memory,
            Err(e) => {
                unsafe { device.destroy_buffer(buffer, None) };
                return Err(e);
            }
        };
        if let Err(e) =
            unsafe { device.bind_buffer_memory(buffer, memory, 0) }.context("bind staging")
        {
            unsafe {
                device.destroy_buffer(buffer, None);
                crate::devmem::untrack(memory);
                device.free_memory(memory, None);
            }
            return Err(e);
        }
        self.buffer = buffer;
        self.memory = memory;
        self.capacity = size;
        Ok(())
    }

    /// Copy `data` into the staging (caller must have `ensure`d `data.len()` capacity). Maps and
    /// unmaps around the copy; `HOST_COHERENT` means no explicit flush is needed.
    pub fn write(&self, gpu: &Gpu, data: &[u8]) -> Result<()> {
        assert!(data.len() as vk::DeviceSize <= self.capacity);
        let device = &gpu.device;
        unsafe {
            let ptr = device
                .map_memory(
                    self.memory,
                    0,
                    data.len() as vk::DeviceSize,
                    vk::MemoryMapFlags::empty(),
                )
                .context("map staging")? as *mut u8;
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
            device.unmap_memory(self.memory);
        }
        Ok(())
    }

    /// Copy `len` bytes out of the staging into a fresh `Vec` (caller must have `ensure`d `len`
    /// capacity, and the GPU write must already be complete — the renderer fence-waits). Maps and
    /// unmaps around the copy; `HOST_COHERENT` means no explicit invalidate is needed.
    pub fn read(&self, gpu: &Gpu, len: usize) -> Result<Vec<u8>> {
        assert!(len as vk::DeviceSize <= self.capacity);
        let device = &gpu.device;
        let mut out = vec![0u8; len];
        unsafe {
            let ptr = device
                .map_memory(
                    self.memory,
                    0,
                    len as vk::DeviceSize,
                    vk::MemoryMapFlags::empty(),
                )
                .context("map staging for readback")? as *const u8;
            std::ptr::copy_nonoverlapping(ptr, out.as_mut_ptr(), len);
            device.unmap_memory(self.memory);
        }
        Ok(out)
    }

    /// Free the underlying buffer + memory (idempotent; leaves the staging empty — a later `ensure`
    /// reallocates).
    ///
    /// # Safety
    /// No in-flight GPU work may reference the buffer.
    pub unsafe fn destroy(&mut self, device: &ash::Device) {
        if self.memory != vk::DeviceMemory::null() {
            device.destroy_buffer(self.buffer, None);
            crate::devmem::untrack(self.memory);
            device.free_memory(self.memory, None);
        }
        self.buffer = vk::Buffer::null();
        self.memory = vk::DeviceMemory::null();
        self.capacity = 0;
    }
}

impl Default for Staging {
    fn default() -> Self {
        Self::new()
    }
}

/// A glyph-atlas staging buffer with its pixels already written and its copy regions computed,
/// waiting for someone to record the copy ([`record_coverage_copy`]) into a command buffer.
///
/// Owns its buffer + memory and frees them on drop, holding an `Arc<Gpu>` to guarantee the device
/// outlives them — the same shape as `VkSubmitFence`. That is what lets the *frame* decide, at
/// submit time, whether this lives in an in-flight record or dies with the fence wait, without any
/// retirement path having to know a glyph staging exists.
///
/// **It must outlive the submit of the command buffer its copy was recorded into.** Dropping it
/// earlier leaves the GPU reading freed memory — legal Vulkan as far as any validation layer is
/// concerned, and invisible until glyphs come out as garbage.
pub struct GlyphStaging {
    gpu: Arc<Gpu>,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    copies: Vec<vk::BufferImageCopy>,
}

impl Drop for GlyphStaging {
    fn drop(&mut self) {
        unsafe {
            self.gpu.device.destroy_buffer(self.buffer, None);
            crate::devmem::untrack(self.memory);
            self.gpu.device.free_memory(self.memory, None);
        }
    }
}

/// Record a staged glyph upload into `cbuf`: barrier out of the sampleable layout, copy every
/// region, barrier back. Must be called **outside a render pass** (copies and layout transitions
/// both are), and the second barrier is what orders the copy before any glyph draw later in the
/// same command buffer.
///
/// Split out of [`Texture::upload_coverage_regions`] so the same recording serves a standalone
/// submit and a fold into a frame's own command buffer.
pub fn record_coverage_copy(
    device: &ash::Device,
    cbuf: vk::CommandBuffer,
    image: vk::Image,
    staged: &GlyphStaging,
) {
    unsafe {
        // Preserve what is already in the atlas: transition FROM the sampleable layout the image is
        // left in, not from UNDEFINED (which would license discarding it). The image is never in
        // UNDEFINED here — `new_coverage_atlas` transitions it at creation.
        transition(
            device,
            cbuf,
            image,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::AccessFlags::SHADER_READ,
            vk::AccessFlags::TRANSFER_WRITE,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
            vk::PipelineStageFlags::TRANSFER,
        );
        device.cmd_copy_buffer_to_image(
            cbuf,
            staged.buffer,
            image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &staged.copies,
        );
        transition(
            device,
            cbuf,
            image,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::AccessFlags::TRANSFER_WRITE,
            vk::AccessFlags::SHADER_READ,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::FRAGMENT_SHADER,
        );
    }
}

/// Record a freshly-created (`UNDEFINED`) image's staging→image upload into `cbuf`: barrier to
/// `TRANSFER_DST`, copy the full `width*height` buffer, barrier to `SHADER_READ_ONLY`. Shared by
/// [`Texture::upload`] (one image, one submit) and [`TextureBatch`] (N images, one submit) so the
/// barrier/copy stay byte-identical between the single and batched paths.
unsafe fn record_upload_copy(
    device: &ash::Device,
    cbuf: vk::CommandBuffer,
    image: vk::Image,
    staging: vk::Buffer,
    width: u32,
    height: u32,
) {
    record_upload_copy_at(device, cbuf, image, staging, 0, width, height)
}

/// [`record_upload_copy`] against a shared staging buffer, reading from `buffer_offset` rather
/// than the start. One function for both so a batch and a single upload cannot drift in the
/// barriers they record.
unsafe fn record_upload_copy_at(
    device: &ash::Device,
    cbuf: vk::CommandBuffer,
    image: vk::Image,
    staging: vk::Buffer,
    buffer_offset: vk::DeviceSize,
    width: u32,
    height: u32,
) {
    transition(
        device,
        cbuf,
        image,
        vk::ImageLayout::UNDEFINED,
        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        vk::AccessFlags::empty(),
        vk::AccessFlags::TRANSFER_WRITE,
        vk::PipelineStageFlags::TOP_OF_PIPE,
        vk::PipelineStageFlags::TRANSFER,
    );
    let region = vk::BufferImageCopy::default()
        .buffer_offset(buffer_offset)
        .image_subresource(vk::ImageSubresourceLayers {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            mip_level: 0,
            base_array_layer: 0,
            layer_count: 1,
        })
        .image_extent(vk::Extent3D {
            width,
            height,
            depth: 1,
        });
    device.cmd_copy_buffer_to_image(
        cbuf,
        staging,
        image,
        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        &[region],
    );
    transition(
        device,
        cbuf,
        image,
        vk::ImageLayout::TRANSFER_DST_OPTIMAL,
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        vk::AccessFlags::TRANSFER_WRITE,
        vk::AccessFlags::SHADER_READ,
        vk::PipelineStageFlags::TRANSFER,
        vk::PipelineStageFlags::FRAGMENT_SHADER,
    );
}

#[allow(clippy::too_many_arguments)]
unsafe fn transition(
    device: &ash::Device,
    cbuf: vk::CommandBuffer,
    image: vk::Image,
    old: vk::ImageLayout,
    new: vk::ImageLayout,
    src_access: vk::AccessFlags,
    dst_access: vk::AccessFlags,
    src_stage: vk::PipelineStageFlags,
    dst_stage: vk::PipelineStageFlags,
) {
    let barrier = vk::ImageMemoryBarrier::default()
        .old_layout(old)
        .new_layout(new)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .image(image)
        .subresource_range(COLOR_RANGE)
        .src_access_mask(src_access)
        .dst_access_mask(dst_access);
    crate::stats::barriers(1);
    device.cmd_pipeline_barrier(
        cbuf,
        src_stage,
        dst_stage,
        vk::DependencyFlags::empty(),
        &[],
        &[],
        &[barrier],
    );
}
