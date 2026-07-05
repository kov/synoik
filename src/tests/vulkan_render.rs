//! End-to-end proof that the live `Niri::render` compositing path runs on the **owned Vulkan
//! renderer**, not just GLES: a real client window is mapped through the headless test harness, the
//! whole scene is composited to pixels through `VulkanRenderer`, and the result is read back and
//! checked. This exercises the renderer-agnostic render helpers (Brick 2) *and* the `try_as_gles`
//! degradation guards on the GLES-only sub-paths (xray capture, GNOME wallpaper, per-tile
//! background effect) that would otherwise panic a Vulkan render (Brick 3).
//!
//! Skips gracefully when no Vulkan device is present, like the other `--features vulkan` tests.

use niri_config::Config;
use smithay::backend::allocator::Fourcc;
use smithay::utils::{Physical, Scale, Size, Transform};

use super::fixture::Fixture;
use crate::backend::RendererKind;
use crate::render_helpers::vulkan::VulkanRenderer;
use crate::render_helpers::{render_to_vec, RenderCtx, RenderTarget};

const OUT_W: u16 = 1280;
const OUT_H: u16 = 720;
const WIN: u16 = 200;
// A saturated, opaque green window — distinct from any background/backdrop color, so its presence
// in the composited readback is unambiguous. Single-pixel-buffer channels are premultiplied and
// scaled so u32::MAX == 1.0.
const GREEN: [u32; 4] = [0, u32::MAX, 0, u32::MAX];

/// The tight `Abgr8888` pixel at (x, y) in a `w`-wide readback buffer.
fn px(pixels: &[u8], w: i32, x: i32, y: i32) -> [u8; 4] {
    let i = ((y * w + x) * 4) as usize;
    [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
}

#[test]
fn vulkan_composites_a_mapped_window() {
    // Cheap up-front device probe so we skip (not fail) on machines without Vulkan.
    if let Err(e) = VulkanRenderer::new() {
        eprintln!("skipping vulkan_composites_a_mapped_window: no Vulkan device ({e})");
        return;
    }

    let mut f = Fixture::with_config_and_renderer(Config::default(), RendererKind::Vulkan);
    f.niri_state()
        .backend
        .headless()
        .add_renderer()
        .expect("build the Vulkan renderer");
    f.add_output(1, (OUT_W, OUT_H));

    // Map one plain toplevel with an opaque green single-pixel buffer (smithay renders single-pixel
    // buffers as a solid color, so this composites through `draw_solid` with no client-buffer
    // import needed).
    let id = f.add_client();
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    window.attach_solid_buffer(GREEN[0], GREEN[1], GREEN[2], GREEN[3]);
    window.set_size(WIN, WIN);
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    // Settle any map/open animation so we screenshot a static scene.
    f.niri_complete_animations();
    f.double_roundtrip(id);

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

    assert_eq!(
        pixels.len(),
        (w * h * 4) as usize,
        "unexpected readback size"
    );

    // The window must be visible: some pixel is (close to) opaque green.
    let is_green = |p: [u8; 4]| p[0] < 40 && p[1] > 200 && p[2] < 40 && p[3] > 200;
    let green_count = (0..w * h)
        .filter(|i| is_green(px(&pixels, w, i % w, i / w)))
        .count();
    assert!(
        green_count > 0,
        "the mapped green window is absent from the Vulkan-composited frame"
    );
    // ...but the whole frame is not the window: the background/backdrop composited too.
    assert!(
        green_count < (w * h) as usize,
        "the frame is uniformly the window color; nothing else composited"
    );

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
        "vulkan_composites_a_mapped_window: {green_count} window px; wrote {}",
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
