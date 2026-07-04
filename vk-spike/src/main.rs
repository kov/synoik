//! Stage 0 bring-up spike for the owned Vulkan render stack.
//!
//! Run:  `cargo run -p vk-spike`                        (default ICD → Venus on this VM)
//!       `VK_DRIVER_FILES=/usr/share/vulkan/icd.d/lvp_icd.aarch64.json cargo run -p vk-spike`
//!                                                       (lavapipe → deterministic CPU baseline)
//!
//! We render three quads into an offscreen image through a render pass — a solid fill, an SDF
//! rounded-rectangle (ported from niri's corner-rounding shader), and a sampled texture — then
//! read it back, assert structural pixel invariants, and write a PNG. Proves the whole
//! rasterization path (render pass, pipeline, SPIR-V, push constants, alpha blending, descriptor
//! sets / samplers) on both Venus and lavapipe, headless.

mod gpu;
mod render;
mod texture;

use std::path::PathBuf;

use anyhow::{Context, Result};
use ash::vk;
use gpu::Gpu;
use render::{QuadPipeline, QuadPush, RenderTarget};
use texture::Texture;

const QUAD_VERT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/quad.vert.spv"));
const SOLID_FRAG: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/solid.frag.spv"));
const SDF_FRAG: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/sdf_rect.frag.spv"));
const TEX_FRAG: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/texture.frag.spv"));

const WIDTH: u32 = 384;
const HEIGHT: u32 = 128;
const CLEAR: [u8; 4] = [32, 32, 32, 255];
const RED: [u8; 4] = [220, 60, 60, 255];
const GREEN: [u8; 4] = [60, 200, 90, 255];
const WHITE: [u8; 4] = [255, 255, 255, 255];

// 2x2 texture texels (row-major): sampled with NEAREST so each quadrant is one solid texel.
const TEX_TL: [u8; 4] = [230, 50, 50, 255];
const TEX_TR: [u8; 4] = [240, 200, 40, 255];
const TEX_BL: [u8; 4] = [60, 120, 240, 255];
const TEX_BR: [u8; 4] = [240, 240, 240, 255];

fn main() -> Result<()> {
    eprintln!("vk-spike: enumerating Vulkan devices");
    let gpu = Gpu::new()?;
    eprintln!("vk-spike: using {:?}", gpu.device_name);

    let pixels = render_scene(&gpu)?;

    // Structural invariants — resolution-independent, so they hold on any conformant driver
    // regardless of AA/rounding rasterization differences.
    check("background is clear", sample(&pixels, 8, 8), CLEAR, 2)?;
    check("solid quad is red", sample(&pixels, 64, 64), RED, 2)?;
    check(
        "rounded-rect body is green",
        sample(&pixels, 192, 64),
        GREEN,
        3,
    )?;
    check(
        "rounded-rect corner is clipped away",
        sample(&pixels, 147, 23),
        CLEAR,
        4,
    )?;
    // Textured quad: four quadrants show the four texels.
    check("texture top-left", sample(&pixels, 296, 42), TEX_TL, 2)?;
    check("texture top-right", sample(&pixels, 344, 42), TEX_TR, 2)?;
    check("texture bottom-left", sample(&pixels, 296, 86), TEX_BL, 2)?;
    check("texture bottom-right", sample(&pixels, 344, 86), TEX_BR, 2)?;

    let out = artifact_path("scene.png");
    write_png(&out, WIDTH, HEIGHT, &pixels)?;
    eprintln!("vk-spike: wrote {}", out.display());
    eprintln!("vk-spike: OK — render pass + solid + SDF rounded-rect + textured quad verified");
    Ok(())
}

