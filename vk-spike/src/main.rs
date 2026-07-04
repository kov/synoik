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

mod blur;
mod gpu;
mod pango_ref;
mod render;
mod text;
mod texture;

use std::path::PathBuf;

use anyhow::{Context, Result};
use ash::vk;
use blur::BlurChain;
use gpu::Gpu;
use render::{QuadPipeline, QuadPush, RenderTarget};
use text::{build_text, TextRenderer};
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

    blur_demo(&gpu)?;
    text_demo(&gpu)?;
    Ok(())
}

const TEXT: &str = "Activities 12:34";
const TEXT_PX: f32 = 13.0;
const TW: u32 = 200;
const TH: u32 = 32;
const TEXT_BG: [u8; 4] = [24, 24, 28, 255];
const TEXT_FG: [u8; 4] = [235, 235, 235, 255];
const TEXT_ORIGIN: (f32, f32) = (10.0, 8.0);

/// Render our hinted swash-atlas text into a Vulkan target, then stack the pango/cairo reference
/// below it into text.png for a side-by-side 1x crispness comparison.
fn text_demo(gpu: &Gpu) -> Result<()> {
    let pool_ci = vk::CommandPoolCreateInfo::default().queue_family_index(gpu.queue_family);
    let pool = unsafe { gpu.device.create_command_pool(&pool_ci, None) }.context("text pool")?;

    let atlas = build_text(gpu, pool, TEXT, TEXT_PX)?;
    anyhow::ensure!(!atlas.glyphs.is_empty(), "no glyphs were shaped/rasterized");

    let target = RenderTarget::new(gpu, TW, TH)?;
    let set_layout = render::sampler_set_layout(gpu)?;
    let renderer = TextRenderer::new(gpu, target.render_pass, target.extent(), set_layout)?;

    // Descriptor set pointing at the atlas.
    let sizes = [vk::DescriptorPoolSize::default()
        .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
        .descriptor_count(1)];
    let dp_ci = vk::DescriptorPoolCreateInfo::default()
        .max_sets(1)
        .pool_sizes(&sizes);
    let desc_pool = unsafe { gpu.device.create_descriptor_pool(&dp_ci, None) }.context("pool")?;
    let alloc = vk::DescriptorSetAllocateInfo::default()
        .descriptor_pool(desc_pool)
        .set_layouts(std::slice::from_ref(&set_layout));
    let set = unsafe { gpu.device.allocate_descriptor_sets(&alloc) }.context("set")?[0];
    let img = vk::DescriptorImageInfo::default()
        .sampler(atlas.texture.sampler)
        .image_view(atlas.texture.view)
        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
    let write = vk::WriteDescriptorSet::default()
        .dst_set(set)
        .dst_binding(0)
        .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
        .image_info(std::slice::from_ref(&img));
    unsafe { gpu.device.update_descriptor_sets(&[write], &[]) };

    let dims = [TW as f32, TH as f32];
    gpu.run_commands(pool, |cbuf| {
        target.begin(gpu, cbuf, unorm(TEXT_BG));
        renderer.draw(gpu, cbuf, set, &atlas, TEXT_ORIGIN, dims, unorm(TEXT_FG));
        unsafe { gpu.device.cmd_end_render_pass(cbuf) };
    })?;
    let ours = target.read_back(gpu, pool)?;

    // How much ink did we actually rasterize? (bright text pixels over the dark bg)
    let bright = ours
        .chunks_exact(4)
        .filter(|p| p[0] > 150 && p[1] > 150 && p[2] > 150)
        .count();
    eprintln!("vk-spike: text bright pixels = {bright}");
    check(
        "text bg corner is dark",
        sample_at(&ours, TW, 2, 2),
        TEXT_BG,
        3,
    )?;
    anyhow::ensure!(
        bright > 40,
        "expected visible glyph ink, got {bright} bright pixels"
    );

    // Reference render (pango/cairo), stacked below ours into one PNG.
    let reference = pango_ref::render(
        TEXT,
        TW as i32,
        TH as i32,
        TEXT_PX as f64,
        [TEXT_FG[0], TEXT_FG[1], TEXT_FG[2]],
        [TEXT_BG[0], TEXT_BG[1], TEXT_BG[2]],
        (TEXT_ORIGIN.0 as f64, TEXT_ORIGIN.1 as f64),
    )?;
    let mut combined = ours.clone();
    combined.extend_from_slice(&reference);

    let path = artifact_path("text.png");
    write_png(&path, TW, TH * 2, &combined)?;
    eprintln!(
        "vk-spike: wrote {} (top: swash atlas, bottom: pango ref)",
        path.display()
    );
    eprintln!("vk-spike: OK — hinted glyph-atlas text verified");

    unsafe {
        gpu.device.destroy_descriptor_pool(desc_pool, None);
        gpu.device.destroy_descriptor_set_layout(set_layout, None);
        gpu.device.destroy_command_pool(pool, None);
    }
    renderer.destroy(gpu);
    atlas.texture.destroy(gpu);
    target.destroy(gpu);
    Ok(())
}

