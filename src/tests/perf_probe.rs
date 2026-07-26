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
//! over budget": with `NIRI_VK_ASYNC_SCANOUT=1` the frame's GPU work is invisible to the frame log
//! and overruns land on the *flip* instead. Forcing a CPU wait before queueing exposed 15-20 ms of
//! GPU time per overview frame against a 16.67 ms budget.
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

use std::time::{Duration, Instant};

use niri_config::Action;
use smithay::backend::allocator::Fourcc;
use smithay::utils::{Physical, Scale, Size, Transform};

use super::fixture::Fixture;
use crate::render_helpers::vulkan::VulkanRenderer;
use crate::render_helpers::{render_to_texture, RenderCtx, RenderTarget};

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
    use niri_config::{BackgroundEffectRule, Config, WindowRule};

    if let Err(e) = VulkanRenderer::new() {
        eprintln!("skipping: no Vulkan device ({e})");
        return None;
    }
    niri_vk::stats::set_enabled(true);

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
    f.niri_state()
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
            .niri_state()
            .backend
            .with_vulkan_renderer(|r| r.gpu().clone());
        f.niri().wallpaper.update(&settings, gpu.as_ref());
    }

    f.niri_complete_animations();
    Some(f)
}

/// One full-output render through the owned renderer, no readback. Returns wall time (which
/// includes GPU execution — `finish` fence-waits unless told it may defer), plus the draws and
/// shaded fragments the renderer counted for it.
fn render_once(f: &mut Fixture) -> (Duration, u64, u64) {
    let output = f.niri_output(1);
    let state = f.niri_state();
    state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| -> anyhow::Result<(Duration, u64, u64)> {
            let niri = &mut state.niri;
            niri.update_render_elements(Some(&output));

            let size: Size<i32, Physical> = output.current_mode().unwrap().size;
            let scale = Scale::from(output.current_scale().fractional_scale());
            let ctx = RenderCtx {
                renderer: vk,
                target: RenderTarget::Output,
                xray: None,
            };
            let elements = niri.render_to_vec(ctx, &output, false);

            let (d0, s0) = (niri_vk::stats::draws(), niri_vk::stats::shaded());
            let started = Instant::now();
            let (_tex, _sync) = render_to_texture(
                vk,
                size,
                scale,
                Transform::Normal,
                Fourcc::Abgr8888,
                elements.iter().rev(),
            )?;
            let elapsed = started.elapsed();

            Ok((
                elapsed,
                niri_vk::stats::draws() - d0,
                niri_vk::stats::shaded() - s0,
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

    let output = f.niri_output(1);
    let state = f.niri_state();
    state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| -> anyhow::Result<(Duration, u64, u64)> {
            let niri = &mut state.niri;
            niri.update_render_elements(Some(&output));

            let size: Size<i32, Physical> = output.current_mode().unwrap().size;
            let scale = Scale::from(output.current_scale().fractional_scale());
            let ctx = RenderCtx {
                renderer: vk,
                target: RenderTarget::Output,
                xray: None,
            };
            let elements = niri.render_to_vec(ctx, &output, false);

            let (d0, s0) = (niri_vk::stats::draws(), niri_vk::stats::shaded());
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
                niri_vk::stats::draws() - d0,
                niri_vk::stats::shaded() - s0,
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
        f.niri_state().do_action(Action::OpenOverview, false);
        let samples = f.sample_animation(Duration::from_millis(400), 8, |f| best_of(f, 3));
        for (i, s) in samples.iter().enumerate() {
            row(&format!("  progress {:.2}", i as f64 / 8.), out, *s);
        }
    }

    // --- Sweep 2: resolution, overview settled open. Fragments scale with area, draws do not.
    println!("\n== resolution sweep, 4 windows, overview open ==");
    for out in [(3840u16, 2160u16), (1920, 1080), (960, 540)] {
        if let Some(mut f) = build(out, 4) {
            f.niri_state().do_action(Action::OpenOverview, false);
            f.settle_animations();
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
            f.niri_state().do_action(Action::OpenOverview, false);
            f.settle_animations();
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
            f.niri_state().do_action(Action::OpenOverview, false);
            f.settle_animations();
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
                    f.niri_state().do_action(Action::OpenOverview, false);
                    f.settle_animations();
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
            f.niri_state().do_action(Action::OpenOverview, false);
            f.settle_animations();
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
