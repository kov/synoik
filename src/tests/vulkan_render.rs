//! End-to-end proof that the live `Niri::render` compositing path runs on the **owned Vulkan
//! renderer**, not just GLES: a real client window is mapped through the headless test harness and
//! the whole scene is composited through `VulkanRenderer`, both into an offscreen buffer (the
//! screenshot path) and into a **GBM-allocated scanout dmabuf** (the KMS-present path — everything
//! except the DRM page-flip, which is validated live). Exercises the renderer-agnostic render
//! helpers (Brick 2), the `try_as_gles` degradation guards (Brick 3), and `Bind<Dmabuf>` (Brick A).
//!
//! Skips gracefully when no Vulkan device is present. The scanout test additionally needs a real
//! GBM device (the render node), so it is Venus-only (lavapipe/CPU has no GBM).

use niri_config::{Action, Config};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::{Element, RenderElement};
use smithay::backend::renderer::{Bind, Color32F, ExportMem, Frame, Renderer};
use smithay::output::Output;
use smithay::utils::{Buffer as BufferCoord, Physical, Rectangle, Scale, Size, Transform};

use super::fixture::Fixture;
use crate::backend::RendererKind;
use crate::niri::OutputRenderElements;
use crate::render_helpers::vulkan::VulkanRenderer;
use crate::render_helpers::{render_to_vec, RenderCtx, RenderTarget};

const OUT_W: u16 = 1280;
const OUT_H: u16 = 720;
const WIN: u16 = 200;
// A saturated, opaque green window — distinct from any background/backdrop color, so its presence
// in the composited readback is unambiguous. Single-pixel-buffer channels are premultiplied and
// scaled so u32::MAX == 1.0.
const GREEN: [u32; 4] = [0, u32::MAX, 0, u32::MAX];
// A saturated, opaque red window. Unlike green (R=B=0), red has R≠B, so a readback distinguishes
// RGBA byte order from BGRA — the present-blit scanout test uses it to prove the blit reorders.
const RED: [u32; 4] = [u32::MAX, 0, 0, u32::MAX];

/// The tight `Abgr8888` pixel at (x, y) in a `w`-wide readback buffer.
fn px(pixels: &[u8], w: i32, x: i32, y: i32) -> [u8; 4] {
    let i = ((y * w + x) * 4) as usize;
    [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
}

/// Build a Vulkan-backed fixture with one 1280×720 output and a single opaque window of the given
/// premultiplied color, mapped and animation-settled (a static scene). Returns `None` (with a skip
/// message) when there is no Vulkan device — smithay renders the single-pixel buffer as a solid
/// color, so the window needs no client-buffer import.
fn window_fixture(color: [u32; 4]) -> Option<Fixture> {
    window_fixture_settled(color, true)
}

/// As [`window_fixture`], but `settle` controls whether the map/open animation is completed. Pass
/// `false` to leave the tile open animation active (the guarded GLES-offscreen render path).
fn window_fixture_settled(color: [u32; 4], settle: bool) -> Option<Fixture> {
    if let Err(e) = VulkanRenderer::new() {
        eprintln!("skipping: no Vulkan device ({e})");
        return None;
    }

    let mut f = Fixture::with_config_and_renderer(Config::default(), RendererKind::Vulkan);
    f.niri_state()
        .backend
        .headless()
        .add_renderer()
        .expect("build the Vulkan renderer");
    f.add_output(1, (OUT_W, OUT_H));

    let id = f.add_client();
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    window.attach_solid_buffer(color[0], color[1], color[2], color[3]);
    window.set_size(WIN, WIN);
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    if settle {
        // Settle any map/open animation so we composite a static scene.
        f.niri_complete_animations();
        f.double_roundtrip(id);
    }

    Some(f)
}

/// The green-window fixture used by the offscreen/direct-scanout tests.
fn green_window_fixture() -> Option<Fixture> {
    window_fixture(GREEN)
}

/// Assert the composited frame shows the window (some opaque-green pixel) *and* composited more
/// than just the window (background/backdrop present). Returns the green-pixel count for logging.
fn assert_window_and_background(pixels: &[u8], w: i32, h: i32) -> usize {
    assert_eq!(
        pixels.len(),
        (w * h * 4) as usize,
        "unexpected readback size"
    );
    let is_green = |p: [u8; 4]| p[0] < 40 && p[1] > 200 && p[2] < 40 && p[3] > 200;
    let green = (0..w * h)
        .filter(|i| is_green(px(pixels, w, i % w, i / w)))
        .count();
    assert!(
        green > 0,
        "the mapped green window is absent from the frame"
    );
    assert!(
        green < (w * h) as usize,
        "the frame is uniformly the window color; nothing else composited"
    );
    green
}

/// Composite the whole `output` through the owned Vulkan renderer (the `Niri::screenshot` path),
/// returning the tight `Abgr8888` readback and its dimensions.
fn render_output_vulkan(f: &mut Fixture, output: &Output) -> (Vec<u8>, i32, i32) {
    let state = f.niri_state();
    state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| -> anyhow::Result<(Vec<u8>, i32, i32)> {
            let niri = &mut state.niri;
            niri.update_render_elements(Some(output));

            let size: Size<i32, Physical> = output.current_mode().unwrap().size;
            let size = output.current_transform().transform_size(size);
            let scale = Scale::from(output.current_scale().fractional_scale());

            let ctx = RenderCtx {
                renderer: vk,
                target: RenderTarget::ScreenCapture,
                xray: None,
            };
            let elements = niri.render_to_vec(ctx, output, false);
            let elements = elements.iter().rev();
            let pixels = render_to_vec(
                vk,
                size,
                scale,
                Transform::Normal,
                Fourcc::Abgr8888,
                elements,
            )?;
            Ok((pixels, size.w, size.h))
        })
        .expect("headless backend must hold a Vulkan renderer")
        .expect("compositing through Vulkan must not error")
}

