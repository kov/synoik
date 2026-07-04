//! Stage 0 bring-up spike for the owned Vulkan render stack.
//!
//! Run:  `cargo run -p vk-spike`                        (default ICD → Venus on this VM)
//!       `VK_DRIVER_FILES=/usr/share/vulkan/icd.d/lvp_icd.aarch64.json cargo run -p vk-spike`
//!                                                       (lavapipe → deterministic CPU baseline)
//!
//! This first milestone only proves the pipeline end to end: create a device, clear an
//! offscreen image to a known color, copy it back to host memory, and write a PNG. If this
//! runs and the readback matches the clear color, `vkCreateDevice`/`vkQueueSubmit`/readback
//! all work in this environment — the one empirically-unproven step of the whole plan.

mod gpu;

use std::path::PathBuf;

use anyhow::{Context, Result};
use ash::vk;
use gpu::Gpu;

const WIDTH: u32 = 256;
const HEIGHT: u32 = 128;
/// Distinctive non-gray clear color so a wrong readback is obvious. RGBA8.
const CLEAR: [u8; 4] = [64, 128, 192, 255];

fn main() -> Result<()> {
    eprintln!("vk-spike: enumerating Vulkan devices");
    let gpu = Gpu::new()?;
    eprintln!("vk-spike: using {:?}", gpu.device_name);

    let pixels = render_clear(&gpu, WIDTH, HEIGHT, CLEAR)?;

    let center = sample(&pixels, WIDTH, WIDTH / 2, HEIGHT / 2);
    eprintln!("vk-spike: center pixel = {center:?} (expected {CLEAR:?})");
    anyhow::ensure!(
        center == CLEAR,
        "readback mismatch: got {center:?}, expected {CLEAR:?}"
    );

    let out = artifact_path("clear.png");
    write_png(&out, WIDTH, HEIGHT, &pixels)?;
    eprintln!("vk-spike: wrote {}", out.display());
    eprintln!("vk-spike: OK — device bring-up + submit + readback verified");
    Ok(())
}

/// Clear an offscreen `R8G8B8A8_UNORM` image to `color` and read it back to host RGBA bytes.
fn render_clear(gpu: &Gpu, width: u32, height: u32, color: [u8; 4]) -> Result<Vec<u8>> {
    let device = &gpu.device;
    let size = (width as vk::DeviceSize) * (height as vk::DeviceSize) * 4;

    // --- offscreen render target (device-local) ---
    let image_ci = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(vk::Format::R8G8B8A8_UNORM)
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
    let image = unsafe { device.create_image(&image_ci, None) }.context("create_image")?;
    let img_req = unsafe { device.get_image_memory_requirements(image) };
    let img_mem = allocate(gpu, img_req, vk::MemoryPropertyFlags::DEVICE_LOCAL)?;
    unsafe { device.bind_image_memory(image, img_mem, 0)? };

    // --- host-visible readback buffer ---
    let buf_ci = vk::BufferCreateInfo::default()
        .size(size)
        .usage(vk::BufferUsageFlags::TRANSFER_DST)
        .sharing_mode(vk::SharingMode::EXCLUSIVE);
    let buffer = unsafe { device.create_buffer(&buf_ci, None) }.context("create_buffer")?;
    let buf_req = unsafe { device.get_buffer_memory_requirements(buffer) };
    let buf_mem = allocate(
        gpu,
        buf_req,
        vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT,
    )?;
    unsafe { device.bind_buffer_memory(buffer, buf_mem, 0)? };

    // --- command pool ---
    let pool_ci = vk::CommandPoolCreateInfo::default().queue_family_index(gpu.queue_family);
    let pool = unsafe { device.create_command_pool(&pool_ci, None) }.context("command pool")?;

    let full = vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    };
    // Vulkan's clear color is normalized floats; convert the 8-bit color.
    let clear = vk::ClearColorValue {
        float32: color.map(|c| c as f32 / 255.0),
    };

    gpu.run_commands(pool, |cbuf| unsafe {
        // UNDEFINED -> TRANSFER_DST for the clear.
        barrier_image(
            device,
            cbuf,
            image,
            full,
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
            &clear,
            &[full],
        );
        // TRANSFER_DST -> TRANSFER_SRC for the copy.
        barrier_image(
            device,
            cbuf,
            image,
            full,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            vk::AccessFlags::TRANSFER_WRITE,
            vk::AccessFlags::TRANSFER_READ,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::TRANSFER,
        );
        let region = vk::BufferImageCopy::default()
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
        device.cmd_copy_image_to_buffer(
            cbuf,
            image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            buffer,
            &[region],
        );
        // Make the copy visible to host reads.
        let host = vk::MemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::HOST_READ);
        device.cmd_pipeline_barrier(
            cbuf,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::HOST,
            vk::DependencyFlags::empty(),
            &[host],
            &[],
            &[],
        );
    })?;

    // --- read back ---
    let mut pixels = vec![0u8; size as usize];
    unsafe {
        let ptr = device
            .map_memory(buf_mem, 0, size, vk::MemoryMapFlags::empty())
            .context("map readback buffer")? as *const u8;
        std::ptr::copy_nonoverlapping(ptr, pixels.as_mut_ptr(), size as usize);
        device.unmap_memory(buf_mem);
    }

    unsafe {
        device.destroy_command_pool(pool, None);
        device.destroy_buffer(buffer, None);
        device.free_memory(buf_mem, None);
        device.destroy_image(image, None);
        device.free_memory(img_mem, None);
    }
    Ok(pixels)
}

fn allocate(
    gpu: &Gpu,
    req: vk::MemoryRequirements,
    flags: vk::MemoryPropertyFlags,
) -> Result<vk::DeviceMemory> {
    let index = gpu.find_memory_type(req.memory_type_bits, flags)?;
    let info = vk::MemoryAllocateInfo::default()
        .allocation_size(req.size)
        .memory_type_index(index);
    unsafe { gpu.device.allocate_memory(&info, None) }.context("allocate_memory")
}

/// Record an image layout transition with the given access/stage scopes.
#[allow(clippy::too_many_arguments)]
unsafe fn barrier_image(
    device: &ash::Device,
    cbuf: vk::CommandBuffer,
    image: vk::Image,
    range: vk::ImageSubresourceRange,
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
        .subresource_range(range)
        .src_access_mask(src_access)
        .dst_access_mask(dst_access);
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

fn sample(pixels: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * width + x) * 4) as usize;
    [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
}

fn artifact_path(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("artifacts");
    let _ = std::fs::create_dir_all(&dir);
    dir.join(name)
}

fn write_png(path: &std::path::Path, width: u32, height: u32, rgba: &[u8]) -> Result<()> {
    let file = std::fs::File::create(path).with_context(|| format!("create {}", path.display()))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(rgba)?;
    Ok(())
}
