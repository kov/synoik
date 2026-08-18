// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Frame-cost instrument: prices a composited frame against the things it might scale with, in a
//! headless fixture, so a live-seat stutter can be reproduced and attributed without the seat.
//!
//! Timings, so `#[ignore]`d and printed rather than asserted — this is a measuring device, not a
//! guard. What it *does* print alongside every time is the renderer's own draw and shaded-fragment
//! counters, which are exact and reproducible; assertions belong on those (see
//! `vulkan_render.rs`), not on wall clock.
//!
//! Run: `cargo test --workspace perf_probe -- --nocapture --ignored`
//!
//! # What it found (2026-07-26)
//!
//! The live seat was missing one vblank in ~10 during the overview while every frame reported "0
//! over budget": with `SYNOIK_VK_ASYNC_SCANOUT=1` the frame's GPU work is invisible to the frame
//! log and overruns land on the *flip* instead. Forcing a CPU wait before queueing exposed 15-20 ms
//! of GPU time per overview frame against a 16.67 ms budget.
//!
//! Sweeps 1-4 ruled out every structural explanation. A frame of that shape fits
//! `0.9 ms + 0.112 ms/Mpx`: draw count is nearly free (23 → 34 draws moved nothing), minified
//! sampling gets *cheaper* per fragment as thumbnails shrink, and a LINEAR GBM scanout dmabuf costs
//! the same per pixel as an OPTIMAL offscreen. At 21 Mpx that is 2.5-3 ms — 5-7x under the live
//! figure, so the difference had to be *content*, not shape.
//!
//! Sweeps 5-6 found it. One `background-effect { blur true }` window rule takes the frame from
//! 1.9 ms to 21.4 ms, and the shaded fragments from 2.53x to 9.62x the output. Each blurred window
//! adds ~21 Mpx — about 2.5x the whole screen — **whether it is drawn full-size or as a postage
//! stamp overview thumbnail**: the chain runs at output resolution per blurred window, ignoring the
//! destination geometry. The shader is not slow (0.092 ms/Mpx stands); we ask it for ten screens of
//! fragments.
//!
//! # Sweep 7, and what it settled (2026-07-26, with working GPU timestamps)
//!
//! Sweep 1-4's "draw count is nearly free" rested on 23 → 34 draws, which is inside this probe's
//! noise; a regression over 1123 live frames pointed the other way, at 41 µs/draw. Sweep 7 is the
//! controlled version — the same output tiled by 1 to 256 cells, so **shaded fragments are held
//! fixed while draws move 256x**. Point estimate ~0.6 µs/draw, the same for solid quads, for
//! textured quads sharing one descriptor set, and for a distinct texture per draw. Read that as a
//! **bound, not a value**: the sweep's spread is ~±0.1 ms over a 255-draw lever, so it cannot
//! exclude anything under ~1.5 µs/draw, and the slope is dominated by the 256 point. What it does
//! exclude is 41 µs/draw, by about 30x — that would put the 256-draw row at 10.6 ms.
//!
//! Two things the sweep deliberately does not vary, so do not generalize past them: every variant
//! draws one pipeline (a live frame interleaves solid/texture/glyph/rounded), and its textures are
//! 64x64 and magnified — cache-resident by construction, where a live draw minifies a mip-less
//! full-window source. "A draw is free" means its *fixed* cost is; its marginal sampling cost is
//! exactly what was held out. The live regression's coefficient is also confounded: in that session
//! draws, coverage, damage extent, client-commit rate and load all flip together when the overview
//! opens.
//!
//! Which leaves a content gap, and it is **much smaller than the first writeup of this claimed**.
//! Compare like with like — the probe's minimum against the live *minimum* of a draw bucket, and a
//! probe scene carrying the seat's ingredients rather than `Scene::default()`:
//!
//! | | gpu |
//! |---|---|
//! | probe, bare | 0.70 ms |
//! | probe, + wallpaper | 0.89 ms |
//! | probe, + wallpaper + xray/blur | **1.13 ms** |
//! | live seat, >=150 draws, **minimum** | **2.71 ms** |
//! | live seat, >=150 draws, median | 9.30 ms |
//!
//! So the like-for-like residue is **2.4x**, not the 15x that came from holding a probe minimum
//! against a live median on a bare scene. Candidates for the 1.6 ms: the full-damage present blit
//! into the LINEAR scanout dmabuf (inside the timestamp bracket, `frame.rs:1560`, and **not counted
//! by `shaded`** — blits are not draws), per-commit client-dmabuf acquire barriers (also bracketed,
//! also absent here), and real LINEAR client buffers minified into thumbnails.
//!
//! The bigger question this leaves is the **spread**, not the mean. In the same live bucket, frames
//! with *identical counters* — 134 vs 137 draws, 2.30x vs 2.40x — cost under 4 ms and over 9 ms,
//! interleaved within the same seconds. Nothing the frame log records distinguishes them. Until
//! that is explained, a single live number is not a workload measurement.
//!
//! # Sweep 8: the app-catalog reload (2026-08-12)
//!
//! Asked whether `reload_app_catalog` could stop early-returning on an unchanged enumeration, so
//! that `touch`ing a `.desktop` becomes a "reload now" trigger. Measured on the real 94-app gio
//! catalog with the overview and app grid open, `refresh()` timed separately because both shapes
//! pay it:
//!
//! | | reload_app_catalog | of which sync_app_grid |
//! |---|---|---|
//! | early-return on unchanged (before) | 6.87 ms | — (skipped) |
//! | unconditional (after) | **32.98 ms** | 27.80 ms |
//! | unconditional, favorites hoisted | **8.14 ms** | 0.57 ms |
//!
//! `refresh()` is ~7 ms of that in every row, and the GPU side does not move at all: 0.37 ms warm,
//! 0.30 ms with all 46 icons re-uploaded in a single frame. Icon uploads are not the cost anyone
//! would have guessed they were.
//!
//! The 27.8 ms was **not** the reload's fault. `AppSystem::is_favorite` re-resolves every stored
//! favorite through `DesktopAppInfo::new` on each call, and `sync_app_grid` called it once per
//! installed app — 752 desktop-file parses per sync, paid on every *genuine* catalog change too.
//! Resolving the favorites once ahead of the filter is the whole fix, and it is what makes the
//! unconditional reload cost ~1.2 ms of downstream instead of ~26.
//!
//! Read GPU time, not wall clock, for any of this: [`render_once_gpu`] and [`best_gpu_of`] exist
//! because the wall-clock figures in sweeps 1-6 fold in the host round trip, which on this stack is
//! ~2.6 ms of mesa's userspace fence poll and has nothing to do with the frame.