fn sample_at(pixels: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * width + x) * 4) as usize;
    [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
}

const SRC_W: u32 = 192;
const SRC_H: u32 = 128;
const EDGE_RED: [u8; 4] = [230, 40, 40, 255];
const EDGE_BLUE: [u8; 4] = [40, 60, 230, 255];
const BLUR_PASSES: usize = 3;
const BLUR_OFFSET: f32 = 3.0;

/// Blur a hard vertical red|blue edge and check the step became a smooth gradient.
fn blur_demo(gpu: &Gpu) -> Result<()> {
    let pool_ci = vk::CommandPoolCreateInfo::default().queue_family_index(gpu.queue_family);
    let pool = unsafe { gpu.device.create_command_pool(&pool_ci, None) }.context("blur pool")?;

    // Source: left half red, right half blue (a maximal high-contrast step).
    let mut src = vec![0u8; (SRC_W * SRC_H * 4) as usize];
    for y in 0..SRC_H {
        for x in 0..SRC_W {
            let c = if x < SRC_W / 2 { EDGE_RED } else { EDGE_BLUE };
            let i = ((y * SRC_W + x) * 4) as usize;
            src[i..i + 4].copy_from_slice(&c);
        }
    }
    let source = Texture::from_rgba(gpu, pool, SRC_W, SRC_H, &src, vk::Filter::LINEAR)?;
    let chain = BlurChain::new(gpu, &source, BLUR_PASSES)?;

    gpu.run_commands(pool, |cbuf| chain.record(gpu, cbuf, BLUR_OFFSET))?;
    let (ow, oh) = chain.output_size();
    let out = chain.read_output(gpu, pool)?;

    let at = |x: u32, y: u32| -> [u8; 4] {
        let i = ((y * ow + x) * 4) as usize;
        [out[i], out[i + 1], out[i + 2], out[i + 3]]
    };
    let cy = oh / 2;
    let left = at(8, cy);
    let right = at(ow - 8, cy);
    let center = at(ow / 2, cy);
    let step_l = at(ow / 2 - 1, cy);
    let step_r = at(ow / 2, cy);
    eprintln!("vk-spike: blur left={left:?} center={center:?} right={right:?}");

    // Far from the edge the colors survive; at the edge they mix; the hard step is gone.
    anyhow::ensure!(
        left[0] as i16 - left[2] as i16 > 40,
        "blur: left should stay red-dominant, got {left:?}"
    );
    anyhow::ensure!(
        right[2] as i16 - right[0] as i16 > 40,
        "blur: right should stay blue-dominant, got {right:?}"
    );
    anyhow::ensure!(
        center[0] > 60 && center[2] > 60 && (center[0] as i16 - center[2] as i16).abs() < 80,
        "blur: center should be a red/blue mix, got {center:?}"
    );
    anyhow::ensure!(
        (step_l[0] as i16 - step_r[0] as i16).abs() < 40,
        "blur: the hard step should be smoothed, got {step_l:?} vs {step_r:?}"
    );
    anyhow::ensure!(
        [left, center, right].iter().all(|p| p[3] == 255),
        "blur: output should stay opaque"
    );

    let path = artifact_path("blur.png");
    write_png(&path, ow, oh, &out)?;
    eprintln!("vk-spike: wrote {}", path.display());
    eprintln!("vk-spike: OK — dual-kawase blur verified");

    chain.destroy(gpu);
    source.destroy(gpu);
    unsafe { gpu.device.destroy_command_pool(pool, None) };
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
