//! Stage 0 bring-up spike for the owned Vulkan render stack.
//!
//! Run:  `cargo run -p vk-spike`                        (default ICD → Venus on this VM)
//!       `VK_DRIVER_FILES=/usr/share/vulkan/icd.d/lvp_icd.aarch64.json cargo run -p vk-spike`
//!                                                       (lavapipe → deterministic CPU baseline)
//!
//! Milestone 2: a real graphics pipeline. We render into an offscreen image through a render
//! pass — a solid-fill quad and an SDF rounded-rectangle (ported from niri's corner-rounding
//! shader) — then read it back, assert structural pixel invariants, and write a PNG. Proves the
//! whole rasterization path (render pass, pipeline, SPIR-V, push constants, alpha blending) on
//! both Venus and lavapipe, headless.

mod gpu;
mod render;

use std::path::PathBuf;

use anyhow::{Context, Result};
use ash::vk;
use gpu::Gpu;
use render::{QuadPipeline, QuadPush, RenderTarget};

const QUAD_VERT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/quad.vert.spv"));
const SOLID_FRAG: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/solid.frag.spv"));
const SDF_FRAG: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/sdf_rect.frag.spv"));

const WIDTH: u32 = 256;
const HEIGHT: u32 = 128;
const CLEAR: [u8; 4] = [32, 32, 32, 255];
const RED: [u8; 4] = [220, 60, 60, 255];
const GREEN: [u8; 4] = [60, 200, 90, 255];

fn main() -> Result<()> {
    eprintln!("vk-spike: enumerating Vulkan devices");
    let gpu = Gpu::new()?;
    eprintln!("vk-spike: using {:?}", gpu.device_name);

    let pixels = render_scene(&gpu)?;

    // Structural invariants — resolution-independent, so they hold on any conformant driver
    // regardless of AA/rounding rasterization differences.
    let bg = sample(&pixels, 8, 8);
    let solid = sample(&pixels, 64, 64);
    let rounded_center = sample(&pixels, 196, 64);
    let rounded_corner = sample(&pixels, 154, 22); // outside the rounded corner → background
    eprintln!("vk-spike: bg={bg:?} solid={solid:?} rounded_center={rounded_center:?} rounded_corner={rounded_corner:?}");

    check("background is clear", bg, CLEAR, 2)?;
    check("solid quad is red", solid, RED, 2)?;
    check("rounded-rect body is green", rounded_center, GREEN, 3)?;
    check(
        "rounded-rect corner is clipped away",
        rounded_corner,
        CLEAR,
        4,
    )?;

    let out = artifact_path("scene.png");
    write_png(&out, WIDTH, HEIGHT, &pixels)?;
    eprintln!("vk-spike: wrote {}", out.display());
    eprintln!("vk-spike: OK — render pass + solid + SDF rounded-rect verified");
    Ok(())
}

fn render_scene(gpu: &Gpu) -> Result<Vec<u8>> {
    let target = RenderTarget::new(gpu, WIDTH, HEIGHT)?;
    let solid = QuadPipeline::new(
        gpu,
        target.render_pass,
        target.extent(),
        QUAD_VERT,
        SOLID_FRAG,
    )?;
    let rounded = QuadPipeline::new(
        gpu,
        target.render_pass,
        target.extent(),
        QUAD_VERT,
        SDF_FRAG,
    )?;

    let pool_ci = vk::CommandPoolCreateInfo::default().queue_family_index(gpu.queue_family);
    let pool = unsafe { gpu.device.create_command_pool(&pool_ci, None) }.context("command pool")?;

    let dims = [WIDTH as f32, HEIGHT as f32];
    let solid_quad = QuadPush {
        origin: [24.0, 20.0],
        size: [80.0, 88.0],
        target: dims,
        corner_radius: 0.0,
        _pad0: 0.0,
        color: unorm(RED),
    };
    let rounded_quad = QuadPush {
        origin: [152.0, 20.0],
        size: [88.0, 88.0],
        target: dims,
        corner_radius: 26.0,
        _pad0: 0.0,
        color: unorm(GREEN),
    };

    gpu.run_commands(pool, |cbuf| {
        target.begin(gpu, cbuf, unorm(CLEAR));
        solid.draw(gpu, cbuf, &solid_quad);
        rounded.draw(gpu, cbuf, &rounded_quad);
        unsafe { gpu.device.cmd_end_render_pass(cbuf) };
    })?;

    let pixels = target.read_back(gpu, pool)?;

    unsafe { gpu.device.destroy_command_pool(pool, None) };
    solid.destroy(gpu);
    rounded.destroy(gpu);
    target.destroy(gpu);
    Ok(pixels)
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