use std::time::{Duration, Instant};

use smithay::backend::allocator::Fourcc;
use smithay::utils::{Physical, Scale, Size, Transform};
use synoik_config::Action;

use super::fixture::Fixture;
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::render_helpers::{render_to_texture, RenderCtx, RenderTarget, NATIVE_FOURCC};

/// A window big enough that its texture must be minified hard to fit an overview thumbnail.
const WIN: (i32, i32) = (1600, 1000);

/// The gsrs session's wallpaper (`org.gnome.desktop.background picture-uri`).
const WALLPAPER: &str = "/usr/share/backgrounds/f34/default/f34-01-day.png";

/// What live ingredients a probe scene carries beyond bare windows. The live seat has all of them
/// at once; the point is to price them one at a time.
#[derive(Clone, Copy, Default)]
struct Scene {
    /// Decode and upload the real `org.gnome.desktop.background` picture, so the backdrop is a
    /// sampled 4K texture rather than a solid fill.
    wallpaper: bool,
    /// The gsrs XRAY rule: 85% opacity, no opaque border background, `background-effect blur`.
    blur: bool,
    /// …and `background-effect xray`, the see-through sampling path.
    xray: bool,
}

fn build(out: (u16, u16), windows: usize) -> Option<Fixture> {
    build_scene(out, windows, Scene::default())
}

fn build_scene(out: (u16, u16), windows: usize, scene: Scene) -> Option<Fixture> {
    use synoik_config::{BackgroundEffectRule, Config, WindowRule};

    if let Err(e) = VulkanRenderer::new() {
        eprintln!("skipping: no Vulkan device ({e})");
        return None;
    }
    synoik_vk::stats::set_enabled(true);

    let mut config = Config::default();
    if scene.blur || scene.xray {
        config.window_rules.push(WindowRule {
            opacity: Some(0.85),
            draw_border_with_background: Some(false),
            background_effect: BackgroundEffectRule {
                blur: scene.blur.then_some(true),
                xray: scene.xray.then_some(true),
                ..Default::default()
            },
            ..Default::default()
        });
    }

    let mut f = Fixture::with_config(config);
    f.synoik_state()
        .backend
        .headless()
        .add_renderer()
        .expect("build the Vulkan renderer");
    f.add_output(1, out);

    for _ in 0..windows {
        let id = f.add_client();
        let window = f.client(id).create_window();
        let surface = window.surface.clone();
        window.commit();
        f.roundtrip(id);

        // A real shm buffer, not a single-pixel solid: the solid path never samples a texture, and
        // sampling is one of the three things under test.
        let window = f.client(id).window(&surface);
        window.attach_shm_buffer(WIN.0, WIN.1, 200, 100, 50, 255);
        window.set_size(WIN.0 as u16, WIN.1 as u16);
        window.ack_last_and_commit();
        f.double_roundtrip(id);
    }

    if scene.wallpaper {
        // The real picture, decoded synchronously (no worker in the harness) and staged straight
        // into device memory, exactly as the session does it. Slow in a debug build — it happens
        // once per fixture, outside every timed render.
        let settings = crate::gnome::BackgroundSettings {
            picture: Some(std::path::PathBuf::from(WALLPAPER)),
            options: crate::gnome::BackgroundOptions::default(),
        };
        let gpu = f
            .synoik_state()
            .backend
            .with_vulkan_renderer(|r| r.gpu().clone());
        f.synoik().wallpaper.update(&settings, gpu.as_ref());
    }

    f.settle();
    Some(f)
}

/// One full-output render through the owned renderer, no readback. Returns wall time (which
/// includes GPU execution — `finish` fence-waits unless told it may defer), plus the draws and
/// shaded fragments the renderer counted for it.
fn render_once(f: &mut Fixture) -> (Duration, u64, u64) {
    let output = f.synoik_output(1);
    let state = f.synoik_state();
    state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| -> anyhow::Result<(Duration, u64, u64)> {
            let synoik = &mut state.synoik;
            synoik.update_render_elements(Some(&output));

            let size: Size<i32, Physical> = output.current_mode().unwrap().size;
            let scale = Scale::from(output.current_scale().fractional_scale());
            let ctx = RenderCtx {
                renderer: vk,
                target: RenderTarget::Output,
                xray: None,
                appearance: Some(synoik.appearance()),
            };
            let elements = synoik.render_to_vec(ctx, &output, false);

            let (d0, s0) = (synoik_vk::stats::draws(), synoik_vk::stats::shaded());
            let started = Instant::now();
            let (_tex, _sync) = render_to_texture(
                vk,
                size,
                scale,
                Transform::Normal,
                NATIVE_FOURCC,
                elements.iter().rev(),
            )?;
            let elapsed = started.elapsed();

            Ok((
                elapsed,
                synoik_vk::stats::draws() - d0,
                synoik_vk::stats::shaded() - s0,
            ))
        })
        .expect("headless backend must hold a Vulkan renderer")
        .expect("rendering must not error")
}