#[test]
fn vulkan_composites_a_mapped_window() {
    let Some(mut f) = green_window_fixture() else {
        return;
    };
    let output = f.niri_output(1);

    // Composite the whole output through the owned Vulkan renderer — the same element collection
    // (`Niri::render_to_vec`) and offscreen readback (`render_helpers::render_to_vec`) that
    // `Niri::screenshot` drives. Reaching pixels at all proves the guarded GLES-only sub-paths
    // degraded instead of panicking.
    let state = f.niri_state();
    let (pixels, w, h) = state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| -> anyhow::Result<(Vec<u8>, i32, i32)> {
            let niri = &mut state.niri;
            niri.update_render_elements(Some(&output));

            let size: Size<i32, Physical> = output.current_mode().unwrap().size;
            let size = output.current_transform().transform_size(size);
            let scale = Scale::from(output.current_scale().fractional_scale());

            let ctx = RenderCtx {
                renderer: vk,
                target: RenderTarget::ScreenCapture,
                xray: None,
            };
            let elements = niri.render_to_vec(ctx, &output, false);
            let elements = elements.iter().rev();
            let pixels = render_to_vec(
                vk,
                size,
                scale,
                Transform::Normal,
                Fourcc::Abgr8888,
                elements,
            )?;
            Ok((pixels, size.w, size.h))
        })
        .expect("headless backend must hold a Vulkan renderer")
        .expect("compositing through Vulkan must not error");

    let green = assert_window_and_background(&pixels, w, h);

    // Write the observable artifact next to the target dir.
    let path = std::env::temp_dir().join("vulkan_composited_window.png");
    image::save_buffer(
        &path,
        &pixels,
        w as u32,
        h as u32,
        image::ExtendedColorType::Rgba8,
    )
    .expect("write PNG");
    eprintln!(
        "vulkan_composites_a_mapped_window: {green} window px; wrote {}",
        path.display()
    );

    // Finally, smoke-test the genericized `Niri::screenshot` itself end-to-end on Vulkan (no disk
    // write, so no async encode thread to await): it must run the same path without panicking.
    let state = f.niri_state();
    let ran = state.backend.headless().with_vulkan_renderer(|vk| {
        state
            .niri
            .screenshot(vk, &output, false, false, None)
            .expect("Niri::screenshot must succeed on the Vulkan renderer");
    });
    assert!(
        ran.is_some(),
        "screenshot did not run on the Vulkan renderer"
    );
}

