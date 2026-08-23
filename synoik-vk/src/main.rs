// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Stage 0 bring-up spike for the owned Vulkan render stack.
//!
//! Run:  `cargo run -p synoik-vk`                         (default ICD → Venus on this VM)
//!       `VK_DRIVER_FILES=/usr/share/vulkan/icd.d/lvp_icd.aarch64.json cargo run -p synoik-vk`
//!                                                       (lavapipe → deterministic CPU baseline)
//! Test: `cargo test -p synoik-vk`                        (same, on whichever ICD the env selects)
//!
//! Three offscreen render demos, each split into a pure `render_*` (returns read-back pixels) and
//! a shared `assert_*` (structural, resolution-independent invariants):
//!   - `scene`: solid + SDF rounded-rect (from niri's corner shader) + sampled-texture quads,
//!   - `blur`:  gaussian blur of a hard edge into a smooth gradient,
//!   - `text`:  hinted swash glyph-atlas text (vs a stacked pango/cairo reference in the PNG).
//!
//! `main` renders + asserts + writes a PNG for each, then prints forward-looking device `probes`.
//! The same render+assert pairs run under `#[test]`, so the invariants double as a dual-ICD CI
//! baseline (lavapipe = deterministic) with no golden images.
//!
//! Proves the whole rasterization path (render pass, pipeline, SPIR-V, push constants, alpha
//! blending, descriptor sets / samplers, ping-pong blur, glyph atlas) on Venus and lavapipe,
//! headless.

#[cfg(feature = "pango-ref")]
mod pango_ref;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use ash::vk;
use synoik_vk::blur::BlurChain;
use synoik_vk::dmabuf::{ForeignBuffer, ImportedImage};
use synoik_vk::gpu::Gpu;
use synoik_vk::probes;
use synoik_vk::render::{self, QuadPipeline, QuadPush, RenderTarget};
use synoik_vk::shaders::{QUAD_VERT, SDF_FRAG, SOLID_FRAG, TEX_FRAG};
use synoik_vk::text::{build_text, TextRenderer};
use synoik_vk::texture::Texture;

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
    eprintln!("synoik-vk: enumerating Vulkan devices");
    let gpu = Arc::new(Gpu::new()?);
    eprintln!("synoik-vk: using {:?}", gpu.device_name);

    let scene = render_scene(&gpu)?;
    assert_scene(&scene)?;
    let out = artifact_path("scene.png");
    write_png(&out, WIDTH, HEIGHT, &scene)?;
    eprintln!("synoik-vk: wrote {}", out.display());
    eprintln!("synoik-vk: OK — render pass + solid + SDF rounded-rect + textured quad verified");

    let blur = render_blur(&gpu)?;
    assert_blur(&blur)?;
    let path = artifact_path("blur.png");
    write_png(&path, SRC_W, SRC_H, &blur)?;
    eprintln!("synoik-vk: wrote {}", path.display());
    eprintln!("synoik-vk: OK — gaussian blur verified");

    let text = render_text(&gpu)?;
    assert_text(&text)?;
    let path = artifact_path("text.png");
    #[cfg(feature = "pango-ref")]
    {
        // Reference render (pango/cairo), stacked below ours for a 1x crispness compare.
        let reference = pango_ref::render(
            TEXT,
            TW as i32,
            TH as i32,
            TEXT_PX as f64,
            [TEXT_FG[0], TEXT_FG[1], TEXT_FG[2]],
            [TEXT_BG[0], TEXT_BG[1], TEXT_BG[2]],
            (TEXT_ORIGIN.0 as f64, TEXT_ORIGIN.1 as f64),
        )?;
        let mut combined = text.clone();
        combined.extend_from_slice(&reference);
        write_png(&path, TW, TH * 2, &combined)?;
        eprintln!(
            "synoik-vk: wrote {} (top: swash atlas, bottom: pango ref)",
            path.display()
        );
    }
    #[cfg(not(feature = "pango-ref"))]
    {
        write_png(&path, TW, TH, &text)?;
        eprintln!("synoik-vk: wrote {} (swash atlas)", path.display());
    }
    eprintln!("synoik-vk: OK — hinted glyph-atlas text verified");

    if let Some(dmabuf) = render_dmabuf(&gpu)? {
        assert_dmabuf(&dmabuf)?;
        let path = artifact_path("dmabuf.png");
        write_png(&path, DMABUF_W, DMABUF_H, &dmabuf)?;
        eprintln!("synoik-vk: wrote {}", path.display());
        eprintln!("synoik-vk: OK — foreign dmabuf import (GBM, LINEAR modifier) verified");
    }

    probes::report(&gpu);

    // Stage 3 de-risking: does host-GPU completion propagate to a guest sync_file/syncobj? (The one
    // measured risk in docs/fork/explicit-sync.md) Non-fatal if it errors.
    match synoik_vk::sync_spike::run_sync_spike(&gpu) {
        Ok(report) => report.print(),
        Err(e) => eprintln!("synoik-vk: sync spike error: {e:#}"),
    }

    Ok(())
}

