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

    // Settle any map/open animation so we composite a static scene.
    f.niri_complete_animations();
    f.double_roundtrip(id);

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