#[test]
fn vulkan_composites_the_run_dialog() {
    let Some(mut f) = green_window_fixture() else {
        return;
    };
    let output = f.niri_output(1);

    // Baseline: the static scene with no dialog.
    let (before, w, h) = render_output_vulkan(&mut f, &output);

    // Open the Alt+F2 run dialog, settle, and recomposite.
    f.niri_state().do_action(Action::ShowRunDialog, false);
    f.niri_complete_animations();
    assert!(f.niri().run_dialog.is_open(), "run dialog must be open");
    let (after, aw, ah) = render_output_vulkan(&mut f, &output);
    assert_eq!((w, h), (aw, ah), "output size changed between renders");

    // The old GLES-locked path early-returned before pushing *any* element on the Vulkan renderer,
    // so opening the dialog left the frame byte-identical. The genericized path uploads the
    // CPU-rendered dialog through Vulkan's `ImportMem` and draws it (plus its backdrop), so the
    // frame must now change.
    assert_ne!(
        before, after,
        "opening the run dialog did not change the Vulkan-composited frame (dialog skipped?)"
    );

    // Stronger than a global diff: the dialog box and its backdrop cover the output *center*, so
    // the center pixel must change — not merely some backdrop edge.
    let (cx, cy) = (w / 2, h / 2);
    assert_ne!(
        px(&before, w, cx, cy),
        px(&after, w, cx, cy),
        "the dialog did not composite at the output center"
    );
    eprintln!("vulkan_composites_the_run_dialog: {w}x{h} frame changed with the dialog open");
}

/// The KMS-present pipeline minus the flip: composite the same live scene straight into a
/// GBM-allocated **scanout dmabuf** via `Bind<Dmabuf>`, exactly as the tty present path will, and
/// read it back from the dmabuf's own memory. This proves the whole GPU half of Stage 3 Brick B —
/// `Niri::render` (Brick 3) → owned Vulkan renderer → scanout buffer (Brick A) — is correct; only
/// the DRM framebuffer export + atomic page-flip remain (live-validated). Venus-only (needs GBM).
#[test]
fn vulkan_composites_a_scene_into_a_scanout_dmabuf() {
    use std::fs::File;

    use smithay::backend::allocator::dmabuf::AsDmabuf;
    use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
    use smithay::backend::allocator::{Allocator, Buffer as _, Modifier};

    let Some(mut f) = green_window_fixture() else {
        return;
    };
    let output = f.niri_output(1);

    // Allocate a scanout-flagged GBM buffer the size of the output and export it as a Smithay
    // Dmabuf — the same allocation the tty backend performs for a scanout target.
    let file = match File::options()
        .read(true)
        .write(true)
        .open("/dev/dri/renderD128")
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "skipping vulkan_composites_a_scene_into_a_scanout_dmabuf: no render node ({e})"
            );
            return;
        }
    };
    let gbm = match GbmDevice::new(file) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("skipping vulkan_composites_a_scene_into_a_scanout_dmabuf: no GBM ({e})");
            return;
        }
    };
    let mut alloc = GbmAllocator::new(gbm, GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT);
    let bo = match alloc.create_buffer(
        u32::from(OUT_W),
        u32::from(OUT_H),
        Fourcc::Abgr8888,
        &[Modifier::Linear],
    ) {
        Ok(b) => b,
        Err(e) => {
            eprintln!(
                "skipping vulkan_composites_a_scene_into_a_scanout_dmabuf: GBM cannot allocate \
                 Abgr8888 LINEAR scanout buffer ({e})"
            );
            return;
        }
    };
    let mut dmabuf = bo.export().expect("export scanout dmabuf");
    eprintln!(
        "scanout dmabuf: {:?} {}x{} modifier {:?}",
        dmabuf.format().code,
        dmabuf.width(),
        dmabuf.height(),
        dmabuf.format().modifier,
    );

    let state = f.niri_state();
    let (pixels, w, h) = state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| -> anyhow::Result<(Vec<u8>, i32, i32)> {
            let niri = &mut state.niri;
            niri.update_render_elements(Some(&output));

            let size: Size<i32, Physical> = output.current_mode().unwrap().size;
            let size = output.current_transform().transform_size(size);
            let scale = Scale::from(output.current_scale().fractional_scale());

            // Collect the scene's elements for the Vulkan renderer.
            let ctx = RenderCtx {
                renderer: vk,
                target: RenderTarget::Output,
                xray: None,
            };
            let elements: Vec<OutputRenderElements<VulkanRenderer>> =
                niri.render_to_vec(ctx, &output, false);

            // Bind the scanout dmabuf as the render target and composite straight into it.
            let mut fb = vk
                .bind(&mut dmabuf)
                .map_err(|e| anyhow::anyhow!("bind dmabuf: {e}"))?;
            {
                let mut frame = vk
                    .render(&mut fb, size, Transform::Normal)
                    .map_err(|e| anyhow::anyhow!("render: {e}"))?;
                frame
                    .clear(Color32F::TRANSPARENT, &[Rectangle::from_size(size)])
                    .map_err(|e| anyhow::anyhow!("clear: {e}"))?;
                // Back-to-front (elements are front-to-back).
                for e in elements.iter().rev() {
                    let geo = Element::geometry(e, scale);
                    if let Some(mut damage) = Rectangle::from_size(size).intersection(geo) {
                        damage.loc -= geo.loc;
                        RenderElement::<VulkanRenderer>::draw(
                            e,
                            &mut frame,
                            Element::src(e),
                            geo,
                            &[damage],
                            &[],
                            None,
                        )
                        .map_err(|e| anyhow::anyhow!("draw: {e}"))?;
                    }
                }
                let _sync = frame.finish().map_err(|e| anyhow::anyhow!("finish: {e}"))?;
            }

            // Read back from the dmabuf's own memory — correct pixels prove the scene landed in the
            // scanout buffer.
            let region = Rectangle::<i32, BufferCoord>::from_size(Size::from((size.w, size.h)));
            let mapping = vk
                .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
                .map_err(|e| anyhow::anyhow!("copy_framebuffer: {e}"))?;
            let pixels = vk
                .map_texture(&mapping)
                .map_err(|e| anyhow::anyhow!("map_texture: {e}"))?
                .to_vec();
            Ok((pixels, size.w, size.h))
        })
        .expect("headless backend must hold a Vulkan renderer")
        .expect("compositing into the scanout dmabuf must not error");

    let green = assert_window_and_background(&pixels, w, h);

    let path = std::env::temp_dir().join("vulkan_scanout_dmabuf.png");
    image::save_buffer(
        &path,
        &pixels,
        w as u32,
        h as u32,
        image::ExtendedColorType::Rgba8,
    )
    .expect("write PNG");
    eprintln!(
        "vulkan_composites_a_scene_into_a_scanout_dmabuf: {green} window px; wrote {}",
        path.display()
    );
}