// --- scene: solid + SDF rounded-rect + textured quads ------------------------------------------

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
        color: unorm(color),
        ..Default::default()
    };
    let solid_quad = column(16.0, RED, 0.0);
    let rounded_quad = column(144.0, GREEN, 26.0);
    let textured_quad = column(272.0, WHITE, 0.0);

    gpu.run_commands(pool, synoik_vk::stats::SubmitSite::OffscreenFrame, |cbuf| {
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

/// Structural invariants — resolution-independent, so they hold on any conformant driver
/// regardless of AA/rounding rasterization differences.
fn assert_scene(pixels: &[u8]) -> Result<()> {
    check("background is clear", sample(pixels, 8, 8), CLEAR, 2)?;
    check("solid quad is red", sample(pixels, 64, 64), RED, 2)?;
    check(
        "rounded-rect body is green",
        sample(pixels, 192, 64),
        GREEN,
        3,
    )?;
    check(
        "rounded-rect corner is clipped away",
        sample(pixels, 147, 23),
        CLEAR,
        4,
    )?;
    // Textured quad: four quadrants show the four texels.
    check("texture top-left", sample(pixels, 296, 42), TEX_TL, 2)?;
    check("texture top-right", sample(pixels, 344, 42), TEX_TR, 2)?;
    check("texture bottom-left", sample(pixels, 296, 86), TEX_BL, 2)?;
    check("texture bottom-right", sample(pixels, 344, 86), TEX_BR, 2)?;
    Ok(())
}

// --- blur: gaussian of a hard edge --------------------------------------------------------------

const SRC_W: u32 = 192;
const SRC_H: u32 = 128;
const EDGE_RED: [u8; 4] = [230, 40, 40, 255];
const EDGE_BLUE: [u8; 4] = [40, 60, 230, 255];
/// `2σ`, the convention every radius in this crate carries.
const BLUR_RADIUS: f64 = 24.0;

/// Blur a hard vertical red|blue edge; output is level-0 sized (SRC_W x SRC_H).
fn render_blur(gpu: &Gpu) -> Result<Vec<u8>> {
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
    let passes = synoik_vk::blur::downscale_levels(SRC_W, SRC_H, BLUR_RADIUS).max(1);
    let chain = BlurChain::new(gpu, &source, passes)?;

    gpu.run_commands(pool, synoik_vk::stats::SubmitSite::Blur, |cbuf| {
        chain.record_gaussian(gpu, cbuf, BLUR_RADIUS, 1.0)
    })?;
    let (ow, oh) = chain.output_size();
    anyhow::ensure!(
        (ow, oh) == (SRC_W, SRC_H),
        "blur output {ow}x{oh} != source {SRC_W}x{SRC_H}; assert_blur assumes level-0 sizing"
    );
    let out = chain.read_output(gpu, pool)?;

    chain.destroy(gpu);
    source.destroy(gpu);
    unsafe { gpu.device.destroy_command_pool(pool, None) };
    Ok(out)
}

/// Far from the edge the colors survive; at the edge they mix; the hard step is gone.
fn assert_blur(out: &[u8]) -> Result<()> {
    let (ow, oh) = (SRC_W, SRC_H);
    // `Blur::read_output` hands back level 0's raw bytes, and the chain's levels are
    // `render::RENDER_FORMAT` — BGRA order. Reorder to RGBA so the channel assertions below read
    // the way they are written.
    let at = |x: u32, y: u32| {
        let [b, g, r, a] = sample_at(out, ow, x, y);
        [r, g, b, a]
    };
    let cy = oh / 2;
    let left = at(8, cy);
    let right = at(ow - 8, cy);
    let center = at(ow / 2, cy);
    let step_l = at(ow / 2 - 1, cy);
    let step_r = at(ow / 2, cy);
    eprintln!("synoik-vk: blur left={left:?} center={center:?} right={right:?}");

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
    Ok(())
}

// --- text: hinted swash glyph atlas ------------------------------------------------------------

const TEXT: &str = "Activities 12:34";
const TEXT_PX: f32 = 13.0;
const TW: u32 = 200;
const TH: u32 = 32;
const TEXT_BG: [u8; 4] = [24, 24, 28, 255];
const TEXT_FG: [u8; 4] = [235, 235, 235, 255];
const TEXT_ORIGIN: (f32, f32) = (10.0, 8.0);

/// Shape + rasterize hinted text through our swash atlas into a TW x TH Vulkan target.
fn render_text(gpu: &Arc<Gpu>) -> Result<Vec<u8>> {
    let pool_ci = vk::CommandPoolCreateInfo::default().queue_family_index(gpu.queue_family);
    let pool = unsafe { gpu.device.create_command_pool(&pool_ci, None) }.context("text pool")?;

    let (atlas, atlas_side, run) = build_text(gpu, pool, TEXT, TEXT_PX)?;
    anyhow::ensure!(!run.glyphs.is_empty(), "no glyphs were shaped/rasterized");

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
    synoik_vk::stats::descriptor_allocs(1);
    let set = unsafe { gpu.device.allocate_descriptor_sets(&alloc) }.context("set")?[0];
    let img = vk::DescriptorImageInfo::default()
        .sampler(atlas.sampler)
        .image_view(atlas.view)
        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
    let write = vk::WriteDescriptorSet::default()
        .dst_set(set)
        .dst_binding(0)
        .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
        .image_info(std::slice::from_ref(&img));
    synoik_vk::stats::descriptor_writes(1);
    unsafe { gpu.device.update_descriptor_sets(&[write], &[]) };

    let dims = [TW as f32, TH as f32];
    gpu.run_commands(pool, synoik_vk::stats::SubmitSite::OffscreenFrame, |cbuf| {
        target.begin(gpu, cbuf, unorm(TEXT_BG));
        renderer.draw(
            gpu,
            cbuf,
            set,
            &run,
            atlas_side,
            TEXT_ORIGIN,
            dims,
            unorm(TEXT_FG),
        );
        unsafe { gpu.device.cmd_end_render_pass(cbuf) };
    })?;
    let ours = target.read_back(gpu, pool)?;

    unsafe {
        gpu.device.destroy_descriptor_pool(desc_pool, None);
        gpu.device.destroy_descriptor_set_layout(set_layout, None);
        gpu.device.destroy_command_pool(pool, None);
    }
    renderer.destroy(gpu);
    atlas.destroy(gpu);
    target.destroy(gpu);
    Ok(ours)
}

/// The dark bg survives and we rasterized a plausible amount of bright glyph ink.
fn assert_text(ours: &[u8]) -> Result<()> {
    let bright = ours
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| p[0] > 150 && p[1] > 150 && p[2] > 150)
        .count();
    eprintln!("synoik-vk: text bright pixels = {bright}");
    check(
        "text bg corner is dark",
        sample_at(ours, TW, 2, 2),
        TEXT_BG,
        3,
    )?;
    anyhow::ensure!(
        bright > 40,
        "expected visible glyph ink, got {bright} bright pixels"
    );
    Ok(())
}

// --- dmabuf: import a foreign (GBM) buffer and sample it ---------------------------------------

const DMABUF_W: u32 = 64;
const DMABUF_H: u32 = 64;
// Four distinct quadrant colors (RGBA), row-major TL, TR, BL, BR.
const DMA_TL: [u8; 4] = [200, 40, 50, 255];
const DMA_TR: [u8; 4] = [50, 200, 70, 255];
const DMA_BL: [u8; 4] = [60, 90, 220, 255];
const DMA_BR: [u8; 4] = [230, 215, 60, 255];

/// Import a GBM-allocated LINEAR dmabuf and sample it into an RGBA target. Returns `None` when the
/// dmabuf extensions are absent (lavapipe) so the caller can skip cleanly.
fn render_dmabuf(gpu: &Gpu) -> Result<Option<Vec<u8>>> {
    // A CPU device (lavapipe) isn't the GPU backing the DRM render node, so it can't import its
    // dmabufs — skip. (lavapipe still advertises the extensions, so this must gate before them.)
    if gpu.device_type == vk::PhysicalDeviceType::CPU {
        eprintln!(
            "synoik-vk: skipping dmabuf demo — {:?} is a CPU device (no dmabuf import)",
            gpu.device_name
        );
        return Ok(None);
    }
    for ext in [
        "VK_KHR_external_memory_fd",
        "VK_EXT_external_memory_dma_buf",
        "VK_EXT_image_drm_format_modifier",
    ] {
        if !gpu.supports(ext) {
            eprintln!("synoik-vk: skipping dmabuf demo — {ext} not available on this ICD");
            return Ok(None);
        }
    }

    let buf = ForeignBuffer::allocate_filled(DMABUF_W, DMABUF_H, [DMA_TL, DMA_TR, DMA_BL, DMA_BR])?;
    eprintln!(
        "synoik-vk: dmabuf {}x{} modifier {:#x} stride {} offset {}",
        buf.width, buf.height, buf.modifier, buf.stride, buf.offset
    );

    let pool_ci = vk::CommandPoolCreateInfo::default().queue_family_index(gpu.queue_family);
    let pool = unsafe { gpu.device.create_command_pool(&pool_ci, None) }.context("dmabuf pool")?;

    let imported = ImportedImage::import(gpu, pool, &buf)?;

    // Sample the imported image with a full-target textured quad into a same-size RGBA target.
    let target = RenderTarget::new(gpu, DMABUF_W, DMABUF_H)?;
    let set_layout = render::sampler_set_layout(gpu)?;
    let textured = QuadPipeline::new(
        gpu,
        target.render_pass,
        target.extent(),
        QUAD_VERT,
        TEX_FRAG,
        std::slice::from_ref(&set_layout),
    )?;
    let (desc_pool, set) = bind_sampled(gpu, set_layout, imported.view, imported.sampler)?;

    let dims = [DMABUF_W as f32, DMABUF_H as f32];
    let quad = QuadPush {
        origin: [0.0, 0.0],
        size: dims,
        target: dims,
        ..Default::default()
    };
    gpu.run_commands(pool, synoik_vk::stats::SubmitSite::OffscreenFrame, |cbuf| {
        target.begin(gpu, cbuf, unorm(CLEAR));
        textured.draw(gpu, cbuf, &quad, Some(set));
        unsafe { gpu.device.cmd_end_render_pass(cbuf) };
    })?;
    let pixels = target.read_back(gpu, pool)?;

    unsafe {
        gpu.device.destroy_descriptor_pool(desc_pool, None);
        gpu.device.destroy_descriptor_set_layout(set_layout, None);
        gpu.device.destroy_command_pool(pool, None);
    }
    textured.destroy(gpu);
    target.destroy(gpu);
    imported.destroy(gpu);
    drop(buf);
    Ok(Some(pixels))
}

/// The four quadrant centers sampled back must match the four colors written into the dmabuf.
fn assert_dmabuf(pixels: &[u8]) -> Result<()> {
    let tl = sample_at(pixels, DMABUF_W, DMABUF_W / 4, DMABUF_H / 4);
    let tr = sample_at(pixels, DMABUF_W, DMABUF_W * 3 / 4, DMABUF_H / 4);
    let bl = sample_at(pixels, DMABUF_W, DMABUF_W / 4, DMABUF_H * 3 / 4);
    let br = sample_at(pixels, DMABUF_W, DMABUF_W * 3 / 4, DMABUF_H * 3 / 4);
    eprintln!("synoik-vk: dmabuf quadrants TL={tl:?} TR={tr:?} BL={bl:?} BR={br:?}");
    check("dmabuf TL", tl, DMA_TL, 2)?;
    check("dmabuf TR", tr, DMA_TR, 2)?;
    check("dmabuf BL", bl, DMA_BL, 2)?;
    check("dmabuf BR", br, DMA_BR, 2)?;
    Ok(())
}

// --- shared helpers ----------------------------------------------------------------------------

/// Allocate a one-set descriptor pool and point set 0 / binding 0 at `tex`.
fn bind_texture(
    gpu: &Gpu,
    set_layout: vk::DescriptorSetLayout,
    tex: &Texture,
) -> Result<(vk::DescriptorPool, vk::DescriptorSet)> {
    bind_sampled(gpu, set_layout, tex.view, tex.sampler)
}

/// Allocate a one-set descriptor pool and point set 0 / binding 0 at a sampled `view`/`sampler`.
fn bind_sampled(
    gpu: &Gpu,
    set_layout: vk::DescriptorSetLayout,
    view: vk::ImageView,
    sampler: vk::Sampler,
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
    synoik_vk::stats::descriptor_allocs(1);
    let set = unsafe { device.allocate_descriptor_sets(&alloc) }.context("alloc desc set")?[0];

    let img = vk::DescriptorImageInfo::default()
        .sampler(sampler)
        .image_view(view)
        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
    let write = vk::WriteDescriptorSet::default()
        .dst_set(set)
        .dst_binding(0)
        .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
        .image_info(std::slice::from_ref(&img));
    synoik_vk::stats::descriptor_writes(1);
    unsafe { device.update_descriptor_sets(&[write], &[]) };

    Ok((desc_pool, set))
}

fn unorm(c: [u8; 4]) -> [f32; 4] {
    c.map(|v| v as f32 / 255.0)
}

/// Read one RGBA pixel from a `WIDTH`-wide buffer (the scene's stride).
fn sample(pixels: &[u8], x: u32, y: u32) -> [u8; 4] {
    sample_at(pixels, WIDTH, x, y)
}

/// Read one RGBA pixel from a `width`-strided buffer.
fn sample_at(pixels: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * width + x) * 4) as usize;
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

#[cfg(test)]
mod tests {
    use super::*;

    // Exercises the full offscreen path — scene, blur, text — on a single device. The ICD is
    // chosen by the environment: `cargo test -p synoik-vk` hits the real GPU (Venus here); pinning
    // VK_DRIVER_FILES to lavapipe gives a deterministic CPU baseline. Assertions are structural
    // (resolution-independent), so they hold on any conformant driver — no golden images.
    #[test]
    fn renders_all_primitives() -> Result<()> {
        let gpu = Arc::new(Gpu::new()?);
        assert_scene(&render_scene(&gpu)?)?;
        assert_blur(&render_blur(&gpu)?)?;
        assert_text(&render_text(&gpu)?)?;
        // dmabuf import only where the extensions exist (Venus yes, lavapipe no).
        if let Some(dmabuf) = render_dmabuf(&gpu)? {
            assert_dmabuf(&dmabuf)?;
        }
        Ok(())
    }
}