fn render_scene(gpu: &Gpu) -> Result<Vec<u8>> {
    let device = &gpu.device;
    let target = RenderTarget::new(gpu, WIDTH, HEIGHT)?;

    let pool_ci = vk::CommandPoolCreateInfo::default().queue_family_index(gpu.queue_family);
    let pool = unsafe { device.create_command_pool(&pool_ci, None) }.context("command pool")?;

    // Upload the 2x2 texture and bind it to a descriptor set.
    let texels: Vec<u8> = [TEX_TL, TEX_TR, TEX_BL, TEX_BR].concat();
    let tex = Texture::from_rgba(gpu, pool, 2, 2, &texels, vk::Filter::NEAREST)?;
    let set_layout = render::sampler_set_layout(gpu)?;
    let (desc_pool, set) = bind_texture(gpu, set_layout, &tex)?;

    let ext = target.extent();
    let solid = QuadPipeline::new(gpu, target.render_pass, ext, QUAD_VERT, SOLID_FRAG, &[])?;
    let rounded = QuadPipeline::new(gpu, target.render_pass, ext, QUAD_VERT, SDF_FRAG, &[])?;
    let textured = QuadPipeline::new(
        gpu,
        target.render_pass,
        ext,
        QUAD_VERT,
        TEX_FRAG,
        std::slice::from_ref(&set_layout),
    )?;

    let dims = [WIDTH as f32, HEIGHT as f32];
    let column = |x: f32, color: [u8; 4], corner_radius: f32| QuadPush {
        origin: [x, 20.0],
        size: [96.0, 88.0],
        target: dims,
        corner_radius,
        _pad0: 0.0,
        color: unorm(color),
    };
    let solid_quad = column(16.0, RED, 0.0);
    let rounded_quad = column(144.0, GREEN, 26.0);
    let textured_quad = column(272.0, WHITE, 0.0);

    gpu.run_commands(pool, |cbuf| {
        target.begin(gpu, cbuf, unorm(CLEAR));
        solid.draw(gpu, cbuf, &solid_quad, None);
        rounded.draw(gpu, cbuf, &rounded_quad, None);
        textured.draw(gpu, cbuf, &textured_quad, Some(set));
        unsafe { gpu.device.cmd_end_render_pass(cbuf) };
    })?;

    let pixels = target.read_back(gpu, pool)?;

    unsafe {
        device.destroy_descriptor_pool(desc_pool, None);
        device.destroy_descriptor_set_layout(set_layout, None);
        device.destroy_command_pool(pool, None);
    }
    tex.destroy(gpu);
    solid.destroy(gpu);
    rounded.destroy(gpu);
    textured.destroy(gpu);
    target.destroy(gpu);
    Ok(pixels)
}

/// Allocate a one-set descriptor pool and point set 0 / binding 0 at `tex`.
fn bind_texture(
    gpu: &Gpu,
    set_layout: vk::DescriptorSetLayout,
    tex: &Texture,
) -> Result<(vk::DescriptorPool, vk::DescriptorSet)> {
    let device = &gpu.device;
    let sizes = [vk::DescriptorPoolSize::default()
        .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
        .descriptor_count(1)];
    let dp_ci = vk::DescriptorPoolCreateInfo::default()
        .max_sets(1)
        .pool_sizes(&sizes);
    let desc_pool = unsafe { device.create_descriptor_pool(&dp_ci, None) }.context("desc pool")?;

    let alloc = vk::DescriptorSetAllocateInfo::default()
        .descriptor_pool(desc_pool)
        .set_layouts(std::slice::from_ref(&set_layout));
    let set = unsafe { device.allocate_descriptor_sets(&alloc) }.context("alloc desc set")?[0];

    let img = vk::DescriptorImageInfo::default()
        .sampler(tex.sampler)
        .image_view(tex.view)
        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
    let write = vk::WriteDescriptorSet::default()
        .dst_set(set)
        .dst_binding(0)
        .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
        .image_info(std::slice::from_ref(&img));
    unsafe { device.update_descriptor_sets(&[write], &[]) };

    Ok((desc_pool, set))
}

fn unorm(c: [u8; 4]) -> [f32; 4] {
    c.map(|v| v as f32 / 255.0)
}

fn sample(pixels: &[u8], x: u32, y: u32) -> [u8; 4] {
    let i = ((y * WIDTH + x) * 4) as usize;
    [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
}

fn check(what: &str, got: [u8; 4], want: [u8; 4], tol: u8) -> Result<()> {
    let ok = got
        .iter()
        .zip(want.iter())
        .all(|(g, w)| g.abs_diff(*w) <= tol);
    anyhow::ensure!(ok, "{what}: got {got:?}, expected ~{want:?} (tol {tol})");
    Ok(())
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