/// The best of `n` renders — the least-contended sample is the closest thing to a clean read on a
/// VM whose host GPU is never quiet.
fn best_of(f: &mut Fixture, n: usize) -> (Duration, u64, u64) {
    let mut best = None;
    for _ in 0..n {
        let s = render_once(f);
        if best.is_none_or(|(b, _, _): (Duration, _, _)| s.0 < b) {
            best = Some(s);
        }
    }
    best.unwrap()
}

/// As [`render_once`], but into a GBM-allocated LINEAR scanout dmabuf — the live KMS target.
///
/// Hand-rolled rather than routed through `render_to_texture`, so its element loop is not the
/// renderer's: it issues fewer draws for the same scene (5 vs 26). Read the per-pixel rate this
/// produces, not the absolute — it exists to price the *target*, holding everything else roughly
/// fixed, and for that the rate is enough.
fn render_once_dmabuf(
    f: &mut Fixture,
    dmabuf: &mut smithay::backend::allocator::dmabuf::Dmabuf,
) -> (Duration, u64, u64) {
    use smithay::backend::renderer::element::{Element, RenderElement};
    use smithay::backend::renderer::{Bind, Frame, Renderer};

    let output = f.synoik_output(1);
    let state = f.synoik_state();
    state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| -> anyhow::Result<(Duration, u64, u64)> {
            let synoik = &mut state.synoik;
            synoik.update_render_elements(Some(&output));

            let size: Size<i32, Physical> = output.current_mode().unwrap().size;
            let scale = Scale::from(output.current_scale().fractional_scale());
            let ctx = RenderCtx {
                renderer: vk,
                target: RenderTarget::Output,
                xray: None,
                appearance: Some(synoik.appearance()),
            };
            let elements = synoik.render_to_vec(ctx, &output, false);

            let (d0, s0) = (synoik_vk::stats::draws(), synoik_vk::stats::shaded());
            let started = Instant::now();
            {
                let mut fb = vk
                    .bind(dmabuf)
                    .map_err(|e| anyhow::anyhow!("bind dmabuf: {e}"))?;
                let mut frame = vk
                    .render(&mut fb, size, Transform::Normal)
                    .map_err(|e| anyhow::anyhow!("begin frame: {e}"))?;
                for element in elements.iter().rev() {
                    let src = element.src();
                    let dst = element.geometry(scale);
                    element
                        .draw(&mut frame, src, dst, &[dst], &[], None)
                        .map_err(|e| anyhow::anyhow!("draw: {e}"))?;
                }
                // The wait is the point: it is where the GPU time lands.
                frame
                    .finish()
                    .map_err(|e| anyhow::anyhow!("finish: {e}"))?
                    .wait()
                    .map_err(|e| anyhow::anyhow!("wait: {e}"))?;
            }
            let elapsed = started.elapsed();

            Ok((
                elapsed,
                synoik_vk::stats::draws() - d0,
                synoik_vk::stats::shaded() - s0,
            ))
        })
        .expect("headless backend must hold a Vulkan renderer")
        .expect("rendering into the scanout dmabuf must not error")
}

/// Allocate a scanout dmabuf of `fourcc` and take the best of `n` renders into it.
fn best_of_dmabuf(
    f: &mut Fixture,
    out: (u16, u16),
    fourcc: Fourcc,
    n: usize,
) -> Option<(Duration, u64, u64)> {
    use std::fs::File;

    use smithay::backend::allocator::dmabuf::AsDmabuf;
    use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
    use smithay::backend::allocator::{Allocator, Modifier};

    let file = File::options()
        .read(true)
        .write(true)
        .open("/dev/dri/renderD128")
        .ok()?;
    let gbm = GbmDevice::new(file).ok()?;
    let mut alloc = GbmAllocator::new(gbm, GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT);
    let bo = alloc
        .create_buffer(
            u32::from(out.0),
            u32::from(out.1),
            fourcc,
            &[Modifier::Linear],
        )
        .ok()?;
    let mut dmabuf = bo.export().ok()?;

    // Warm: the first bind creates the image, its view and a framebuffer.
    render_once_dmabuf(f, &mut dmabuf);

    let mut best = None;
    for _ in 0..n {
        let s = render_once_dmabuf(f, &mut dmabuf);
        if best.is_none_or(|(b, _, _): (Duration, _, _)| s.0 < b) {
            best = Some(s);
        }
    }
    best
}

fn row(label: &str, out: (u16, u16), (ms, draws, shaded): (Duration, u64, u64)) {
    let ms = ms.as_secs_f64() * 1000.;
    let mpx = shaded as f64 / 1e6;
    let out_px = f64::from(out.0) * f64::from(out.1);
    println!(
        "{label:<28} {ms:7.2}ms  {draws:4} draws  {mpx:6.2} Mpx ({:4.2}x out)  \
         {:6.3} ms/Mpx  {:6.4} ms/draw",
        shaded as f64 / out_px,
        ms / mpx.max(1e-9),
        ms / draws.max(1) as f64,
    );
}

