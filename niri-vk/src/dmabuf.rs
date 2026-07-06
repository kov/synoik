//! Stage 1: import a *foreign* dmabuf (allocated by GBM — the real Wayland-client path) as a
//! VkImage with an explicit DRM format modifier, so the renderer can sample it. This is the #1
//! front-loaded risk of the owned-Vulkan port: client window buffers arrive as dmabufs from other
//! GPU drivers, and we must import + sample them zero-copy on Venus.
//!
//! Scoped to the **LINEAR** modifier — Venus exposes only `0x0` for our formats (Stage 0 probes).
//! lavapipe advertises the dmabuf extensions but is a CPU device (not the GPU behind the DRM
//! node), so the caller skips the demo there.
//!
//! Flow: GBM allocates a LINEAR ARGB8888 buffer; we CPU-fill it with a known 4-quadrant pattern
//! through GBM's map; export it as a dmabuf FD + modifier + plane layout; import into Vulkan via
//! `VK_EXT_external_memory_dma_buf` + `VK_EXT_image_drm_format_modifier` +
//! `VK_KHR_external_memory_fd`; acquire the content from the FOREIGN queue family; then sampling it
//! back and matching the pattern proves the path end-to-end.

use std::fs::File;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, IntoRawFd, OwnedFd};

use anyhow::{anyhow, Context, Result};
use ash::vk;
use gbm::{BufferObjectFlags, Device as GbmDevice, Format, Modifier};

use crate::gpu::Gpu;
use crate::render::COLOR_RANGE;

/// Render node we allocate foreign buffers on (unprivileged, world-rw on this VM).
const RENDER_NODE: &str = "/dev/dri/renderD128";
/// `DRM_FORMAT_MOD_LINEAR` — the only modifier Venus exposes for our formats.
const DRM_FORMAT_MOD_LINEAR: u64 = 0;
/// ARGB8888 (DRM) little-endian memory order is [B, G, R, A] → VK_FORMAT_B8G8R8A8_UNORM.
const IMPORT_FORMAT: vk::Format = vk::Format::B8G8R8A8_UNORM;

/// Log a GBM allocation attempt and convert its result to `Option` for `.or_else` chaining.
fn log_try<T>(label: &str, r: std::io::Result<T>) -> Option<T> {
    match r {
        Ok(v) => {
            eprintln!("niri-vk: gbm OK  {label}");
            Some(v)
        }
        Err(e) => {
            eprintln!("niri-vk: gbm --  {label}: {e}");
            None
        }
    }
}

/// A GBM-allocated LINEAR buffer, CPU-filled and exported as a dmabuf.
pub struct ForeignBuffer {
    // Drop order matters: the gbm_bo holds a ref to the gbm_device, so `bo` must drop before
    // `_device`. Struct fields drop top-to-bottom, so keep `bo` above `_device`.
    _bo: gbm::BufferObject<()>,
    _device: GbmDevice<File>,
    pub width: u32,
    pub height: u32,
    pub modifier: u64,
    pub stride: u32,
    pub offset: u32,
    /// The exported dmabuf FD; we dup it per import so this stays valid until drop.
    fd: OwnedFd,
}