/// The present-blit scanout path (KMS planes that want `Argb8888`/`Xrgb8888`): composite the live
/// scene into a GBM `Argb8888` scanout dmabuf via `Bind<Dmabuf>` — which renders into an R8G8B8A8
/// shadow and blits it into the dmabuf, reordering RGBA→BGRA — then read the dmabuf back (through
/// Vulkan, `ExportMem`, which now targets the scanout buffer) and prove an opaque-**red** window
/// landed as the BGRA bytes `[0,0,255,255]`. This is the exact path the virtio-gpu tty target takes
/// (its primary plane advertises only XR24/AR24). Venus-only (needs GBM).
#[test]
fn vulkan_composites_a_scene_into_an_argb_scanout_dmabuf() {
    use std::fs::File;

    use smithay::backend::allocator::dmabuf::AsDmabuf;
    use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
    use smithay::backend::allocator::{Allocator, Buffer as _, Modifier};

    let Some(mut f) = window_fixture(RED) else {
        return;
    };
    let output = f.niri_output(1);

    let file = match File::options()
        .read(true)
        .write(true)
        .open("/dev/dri/renderD128")
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("skipping vulkan_composites_a_scene_into_an_argb_scanout_dmabuf: no render node ({e})");
            return;
        }
    };
    let gbm = match GbmDevice::new(file) {
        Ok(d) => d,
        Err(e) => {
            eprintln!(
                "skipping vulkan_composites_a_scene_into_an_argb_scanout_dmabuf: no GBM ({e})"
            );
            return;
        }
    };
    let mut alloc = GbmAllocator::new(gbm, GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT);
    let bo = match alloc.create_buffer(
        u32::from(OUT_W),
        u32::from(OUT_H),
        Fourcc::Argb8888,
        &[Modifier::Linear],
    ) {
        Ok(b) => b,
        Err(e) => {
            eprintln!(
                "skipping vulkan_composites_a_scene_into_an_argb_scanout_dmabuf: GBM cannot \
                 allocate Argb8888 LINEAR scanout buffer ({e})"
            );
            return;
        }
    };
    let mut dmabuf = bo.export().expect("export scanout dmabuf");
    eprintln!(
        "argb scanout dmabuf: {:?} {}x{} modifier {:?}",
        dmabuf.format().code,
        dmabuf.width(),
        dmabuf.height(),
        dmabuf.format().modifier,
    );

    let state = f.niri_state();
    let (pixels, w, h) = state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| -> anyhow::Result<(Vec<u8>, i32, i32)> {
            let niri = &mut state.niri;
            niri.update_render_elements(Some(&output));

            let size: Size<i32, Physical> = output.current_mode().unwrap().size;
            let size = output.current_transform().transform_size(size);
            let scale = Scale::from(output.current_scale().fractional_scale());

            let ctx = RenderCtx {
                renderer: vk,
                target: RenderTarget::Output,
                xray: None,
            };
            let elements: Vec<OutputRenderElements<VulkanRenderer>> =
                niri.render_to_vec(ctx, &output, false);

            // Bind the Argb dmabuf: `Bind<Dmabuf>` takes the present-blit path (shadow + blit).
            let mut fb = vk
                .bind(&mut dmabuf)
                .map_err(|e| anyhow::anyhow!("bind dmabuf: {e}"))?;
            {
                let mut frame = vk
                    .render(&mut fb, size, Transform::Normal)
                    .map_err(|e| anyhow::anyhow!("render: {e}"))?;
                frame
                    .clear(Color32F::TRANSPARENT, &[Rectangle::from_size(size)])
                    .map_err(|e| anyhow::anyhow!("clear: {e}"))?;
                for e in elements.iter().rev() {
                    let geo = Element::geometry(e, scale);
                    if let Some(mut damage) = Rectangle::from_size(size).intersection(geo) {
                        damage.loc -= geo.loc;
                        RenderElement::<VulkanRenderer>::draw(
                            e,
                            &mut frame,
                            Element::src(e),
                            geo,
                            &[damage],
                            &[],
                            None,
                        )
                        .map_err(|e| anyhow::anyhow!("draw: {e}"))?;
                    }
                }
                // `finish` runs the present-blit shadow→dmabuf, then CPU-waits for completion.
                let _sync = frame.finish().map_err(|e| anyhow::anyhow!("finish: {e}"))?;
            }

            // Read back the *scanout* buffer (`copy_framebuffer` follows the present-blit to the
            // dmabuf) — the bytes are Argb8888 order, i.e. BGRA.
            let region = Rectangle::<i32, BufferCoord>::from_size(Size::from((size.w, size.h)));
            let mapping = vk
                .copy_framebuffer(&fb, region, Fourcc::Argb8888)
                .map_err(|e| anyhow::anyhow!("copy_framebuffer: {e}"))?;
            let pixels = vk
                .map_texture(&mapping)
                .map_err(|e| anyhow::anyhow!("map_texture: {e}"))?
                .to_vec();
            Ok((pixels, size.w, size.h))
        })
        .expect("headless backend must hold a Vulkan renderer")
        .expect("present-blit compositing into the scanout dmabuf must not error");

    // Argb8888 bytes are `[B, G, R, A]`. The opaque-red window must read `[0, 0, 255, 255]` — a raw
    // (non-reordering) copy would instead leave red as `[255, 0, 0, 255]`, so this proves the blit
    // reordered RGBA→BGRA.
    assert_eq!(
        pixels.len(),
        (w * h * 4) as usize,
        "unexpected readback size"
    );
    let is_red_bgra = |p: [u8; 4]| p[0] < 40 && p[1] < 40 && p[2] > 200 && p[3] > 200;
    let red = (0..w * h)
        .filter(|i| is_red_bgra(px(&pixels, w, i % w, i / w)))
        .count();
    assert!(
        red > 0,
        "the red window is absent, or the present-blit did not reorder RGBA→BGRA"
    );
    assert!(
        red < (w * h) as usize,
        "the frame is uniformly the window color; nothing else composited"
    );

    eprintln!("vulkan_composites_a_scene_into_an_argb_scanout_dmabuf: {red} red (BGRA) window px");
}