/// The backdrop blur is **shared**: one output-sized effect buffer per frame, sampled by every
/// window that asks for it. So adding blurred windows must not add blurs.
///
/// It did. Each window's `Xray::render` re-prepared the shared buffer, and the reuse check
/// answered "someone else owns this texture" about the renderer's *own* pending queues — so every
/// window threw the offscreen away, allocated a replacement with a fresh blur chain, and queued
/// the blur again. Four blurred windows meant four full-output blur chains in a frame that needed
/// one: 79.83 Mpx instead of 25.40, and 21.4 ms instead of 2.7 (`perf_probe`, sweeps 5-6).
///
/// Asserted on shaded fragments rather than time: the counter is exact and reproducible, and it is
/// the quantity that was wrong. The first assertion is the anti-vacuity guard — a blur that never
/// runs would pass the second one trivially.
#[test]
fn a_blurred_frame_does_not_pay_per_blurred_window() {
    let blur = Scene {
        blur: true,
        ..Default::default()
    };

    let measure = |out, scene, windows| {
        build_scene(out, windows, scene).map(|mut f| {
            best_of(&mut f, 1);
            best_of(&mut f, 2).2
        })
    };

    // What an extra blurred window costs, at two output sizes. A window's own drawing does not
    // care how big the screen is; a re-run of the *shared, output-sized* backdrop blur cares about
    // nothing else. So the tell is not the magnitude, it is whether the per-window cost tracks the
    // output — which needs no threshold pulled out of the air.
    let per_window = |out| {
        let one = measure(out, blur, 1)?;
        let four = measure(out, blur, 4)?;
        let plain = measure(out, Scene::default(), 4)?;
        assert!(
            four > plain,
            "the blur shaded nothing at {out:?} ({four} vs {plain} fragments): this test is not \
             exercising it, so its real assertion is vacuous"
        );
        Some((four.saturating_sub(one)) as f64 / 3.)
    };

    let (Some(small), Some(large)) = (per_window((1920u16, 1080u16)), per_window((3840, 2160)))
    else {
        return; // no Vulkan device
    };

    // Quadrupling the output must not multiply what a blurred window costs. Before the fix each
    // window shaded ~2.57x the output, so this ratio was 4.0; with the backdrop blur shared as
    // intended it is ~1.0. Anything under 2 is unambiguously the shared side of that gap.
    let ratio = large / small.max(1.);
    assert!(
        ratio < 2.0,
        "an extra blurred window cost {small:.0} fragments at 1080p and {large:.0} at 4K \
         ({ratio:.1}x): the per-window cost is tracking the output, so the shared backdrop blur \
         is being re-run per window"
    );
}

#[test]
#[ignore = "timings, not a guard; run explicitly"]
fn perf_probe_what_does_the_overview_frame_scale_with() {
    // --- Sweep 1: overview progress. Scene, resolution and draw count are ~fixed; what changes is
    // how far the window textures are minified into their thumbnails.
    println!("\n== overview progress sweep, 3840x2160, 4 windows ==");
    let out = (3840u16, 2160u16);
    if let Some(mut f) = build(out, 4) {
        best_of(&mut f, 2); // warm pipelines/descriptors
        f.synoik_state().do_action(Action::OpenOverview, false);
        let samples = f.sample_animation(Duration::from_millis(400), 8, |f| best_of(f, 3));
        for (i, s) in samples.iter().enumerate() {
            row(&format!("  progress {:.2}", i as f64 / 8.), out, *s);
        }
    }

    // --- Sweep 2: resolution, overview settled open. Fragments scale with area, draws do not.
    println!("\n== resolution sweep, 4 windows, overview open ==");
    for out in [(3840u16, 2160u16), (1920, 1080), (960, 540)] {
        if let Some(mut f) = build(out, 4) {
            f.synoik_state().do_action(Action::OpenOverview, false);
            f.settle();
            best_of(&mut f, 2);
            let s = best_of(&mut f, 5);
            row(&format!("  {}x{}", out.0, out.1), out, s);
        }
    }

    // --- Sweep 3: window count at one resolution. Draws scale, area does not.
    println!("\n== window-count sweep, 3840x2160, overview open ==");
    let out = (3840u16, 2160u16);
    for n in [1usize, 4, 8, 12] {
        if let Some(mut f) = build(out, n) {
            f.synoik_state().do_action(Action::OpenOverview, false);
            f.settle();
            best_of(&mut f, 2);
            let s = best_of(&mut f, 5);
            row(&format!("  {n} windows"), out, s);
        }
    }

    // --- Sweep 5: the content ladder. Sweeps 1-3 priced the *shape* of the frame and came to
    // 0.11 ms/Mpx against the live seat's 0.5-0.65; the difference has to be in what the live
    // scene contains that a bare fixture does not. One ingredient per row.
    println!("\n== content ladder, 3840x2160, 4 windows, overview open ==");
    let out = (3840u16, 2160u16);
    for (label, scene) in [
        ("  bare", Scene::default()),
        (
            "  + wallpaper",
            Scene {
                wallpaper: true,
                ..Default::default()
            },
        ),
        (
            "  + blur (opacity .85)",
            Scene {
                wallpaper: true,
                blur: true,
                ..Default::default()
            },
        ),
        (
            "  + xray",
            Scene {
                wallpaper: true,
                blur: true,
                xray: true,
            },
        ),
    ] {
        if let Some(mut f) = build_scene(out, 4, scene) {
            f.synoik_state().do_action(Action::OpenOverview, false);
            f.settle();
            best_of(&mut f, 2);
            let s = best_of(&mut f, 5);
            row(label, out, s);
        }
    }

    // --- Sweep 6: is the blur priced by the window's on-screen size, or by the output?
    // In the overview a thumbnail covers a fraction of the screen, so a blur scissored to its
    // destination should get *cheaper* as the overview opens. If it does not, the chain is running
    // at full output size per blurred window regardless of how small the window is drawn.
    println!("\n== blur vs window size, 3840x2160 ==");
    let blur = Scene {
        wallpaper: true,
        blur: true,
        ..Default::default()
    };
    for (label, open) in [
        ("  closed (full-size)", false),
        ("  overview (thumbnails)", true),
    ] {
        for n in [1usize, 4] {
            if let Some(mut f) = build_scene(out, n, blur) {
                if open {
                    f.synoik_state().do_action(Action::OpenOverview, false);
                    f.settle();
                }
                best_of(&mut f, 2);
                let s = best_of(&mut f, 5);
                row(&format!("{label}, {n} win"), out, s);
            }
        }
    }

    // --- Sweep 4: the render *target*. The probe renders into an offscreen VkImage (OPTIMAL
    // tiling, device-local); the live seat renders for KMS, into a GBM scanout dmabuf. On
    // virtio-gpu that is a LINEAR blob, and a linear 4K color attachment is the one thing in the
    // live path this probe was not paying for.
    println!("\n== target sweep, 3840x2160, 4 windows, overview open ==");
    let out = (3840u16, 2160u16);
    for fourcc in [Fourcc::Abgr8888, Fourcc::Argb8888] {
        if let Some(mut f) = build(out, 4) {
            f.synoik_state().do_action(Action::OpenOverview, false);
            f.settle();
            best_of(&mut f, 2);
            match best_of_dmabuf(&mut f, out, fourcc, 5) {
                Some(s) => row(&format!("  dmabuf {fourcc:?}"), out, s),
                None => println!("  dmabuf {fourcc:?}: skipped"),
            }
        }
    }

    // --- Control: the same outputs with the overview *closed*, i.e. one full-screen window and no
    // thumbnails. Separates "the overview is expensive" from "4K compositing is expensive".
    println!("\n== control: overview closed ==");
    for out in [(3840u16, 2160u16), (1920, 1080)] {
        if let Some(mut f) = build(out, 4) {
            best_of(&mut f, 2);
            let s = best_of(&mut f, 5);
            row(&format!("  {}x{} closed", out.0, out.1), out, s);
        }
    }
    println!();
}