impl ForeignBuffer {
    /// Allocate a LINEAR ARGB8888 buffer and CPU-fill four solid quadrants (row-major TL, TR, BL,
    /// BR; each an RGBA color written in ARGB8888 memory order).
    pub fn allocate_filled(width: u32, height: u32, quadrants: [[u8; 4]; 4]) -> Result<Self> {
        let file = File::options()
            .read(true)
            .write(true)
            .open(RENDER_NODE)
            .with_context(|| format!("open {RENDER_NODE}"))?;
        let device = GbmDevice::new(file).context("gbm_create_device")?;

        // Get a CPU-writable LINEAR buffer we can import as an explicit-modifier-0 image.
        // virtio-gpu gbm is picky about the flag/format combo (SCANOUT is invalid on a
        // render node, some usage/format pairs EINVAL), so try a matrix of
        // LINEAR-guaranteeing combos and take the first that works — logging each so the
        // winner and the rejects are visible. B/XRGB8888 both map to
        // VK_FORMAT_B8G8R8A8_UNORM with [B,G,R,A] byte order.
        let rl = BufferObjectFlags::RENDERING | BufferObjectFlags::LINEAR;
        let mut bo = Option::<gbm::BufferObject<()>>::None
            .or_else(|| {
                log_try(
                    "ARGB8888 RENDERING|LINEAR",
                    device.create_buffer_object::<()>(width, height, Format::Argb8888, rl),
                )
            })
            .or_else(|| {
                log_try(
                    "XRGB8888 RENDERING|LINEAR",
                    device.create_buffer_object::<()>(width, height, Format::Xrgb8888, rl),
                )
            })
            .or_else(|| {
                log_try(
                    "ARGB8888 LINEAR|WRITE",
                    device.create_buffer_object::<()>(
                        width,
                        height,
                        Format::Argb8888,
                        BufferObjectFlags::LINEAR | BufferObjectFlags::WRITE,
                    ),
                )
            })
            .or_else(|| {
                log_try(
                    "ARGB8888 LINEAR",
                    device.create_buffer_object::<()>(
                        width,
                        height,
                        Format::Argb8888,
                        BufferObjectFlags::LINEAR,
                    ),
                )
            })
            .or_else(|| {
                log_try(
                    "ARGB8888 modifiers2[LINEAR]",
                    device.create_buffer_object_with_modifiers2::<()>(
                        width,
                        height,
                        Format::Argb8888,
                        [Modifier::Linear].into_iter(),
                        BufferObjectFlags::empty(),
                    ),
                )
            })
            .context("gbm allocate LINEAR buffer: every strategy failed (see attempts above)")?;
        eprintln!("niri-vk: gbm buffer reported modifier {:?}", bo.modifier());

        let modifier = match bo.modifier() {
            // USE_LINEAR forces a linear layout; some drivers report INVALID rather than LINEAR.
            Modifier::Linear | Modifier::Invalid => DRM_FORMAT_MOD_LINEAR,
            other => anyhow::bail!("gbm returned non-LINEAR modifier {other:?}"),
        };
        anyhow::ensure!(
            bo.plane_count() == 1,
            "expected 1 plane for LINEAR ARGB8888"
        );

        // CPU-fill through the map (virtio-gpu backs this with a host transfer on unmap). We write
        // at the *map* stride; GBM lands it in the real buffer at `bo.stride()`, which is what we
        // hand Vulkan — so the two strides need not be equal.
        bo.map_mut(0, 0, width, height, |m| -> Result<()> {
            let stride = m.stride() as usize;
            let buf = m.buffer_mut();
            for y in 0..height as usize {
                let bottom = y >= height as usize / 2;
                for x in 0..width as usize {
                    let right = x >= width as usize / 2;
                    let [r, g, b, a] = quadrants[bottom as usize * 2 + right as usize];
                    let i = y * stride + x * 4;
                    buf[i] = b;
                    buf[i + 1] = g;
                    buf[i + 2] = r;
                    buf[i + 3] = a;
                }
            }
            Ok(())
        })
        .context("gbm_bo_map")??;

        let fd = bo.fd().map_err(|e| anyhow!("gbm_bo_get_fd: {e}"))?;
        let stride = bo.stride();
        let offset = bo.offset(0);

        Ok(ForeignBuffer {
            _bo: bo,
            _device: device,
            width,
            height,
            modifier,
            stride,
            offset,
            fd,
        })
    }