/// Render several frames through the present-blit path on **one** renderer + one Argb scanout
/// dmabuf, each a different solid color, and check every frame reads back its own color. This
/// guards the reused present-blit shadow (kept across frames so `bind` doesn't allocate a
/// full-screen device image every frame — the churn that aborted Venus live): a stale or
/// incorrectly-recleared shadow would bleed the previous frame's color. Venus-only (needs GBM).
#[test]
fn vulkan_reuses_present_blit_shadow_across_frames() {
    use std::fs::File;

    use smithay::backend::allocator::dmabuf::AsDmabuf;
    use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
    use smithay::backend::allocator::{Allocator, Modifier};

    let mut vk = match VulkanRenderer::new() {
        Ok(vk) => vk,
        Err(e) => {
            eprintln!("skipping vulkan_reuses_present_blit_shadow_across_frames: no Vulkan ({e})");
            return;
        }
    };

    let file = match File::options()
        .read(true)
        .write(true)
        .open("/dev/dri/renderD128")
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "skipping vulkan_reuses_present_blit_shadow_across_frames: no render node ({e})"
            );
            return;
        }
    };
    let gbm = match GbmDevice::new(file) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("skipping vulkan_reuses_present_blit_shadow_across_frames: no GBM ({e})");
            return;
        }
    };
    const S: i32 = 128;
    let mut alloc = GbmAllocator::new(gbm, GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT);
    let bo = match alloc.create_buffer(S as u32, S as u32, Fourcc::Argb8888, &[Modifier::Linear]) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping vulkan_reuses_present_blit_shadow_across_frames: GBM alloc ({e})");
            return;
        }
    };
    let mut dmabuf = bo.export().expect("export scanout dmabuf");

    let size = Size::<i32, Physical>::from((S, S));
    let region = Rectangle::<i32, BufferCoord>::from_size(Size::from((S, S)));
    // (render color, expected Argb8888/BGRA readback). Red then blue then green: each frame differs
    // from the last, so a shadow that wasn't re-cleared would fail on frame 2+.
    let frames: [(Color32F, [u8; 4]); 3] = [
        (Color32F::from([1., 0., 0., 1.]), [0, 0, 255, 255]),
        (Color32F::from([0., 0., 1., 1.]), [255, 0, 0, 255]),
        (Color32F::from([0., 1., 0., 1.]), [0, 255, 0, 255]),
    ];

    for (i, (color, want)) in frames.iter().enumerate() {
        let mut fb = vk.bind(&mut dmabuf).expect("bind");
        {
            let mut frame = vk.render(&mut fb, size, Transform::Normal).expect("render");
            frame
                .clear(*color, &[Rectangle::from_size(size)])
                .expect("clear");
            let _ = frame.finish().expect("finish");
        }
        let mapping = vk
            .copy_framebuffer(&fb, region, Fourcc::Argb8888)
            .expect("copy_framebuffer");
        let pixels = vk.map_texture(&mapping).expect("map_texture").to_vec();
        let p = px(&pixels, S, S / 2, S / 2);
        let near = |a: u8, b: u8| (i16::from(a) - i16::from(b)).abs() < 40;
        assert!(
            p.iter().zip(want).all(|(a, b)| near(*a, *b)),
            "frame {i}: expected BGRA {want:?}, got {p:?} (stale/reused shadow not re-cleared?)"
        );
    }
    eprintln!("vulkan_reuses_present_blit_shadow_across_frames: 3 frames each read back correctly");
}