/// Sweep 7: **what does a draw call cost?** — the one thing sweeps 1-4 measured badly, and the one
/// thing the live regression measured misleadingly.
///
/// This file's header says "draw count is nearly free (23 → 34 draws moved nothing)". Eleven draws
/// is the whole evidence. Against that, regressing 1123 single-submit live frames on draws and
/// coverage gives `0.05 ms + 41 µs/draw + 0.125 ms/Mpx` (R² 0.766, against 0.687 for draws alone
/// and **0.261 for coverage alone**) — which would put 8.4 of an overview frame's 10.9 ms into draw
/// calls. Both cannot be right, and neither is a controlled measurement: the live one cannot hold
/// anything fixed, and 11 draws is inside this probe's noise.
///
/// So: tile the same output with an N-cell grid, 1 to 256 cells, and **hold the shaded-fragment
/// count fixed** while the draw count moves 256x. Three variants, because "a draw" is not one
/// thing:
///
/// - **solid** — one pipeline, no descriptor, no sampling. The floor.
/// - **one texture** — the texture pipeline, but every cell samples the *same* image, so the
///   descriptor set is bound once and the draws differ only in geometry.
/// - **N textures** — a distinct image per cell, i.e. a descriptor-set bind per draw. This is the
///   shape of an overview frame, where every thumbnail is its own window's texture.
///
/// The spread between the last two is the cost of *rebinding*, which on a paravirtualized driver is
/// ring traffic rather than GPU work. Measured on GPU timestamps, so the host round trip and the
/// CPU-side element loop are excluded by construction.
#[test]
#[ignore]
fn perf_probe_what_does_a_draw_call_cost() {
    use smithay::backend::renderer::element::Kind;
    use smithay::utils::Point;

    use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};

    // The internal panel's shape, i.e. the resolution the live regression was taken at.
    const OUT: (u16, u16) = (2048, 1330);
    /// Square grids, so no cell is a sliver: sweeping strip *shapes* over 256x would confound
    /// per-draw cost with whatever the rasterizer does to thin quads.
    const SIDES: [u32; 7] = [1, 2, 4, 6, 8, 12, 16];
    const REPEATS: usize = 9;

    let Some(mut f) = build(OUT, 0) else { return };
    let timed = f
        .synoik_state()
        .backend
        .headless()
        .with_vulkan_renderer(VulkanRenderer::enable_gpu_timing)
        .unwrap_or(false);
    if !timed {
        println!("skipping: the device declines to timestamp, so only wall clock is available");
        return;
    }

    println!(
        "\n== draw-call sweep, {}x{}, constant coverage ==",
        OUT.0, OUT.1
    );
    let mut fits: Vec<(&str, f64, f64)> = Vec::new();
    for variant in ["solid", "one texture", "N textures"] {
        println!("  -- {variant} --");
        let mut points: Vec<(f64, f64)> = Vec::new();
        for side in SIDES {
            let cells = side * side;
            let cell = (
                f64::from(OUT.0) / f64::from(side),
                f64::from(OUT.1) / f64::from(side),
            );
            let at = |i: u32| {
                Point::<f64, _>::from((f64::from(i % side) * cell.0, f64::from(i / side) * cell.1))
            };

            let sample = if variant == "solid" {
                // Built once and held: `from_buffer` borrows its buffer.
                let buffers: Vec<SolidColorBuffer> = (0..cells)
                    .map(|i| {
                        let shade = 0.2 + 0.15 * f32::from(u8::try_from(i % 5).expect("small"));
                        SolidColorBuffer::new(Size::<f64, _>::from(cell), [shade, 0.4, 0.7, 1.])
                    })
                    .collect();
                let elements: Vec<SolidColorRenderElement> = buffers
                    .iter()
                    .enumerate()
                    .map(|(i, buffer)| {
                        let i = u32::try_from(i).expect("small");
                        SolidColorRenderElement::from_buffer(buffer, at(i), 1., Kind::Unspecified)
                    })
                    .collect();
                best_gpu_of(&mut f, &elements, REPEATS)
            } else {
                let distinct = if variant == "N textures" { cells } else { 1 };
                let elements = textured_grid(&mut f, distinct, cells, cell, at);
                best_gpu_of(&mut f, &elements, REPEATS)
            };

            let Some((gpu, draws, shaded)) = sample else {
                println!("     {cells:4} draws: every pair came back unusable");
                continue;
            };
            let ms = gpu.as_secs_f64() * 1000.;
            println!(
                "     {draws:4} draws  gpu {ms:6.3}ms  {:5.2} Mpx ({:4.2}x out)  {:7.1} µs/draw",
                shaded as f64 / 1e6,
                shaded as f64 / (f64::from(OUT.0) * f64::from(OUT.1)),
                ms * 1000. / draws.max(1) as f64,
            );
            points.push((draws as f64, ms));
        }
        if let Some(fit) = line_fit(&points) {
            fits.push((variant, fit.0, fit.1));
        }
    }

    println!("\n  fits (gpu = intercept + slope × draws, coverage held at 1.00x out):");
    for (variant, intercept, slope) in fits {
        println!(
            "    {variant:<12} {intercept:6.3} ms + {:6.2} µs/draw",
            slope * 1000.
        );
    }
    println!("    live regression over 6-204 draws said 41.4 µs/draw");

    // The comparison that matters: a settled overview frame priced in GPU time, **with the live
    // seat's scene ingredients added one at a time**. The first version of this ran
    // `Scene::default()` — no wallpaper, no blur, no xray — against a seat that has all three, and
    // then attributed the whole difference to client-dmabuf sampling. Half the "content" hypothesis
    // was testable with flags already in this file.
    //
    // Compare **minima with minima**: the live figure to hold this against is the *minimum* of a
    // draw bucket, not its median. A loaded session's median carries contention with the previous
    // frame's tail and with the host compositing the guest window, which an idle probe cannot
    // reproduce; on the 2026-07-26 seat capture that spread alone is 2.71 ms vs 9.30 ms inside one
    // bucket.
    println!("\n  == settled overview, GPU time, live ingredients added one at a time ==");
    println!("     (live seat, >=150 draws: min 2.71ms  median 9.30ms  max 13.13ms, ~2.2x out)");
    let scenes = [
        ("bare", Scene::default()),
        (
            "+ wallpaper",
            Scene {
                wallpaper: true,
                ..Default::default()
            },
        ),
        (
            "+ wallpaper + xray/blur",
            Scene {
                wallpaper: true,
                blur: true,
                xray: true,
            },
        ),
    ];
    for (label, scene) in scenes {
        let Some(mut f) = build_scene(OUT, 4, scene) else {
            return;
        };
        let timed = f
            .synoik_state()
            .backend
            .headless()
            .with_vulkan_renderer(VulkanRenderer::enable_gpu_timing)
            .unwrap_or(false);
        if !timed {
            break;
        }
        f.synoik_state().do_action(Action::OpenOverview, false);
        f.settle();
        let mut samples: Vec<(Duration, u64, u64)> = Vec::new();
        for _ in 0..REPEATS {
            if let Some(s) = render_once_gpu(&mut f) {
                samples.push(s);
            }
        }
        if samples.is_empty() {
            println!("     {label:<24}: every pair came back unusable");
            continue;
        }
        samples.sort_by_key(|(gpu, _, _)| *gpu);
        let (draws, shaded) = (samples[0].1, samples[0].2);
        println!(
            "     {label:<24} gpu min {:6.3}ms  median {:6.3}ms  {draws:4} draws  {:4.2}x out",
            samples[0].0.as_secs_f64() * 1000.,
            samples[samples.len() / 2].0.as_secs_f64() * 1000.,
            shaded as f64 / (f64::from(OUT.0) * f64::from(OUT.1)),
        );
    }
    println!();
}