    /// The exported dmabuf FD (borrowed; valid until this `ForeignBuffer` drops). Callers that need
    /// to hand it to an API that consumes an `OwnedFd` should `try_clone_to_owned` it.
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

/// A dmabuf imported as a sampled VkImage (view + sampler), ready to bind to a descriptor set.
pub struct ImportedImage {
    pub image: vk::Image,
    pub view: vk::ImageView,
    pub sampler: vk::Sampler,
    memory: vk::DeviceMemory,
}

impl ImportedImage {
    pub fn import(gpu: &Gpu, pool: vk::CommandPool, buf: &ForeignBuffer) -> Result<Self> {
        let device = &gpu.device;

        // Create a DRM-format-modifier image with an explicit plane layout matching the dmabuf,
        // declared as externally-shared dma_buf memory.
        let plane_layout = vk::SubresourceLayout {
            offset: buf.offset as u64,
            size: 0,
            row_pitch: buf.stride as u64,
            array_pitch: 0,
            depth_pitch: 0,
        };
        let mut mod_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(buf.modifier)
            .plane_layouts(std::slice::from_ref(&plane_layout));
        let mut ext_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let image_ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(IMPORT_FORMAT)
            .extent(vk::Extent3D {
                width: buf.width,
                height: buf.height,
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
        let image =
            unsafe { device.create_image(&image_ci, None) }.context("create imported image")?;

        let mem_req = unsafe { device.get_image_memory_requirements(image) };

        // Prefer the memory types the driver says are valid for importing this fd, but treat the
        // query as best-effort — on Venus it can reject a perfectly importable dmabuf, so fall back
        // to the image's own requirements and let vkAllocateMemory be the real test.
        let ext_fd = ash::khr::external_memory_fd::Device::new(&gpu.instance, &gpu.device);
        let mut type_bits = mem_req.memory_type_bits;
        let mut fd_props = vk::MemoryFdPropertiesKHR::default();
        match unsafe {
            ext_fd.get_memory_fd_properties(
                vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                buf.fd.as_raw_fd(),
                &mut fd_props,
            )
        } {
            Ok(()) => type_bits &= fd_props.memory_type_bits,
            Err(e) => eprintln!(
                "niri-vk: vkGetMemoryFdPropertiesKHR failed ({e:?}); using image memory_type_bits"
            ),
        }
        anyhow::ensure!(type_bits != 0, "no importable memory type for the dmabuf");
        let mem_type = type_bits.trailing_zeros();

        // Import consumes the fd on success, so hand Vulkan a fresh dup and keep ours in `buf`.
        let raw_fd = buf
            .fd
            .try_clone()
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
        // On failure `raw_fd` leaks (Vulkan only takes ownership on success) — spike-level.
        let memory = unsafe { device.allocate_memory(&alloc_info, None) }
            .context("import dmabuf memory (vkAllocateMemory)")?;
        unsafe { device.bind_image_memory(image, memory, 0) }.context("bind imported memory")?;

        // Acquire the content: the producer is outside Vulkan, so transfer ownership from the
        // FOREIGN queue family (if that extension is present; else a plain transition). For the
        // LINEAR modifier the bytes survive the UNDEFINED old layout (nothing to detile).
        let (src_qf, dst_qf) = if gpu.supports("VK_EXT_queue_family_foreign") {
            (vk::QUEUE_FAMILY_FOREIGN_EXT, gpu.queue_family)
        } else {
            (vk::QUEUE_FAMILY_IGNORED, vk::QUEUE_FAMILY_IGNORED)
        };
        gpu.run_commands(pool, |cbuf| unsafe {
            let barrier = vk::ImageMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::empty())
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_queue_family_index(src_qf)
                .dst_queue_family_index(dst_qf)
                .image(image)
                .subresource_range(COLOR_RANGE);
            device.cmd_pipeline_barrier(
                cbuf,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier],
            );
        })
        .context("acquire imported image")?;

        let view_ci = vk::ImageViewCreateInfo::default()
            .image(image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(IMPORT_FORMAT)
            .subresource_range(COLOR_RANGE);
        let view =
            unsafe { device.create_image_view(&view_ci, None) }.context("imported image view")?;

        let sampler_ci = vk::SamplerCreateInfo::default()
            .mag_filter(vk::Filter::NEAREST)
            .min_filter(vk::Filter::NEAREST)
            .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
            .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE);
        let sampler =
            unsafe { device.create_sampler(&sampler_ci, None) }.context("imported sampler")?;

        Ok(ImportedImage {
            image,
            view,
            sampler,
            memory,
        })
    }

    pub fn destroy(&self, gpu: &Gpu) {
        unsafe {
            let d = &gpu.device;
            d.destroy_sampler(self.sampler, None);
            d.destroy_image_view(self.view, None);
            d.destroy_image(self.image, None);
            d.free_memory(self.memory, None);
        }
    }
}