/// Bind → render → finish the **same** scanout dmabuf a few hundred times, the way the live tty
/// present loop does every frame against `DrmCompositor`'s handful of cycled GBM buffers. Each
/// `bind` re-imports the dmabuf as a Vulkan image, which on Venus creates a host-side resource;
/// re-importing the same buffer every frame churns those host resources and, after some seconds
/// live, trips the ring `FATAL` bit → `abort()` (no guest OOM). This test reproduces that churn
/// deterministically: it must complete all iterations without aborting. Venus-only (needs GBM;
/// lavapipe has no host-import churn to speak of).
#[test]
fn vulkan_reimporting_a_scanout_target_every_frame_does_not_abort() {
    use std::fs::File;

    use smithay::backend::allocator::dmabuf::AsDmabuf;
    use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
    use smithay::backend::allocator::{Allocator, Modifier};

    let mut vk = match VulkanRenderer::new() {
        Ok(vk) => vk,
        Err(e) => {
            eprintln!("skipping vulkan_reimporting_a_scanout_target...: no Vulkan ({e})");
            return;
        }
    };
    let file = match File::options()
        .read(true)
        .write(true)
        .open("/dev/dri/renderD128")
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("skipping vulkan_reimporting_a_scanout_target...: no render node ({e})");
            return;
        }
    };
    let gbm = match GbmDevice::new(file) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("skipping vulkan_reimporting_a_scanout_target...: no GBM ({e})");
            return;
        }
    };
    let mut alloc = GbmAllocator::new(gbm, GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT);
    // Full 720p, like a real scanout buffer, so the per-import host cost matches live.
    let bo = match alloc.create_buffer(
        u32::from(OUT_W),
        u32::from(OUT_H),
        Fourcc::Argb8888,
        &[Modifier::Linear],
    ) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping vulkan_reimporting_a_scanout_target...: GBM alloc ({e})");
            return;
        }
    };
    let mut dmabuf = bo.export().expect("export scanout dmabuf");

    let size = Size::<i32, Physical>::from((i32::from(OUT_W), i32::from(OUT_H)));
    // ~10s of live rendering at 60 Hz. Live crashed at 7–39s, so this comfortably covers it.
    const FRAMES: usize = 600;
    for i in 0..FRAMES {
        let mut fb = vk.bind(&mut dmabuf).expect("bind");
        {
            let mut frame = vk.render(&mut fb, size, Transform::Normal).expect("render");
            frame
                .clear(
                    Color32F::from([0., 0., 1., 1.]),
                    &[Rectangle::from_size(size)],
                )
                .expect("clear");
            let _ = frame.finish().expect("finish");
        }
        if i % 100 == 0 {
            eprintln!("vulkan_reimporting_a_scanout_target...: frame {i}/{FRAMES} ok");
        }
    }
    eprintln!("vulkan_reimporting_a_scanout_target...: survived {FRAMES} re-imports");
}