/// [`render_once`], measured on GPU timestamps instead of wall clock. `None` if the pair came back
/// unusable. Requires `enable_gpu_timing` on the fixture's renderer.
fn render_once_gpu(f: &mut Fixture) -> Option<(Duration, u64, u64)> {
    let output = f.synoik_output(1);
    let state = f.synoik_state();
    state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| -> anyhow::Result<Option<(Duration, u64, u64)>> {
            let synoik = &mut state.synoik;
            synoik.update_render_elements(Some(&output));

            let size: Size<i32, Physical> = output.current_mode().unwrap().size;
            let scale = Scale::from(output.current_scale().fractional_scale());
            let ctx = RenderCtx {
                renderer: vk,
                target: RenderTarget::Output,
                xray: None,
                appearance: Some(synoik.appearance()),
            };
            let elements = synoik.render_to_vec(ctx, &output, false);

            let _ = crate::frame_log::take_gpu_samples();
            let (d0, s0) = (synoik_vk::stats::draws(), synoik_vk::stats::shaded());
            let (_tex, _sync) = render_to_texture(
                vk,
                size,
                scale,
                Transform::Normal,
                NATIVE_FOURCC,
                elements.iter().rev(),
            )?;
            let samples = crate::frame_log::take_gpu_samples();
            // Exactly one, for the reason in `best_gpu_of`.
            Ok((samples.lost == 0 && samples.count == 1).then_some((
                samples.time,
                synoik_vk::stats::draws() - d0,
                synoik_vk::stats::shaded() - s0,
            )))
        })
        .expect("headless backend must hold a Vulkan renderer")
        .expect("rendering must not error")
}