/// A resize animation on a Vulkan session must draw the cross-fade (`render_resize`), not the red
/// `SolidColorBuffer` placeholder. Reproduces the live "the window becomes a red rect while
/// maximizing/restoring" bug: map a window, issue a niri-driven (animated) resize, commit the new
/// size, and composite mid-animation — the frame must show window content with no pure-red fill.
///
/// This exercises the full dual-renderer path the Tty backend uses (snapshot captured through the
/// coexisting GLES renderer, composited through Vulkan), which the headless backend now mirrors.
#[test]
fn vulkan_resize_animation_is_not_a_red_rect() {
    use niri_config::animations::{Curve, EasingParams, Kind};
    use niri_ipc::SizeChange;

    if VulkanRenderer::new().is_err() {
        eprintln!("skipping vulkan_resize_animation_is_not_a_red_rect: no Vulkan device");
        return;
    }

    const LINEAR: Kind = Kind::Easing(EasingParams {
        duration_ms: 1000,
        curve: Curve::Linear,
    });
    let mut config = Config::default();
    config.animations.window_resize.anim.kind = LINEAR;

    let mut f = Fixture::with_config_and_renderer(config, RendererKind::Vulkan);
    f.niri_state()
        .backend
        .headless()
        .add_renderer()
        .expect("build the GLES + Vulkan renderers");
    f.add_output(1, (OUT_W, OUT_H));

    let id = f.add_client();
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    // A real shm-textured buffer (not single-pixel) so the snapshot path can bake it.
    let window = f.client(id).window(&surface);
    window.attach_shm_buffer(WIN as i32, WIN as i32, 0, 255, 0, 255);
    window.set_size(WIN, WIN);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.niri_complete_animations();
    f.double_roundtrip(id);

    // Issue a niri-driven resize (this animates, like a keybind maximize).
    f.niri().layout.set_column_width(SizeChange::SetFixed(900));
    f.double_roundtrip(id);

    // The client commits the new size, which starts the resize animation.
    let window = f.client(id).window(&surface);
    window.attach_shm_buffer(900, WIN as i32, 0, 255, 0, 255);
    window.set_size(900, WIN);
    window.ack_last_and_commit();
    f.roundtrip(id);

    let output = f.niri_output(1);
    assert!(
        f.niri().layout.are_animations_ongoing(Some(&output)),
        "expected an ongoing resize animation to composite"
    );

    let (pixels, w, h) = render_output_vulkan(&mut f, &output);

    let is_red = |p: [u8; 4]| p[0] > 200 && p[1] < 40 && p[2] < 40;
    let is_green = |p: [u8; 4]| p[0] < 40 && p[1] > 200 && p[2] < 40;
    let red = (0..w * h)
        .filter(|i| is_red(px(&pixels, w, i % w, i / w)))
        .count();
    let green = (0..w * h)
        .filter(|i| is_green(px(&pixels, w, i % w, i / w)))
        .count();
    eprintln!("vulkan_resize_animation_is_not_a_red_rect: {green} green px, {red} red px");
    assert!(green > 0, "no window content in the resize frame");
    assert!(
        red < 100,
        "resize rendered the red placeholder ({red} red px) instead of the cross-fade"
    );
}

#[test]
fn vulkan_renders_a_window_mid_open_animation() {
    // The tile open animation renders through a GLES offscreen; before it was guarded, mapping a
    // window (which starts that animation) panicked on the owned Vulkan renderer ("this render path
    // requires a GLES-backed renderer"). Build the scene WITHOUT settling animations so the open
    // animation is active, and composite — it must degrade (skip the offscreen effect) and render
    // the window instead of panicking.
    let Some(mut f) = window_fixture_settled(GREEN, false) else {
        return;
    };
    let output = f.niri_output(1);
    assert!(
        f.niri().layout.are_animations_ongoing(Some(&output)),
        "expected the window open animation to be active (the guarded as_gles render path)"
    );

    // Must not panic; reaching pixels proves the open-animation branch degraded to a plain render.
    let (pixels, w, h) = render_output_vulkan(&mut f, &output);
    let green = assert_window_and_background(&pixels, w, h);
    eprintln!("vulkan_renders_a_window_mid_open_animation: composited {green} window px, no panic");
}

/// Import a **CPU-filled client dmabuf** (a GBM `Argb8888` LINEAR buffer painted with four known
/// quadrant colors) through `ImportDma::import_dmabuf` — the path real GPU app windows take — then
/// sample it 1:1 into an offscreen and read it back, proving the owned renderer imports client
/// dmabufs with the Argb→BGRA byte order and orientation correct. Venus-only (needs GBM). The
/// buffer is CPU-filled, so this validates import+sample+byte-order, not producer-side GPU
/// synchronization.
#[test]
fn vulkan_imports_a_client_dmabuf() {
    use niri_vk::dmabuf::ForeignBuffer;
    use smithay::backend::allocator::dmabuf::{Dmabuf, DmabufFlags};
    use smithay::backend::allocator::Modifier;
    use smithay::backend::renderer::element::Kind;
    use smithay::backend::renderer::ImportDma;
    use smithay::utils::Point;

    use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};

    let mut vk = match VulkanRenderer::new() {
        Ok(vk) => vk,
        Err(e) => {
            eprintln!("skipping vulkan_imports_a_client_dmabuf: no Vulkan device ({e})");
            return;
        }
    };

    // A 64×64 Argb8888 LINEAR dmabuf, four quadrants: TL red, TR green, BL blue, BR yellow (the
    // `[R,G,B,A]` colors are written BGRA into the Argb buffer by `allocate_filled`).
    const S: u32 = 64;
    let fb = match ForeignBuffer::allocate_filled(
        S,
        S,
        [
            [255, 0, 0, 255],
            [0, 255, 0, 255],
            [0, 0, 255, 255],
            [255, 255, 0, 255],
        ],
    ) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping vulkan_imports_a_client_dmabuf: GBM cannot allocate ({e})");
            return;
        }
    };

    // Wrap it as a Smithay client Dmabuf (single plane, explicit LINEAR).
    let mut builder = Dmabuf::builder(
        (S as i32, S as i32),
        Fourcc::Argb8888,
        Modifier::Linear,
        DmabufFlags::empty(),
    );
    assert!(builder.add_plane(
        fb.fd().try_clone_to_owned().expect("dup fd"),
        0,
        fb.offset,
        fb.stride,
    ));
    let dmabuf = builder.build().expect("build dmabuf");

    // The path a real client window takes.
    let imported = vk
        .import_dmabuf(&dmabuf, None)
        .expect("import client dmabuf");

    // Sample it 1:1 into an offscreen and read back tight Abgr8888 (`[R,G,B,A]`).
    let size = Size::<i32, Physical>::from((S as i32, S as i32));
    let buffer = TextureBuffer::from_texture(&vk, imported, 1.0, Transform::Normal, Vec::new());
    let element = TextureRenderElement::from_texture_buffer(
        buffer,
        Point::from((0.0, 0.0)),
        1.0,
        None,
        None,
        Kind::Unspecified,
    );
    let pixels = render_to_vec(
        &mut vk,
        size,
        Scale::from(1.0),
        Transform::Normal,
        Fourcc::Abgr8888,
        [element].into_iter(),
    )
    .expect("render imported dmabuf");

    // Sample the center of each quadrant; readback is `[R,G,B,A]`.
    let q = S as i32 / 4;
    let at = |x: i32, y: i32| px(&pixels, S as i32, x, y);
    let (tl, tr, bl, br) = (at(q, q), at(3 * q, q), at(q, 3 * q), at(3 * q, 3 * q));
    let near = |p: [u8; 4], want: [u8; 4]| {
        p.iter()
            .zip(want)
            .all(|(a, b)| (i16::from(*a) - i16::from(b)).abs() < 40)
    };
    assert!(near(tl, [255, 0, 0, 255]), "TL should be red, got {tl:?}");
    assert!(near(tr, [0, 255, 0, 255]), "TR should be green, got {tr:?}");
    assert!(near(bl, [0, 0, 255, 255]), "BL should be blue, got {bl:?}");
    assert!(
        near(br, [255, 255, 0, 255]),
        "BR should be yellow, got {br:?}"
    );
    eprintln!("vulkan_imports_a_client_dmabuf: quadrants TL={tl:?} TR={tr:?} BL={bl:?} BR={br:?}");
}