/// `cells` textured quads tiling the output, sampling `distinct` separate images (`1` to share one
/// descriptor set across every draw, `cells` to rebind per draw).
fn textured_grid(
    f: &mut Fixture,
    distinct: u32,
    cells: u32,
    cell: (f64, f64),
    at: impl Fn(u32) -> smithay::utils::Point<f64, smithay::utils::Logical>,
) -> Vec<crate::render_helpers::texture::TextureRenderElement<VkTexture>> {
    use smithay::backend::renderer::element::Kind;

    use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};

    /// Small enough that N of them cost nothing to upload, big enough to be a real sampled image
    /// rather than a degenerate 1x1.
    const TEX: i32 = 64;

    let buffers: Vec<TextureBuffer<_>> = f
        .synoik_state()
        .backend
        .headless()
        .with_vulkan_renderer(|vk| {
            (0..distinct)
                .map(|i| {
                    let tint = u8::try_from(40 + i % 200).expect("small");
                    let texels: Vec<u8> = (0..TEX * TEX)
                        .flat_map(|p| [tint, u8::try_from(p % 256).expect("mod"), 90, 255])
                        .collect();
                    TextureBuffer::from_memory(
                        vk,
                        &texels,
                        Fourcc::Abgr8888,
                        (TEX, TEX),
                        false,
                        1.,
                        Transform::Normal,
                        Vec::new(),
                    )
                    .expect("import texture")
                })
                .collect()
        })
        .expect("headless backend must hold a Vulkan renderer");

    (0..cells)
        .map(|i| {
            let buffer = buffers[(i % distinct) as usize].clone();
            let mut element = TextureRenderElement::from_texture_buffer(
                buffer,
                at(i),
                1.,
                None,
                None,
                Kind::Unspecified,
            );
            // Stretch the 64x64 source over the whole cell, so coverage stays at one output
            // whatever the grid is — the control this whole sweep rests on.
            element.set_size(Size::<f64, _>::from(cell));
            element
        })
        .collect()
}

/// Least-squares `(intercept, slope)` through `points`, or `None` with fewer than two.
fn line_fit(points: &[(f64, f64)]) -> Option<(f64, f64)> {
    if points.len() < 2 {
        return None;
    }
    let n = points.len() as f64;
    let (sx, sy): (f64, f64) = points.iter().fold((0., 0.), |a, p| (a.0 + p.0, a.1 + p.1));
    let (mx, my) = (sx / n, sy / n);
    let num: f64 = points.iter().map(|(x, y)| (x - mx) * (y - my)).sum();
    let den: f64 = points.iter().map(|(x, _)| (x - mx).powi(2)).sum();
    let slope = num / den;
    Some((my - slope * mx, slope))
}

/// Render `elements` `n` times and return the cheapest **GPU** sample, with the draw and fragment
/// counts of that render.
///
/// Cheapest rather than mean for the same reason as [`best_of`], and GPU rather than wall clock
/// because the question is what the device did, not what the host round trip cost. Samples are read
/// through the frame log's per-thread store; the fixture renders on this thread, so nothing else
/// can contribute one.
fn best_gpu_of<E>(f: &mut Fixture, elements: &[E], n: usize) -> Option<(Duration, u64, u64)>
where
    E: smithay::backend::renderer::element::RenderElement<VulkanRenderer>,
{
    let output = f.synoik_output(1);
    let size: Size<i32, Physical> = output.current_mode().unwrap().size;
    let scale = Scale::from(output.current_scale().fractional_scale());

    let mut best: Option<(Duration, u64, u64)> = None;
    for _ in 0..n {
        let sample = f
            .synoik_state()
            .backend
            .headless()
            .with_vulkan_renderer(|vk| -> anyhow::Result<Option<(Duration, u64, u64)>> {
                let _ = crate::frame_log::take_gpu_samples();
                let (d0, s0) = (synoik_vk::stats::draws(), synoik_vk::stats::shaded());
                let (_tex, _sync) = render_to_texture(
                    vk,
                    size,
                    scale,
                    Transform::Normal,
                    NATIVE_FOURCC,
                    elements.iter(),
                )?;
                let samples = crate::frame_log::take_gpu_samples();
                // Exactly one: with `SYNOIK_VK_ASYNC_SCANOUT` set in the environment an offscreen
                // finish defers, its pair resolves during the *next* iteration's retire, and one
                // window would then hold two renders' summed time.
                Ok((samples.lost == 0 && samples.count == 1).then_some((
                    samples.time,
                    synoik_vk::stats::draws() - d0,
                    synoik_vk::stats::shaded() - s0,
                )))
            })
            .expect("headless backend must hold a Vulkan renderer")
            .expect("rendering must not error");
        if let Some(sample) = sample {
            if best.is_none_or(|(b, _, _)| sample.0 < b) {
                best = Some(sample);
            }
        }
    }
    best
}

/// # Sweep 8: what does an app-catalog reload cost?
///
/// `reload_app_catalog` currently early-returns when the enumeration is byte-identical to the one
/// it replaces, so a `touch` on a `.desktop` is not a usable "reload now" trigger. This prices the
/// downstream that check skips, on the real installed catalog, with the overview open so the dash
/// and the app grid — the only surfaces that pay — are on screen.
///
/// `refresh()` itself runs in **both** arms, so it is reported separately: the A/B delta is only
/// the work below the check.
#[test]
#[ignore]
fn perf_probe_what_does_an_app_catalog_reload_cost() {
    const OUT: (u16, u16) = (2048, 1330);
    const CALLS: usize = 20;
    const REPEATS: usize = 9;

    let Some(mut f) = build(OUT, 2) else { return };
    let timed = f
        .synoik_state()
        .backend
        .headless()
        .with_vulkan_renderer(VulkanRenderer::enable_gpu_timing)
        .unwrap_or(false);

    // The real installed catalog: fake entries carry no icon names, so every decode and every
    // upload this is trying to price would be missing. The channel end keeps the watcher's sender
    // alive for the length of the test; nothing reads it here.
    let (app_system, _app_db_rx) = crate::app_system::AppSystem::new_gio();
    f.synoik().app_system = app_system;
    let ids: Vec<String> = f
        .synoik()
        .app_system
        .installed()
        .filter(|e| e.should_show)
        .map(|e| e.id.clone())
        .collect();
    let installed = ids.len();
    f.synoik()
        .app_system
        .set_favorites(ids.iter().take(8).cloned().collect());

    f.synoik().sync_dash_favorites();
    f.synoik().sync_app_grid();
    f.synoik_state().do_action(Action::OpenOverview, false);
    // With only the dash on screen the sweep prices 8 favourites; the app grid is where the
    // catalog's icons actually are, and it is the surface a reload has the most to redraw.
    f.synoik().layout.open_app_grid();
    f.settle();
    // Warm up with no worker wired, so `buffer()` decodes inline and the icons are actually on the
    // GPU before anything is measured. Wiring the worker first would mean every frame here merely
    // *enqueued* a decode nobody answers, and the sweep would price an empty dash.
    let _ = best_of(&mut f, 3);
    let warm_uploads = f.synoik().dash.icon_upload_count();
    assert!(
        warm_uploads > 0,
        "no app icons reached the GPU, so this sweep would price an empty dash"
    );

    // Now wire the decode worker's channel, which is the production shape: the main thread pays the
    // request, the worker pays the decode. The buffers warmed above stay cached until an
    // invalidation demotes them, so this changes who does the work, not whether it is needed.
    let icon_rx = f.synoik().app_icon_cache.wire_test_channel();

    // `refresh()` alone — the enumeration both arms pay before the check is even reached.
    let mut refreshes: Vec<Duration> = Vec::new();
    for _ in 0..CALLS {
        let started = Instant::now();
        let _ = f.synoik().app_system.refresh();
        refreshes.push(started.elapsed());
    }

    // The call under test. Which arm this is depends on the compiled source: with the check in
    // place every one of these early-returns, with it removed every one runs the downstream.
    let mut reloads: Vec<Duration> = Vec::new();
    for _ in 0..CALLS {
        let started = Instant::now();
        f.synoik().reload_app_catalog();
        reloads.push(started.elapsed());
    }
    let queued = icon_rx.try_iter().count();
    let ms = |d: Duration| d.as_secs_f64() * 1000.;

    // Attribute the downstream, so a verdict can name what is expensive rather than the call that
    // contains it. Each step is timed on its own, in `reload_app_catalog`'s order.
    let mut parts: Vec<(&str, Duration)> = Vec::new();
    for _ in 0..CALLS {
        let mut at = Instant::now();
        let mut lap = |label: &'static str, parts: &mut Vec<(&'static str, Duration)>| {
            let d = at.elapsed();
            at = Instant::now();
            match parts.iter_mut().find(|(l, _)| *l == label) {
                Some((_, acc)) => *acc += d,
                None => parts.push((label, d)),
            }
        };
        f.synoik().app_icon_cache.clear();
        lap("app_icon_cache.clear", &mut parts);
        f.synoik().sync_overview_search();
        lap("sync_overview_search", &mut parts);
        f.synoik().sync_dash_favorites();
        lap("sync_dash_favorites", &mut parts);
        f.synoik().sync_app_grid();
        lap("sync_app_grid", &mut parts);
        f.synoik().prewarm_app_icons();
        lap("prewarm_app_icons", &mut parts);
        f.synoik().queue_redraw_all();
        lap("queue_redraw_all", &mut parts);
        let _ = icon_rx.try_iter().count();
    }
    for (label, total) in &parts {
        println!(
            "       {label:<22} {:6.3}ms/call",
            ms(*total) / CALLS as f64
        );
    }

    let stat = |v: &mut Vec<Duration>| {
        v.sort();
        (ms(v[0]), ms(v[v.len() / 2]), ms(v[v.len() - 1]))
    };
    let (r_min, r_med, r_max) = stat(&mut refreshes);
    let (l_min, l_med, l_max) = stat(&mut reloads);

    println!();
    println!("  sweep 8: app-catalog reload, {installed} installed apps, overview open");
    println!("     refresh() alone         min {r_min:6.3}ms  median {r_med:6.3}ms  max {r_max:6.3}ms  (both arms)");
    println!("     reload_app_catalog()    min {l_min:6.3}ms  median {l_med:6.3}ms  max {l_max:6.3}ms  (x{CALLS})");
    println!(
        "     icon decodes queued     {queued} over {CALLS} calls, {warm_uploads} icons uploaded"
    );

    if !timed {
        println!("     (no GPU timestamps: frame costs skipped)");
        println!();
        return;
    }

    let mut steady: Vec<Duration> = Vec::new();
    for _ in 0..REPEATS {
        if let Some((gpu, _, _)) = render_once_gpu(&mut f) {
            steady.push(gpu);
        }
    }
    // The worst case for the frame: every app icon re-uploaded at once. In production the drops are
    // per-icon as each replacement decode lands, so the real cost is this spread over many frames —
    // read it as a bound, not a value.
    f.synoik().dash.clear_icon_uploads();
    let cold = render_once_gpu(&mut f);
    let after = f.synoik().dash.icon_upload_count();

    steady.sort();
    if steady.is_empty() {
        println!("     every steady-state pair came back unusable");
    } else {
        println!(
            "     frame gpu, warm         min {:6.3}ms  median {:6.3}ms",
            ms(steady[0]),
            ms(steady[steady.len() / 2]),
        );
    }
    match cold {
        Some((gpu, _, _)) => println!(
            "     frame gpu, all {after:3} icons re-uploaded  {:6.3}ms  (worst case, one frame)",
            ms(gpu),
        ),
        None => println!("     the re-upload frame's pair came back unusable"),
    }
    println!();
}
