//! End-to-end proof that the live `Niri::render` compositing path runs on the **owned Vulkan
//! renderer**, not just GLES: a real client window is mapped through the headless test harness and
//! the whole scene is composited through `VulkanRenderer`, both into an offscreen buffer (the
//! screenshot path) and into a **GBM-allocated scanout dmabuf** (the KMS-present path — everything
//! except the DRM page-flip, which is validated live). Exercises the renderer-agnostic render
//! helpers (Brick 2), the `try_as_gles` degradation guards (Brick 3), and `Bind<Dmabuf>` (Brick A).
//!
//! Skips gracefully when no Vulkan device is present. The scanout test additionally needs a real
//! GBM device (the render node), so it is Venus-only (lavapipe/CPU has no GBM).

use niri_config::{Action, Config, CornerRadius, WindowRule};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::{Element, RenderElement};
use smithay::backend::renderer::{Bind, Color32F, ExportMem, Frame, Renderer};
use smithay::output::Output;
use smithay::utils::{Buffer as BufferCoord, Physical, Rectangle, Scale, Size, Transform};

use super::fixture::Fixture;
use crate::backend::RendererKind;
use crate::niri::OutputRenderElements;
use crate::render_helpers::dual_texture::DualTextureRenderElement;
use crate::render_helpers::vulkan::VulkanRenderer;
use crate::render_helpers::{render_to_vec, RenderCtx, RenderTarget};
use crate::utils::{output_size, to_physical_precise_round};

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
    render_output_vulkan_target(f, output, RenderTarget::ScreenCapture)
}

/// As [`render_output_vulkan`], but for an explicit [`RenderTarget`] — some overlays (e.g. the
/// screen transition) only composite through the owned renderer for `RenderTarget::Output`.
fn render_output_vulkan_target(
    f: &mut Fixture,
    output: &Output,
    target: RenderTarget,
) -> (Vec<u8>, i32, i32) {
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
                target,
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

/// The alt-tab MRU draws its window titles and scope panel as CPU/cairo text that was uploaded
/// through GLES-locked elements — blank on the owned Vulkan renderer. Now the text is a
/// renderer-neutral buffer uploaded through the active renderer. Open the MRU over a window and
/// composite the Output target through Vulkan: the white scope-panel text must be present (before,
/// only the thumbnails and dark backdrop drew).
#[test]
fn vulkan_mru_draws_the_scope_panel() {
    let Some(mut f) = green_window_fixture() else {
        return;
    };
    let output = f.niri_output(1);

    let clock = f.niri().clock.clone();
    let wmru = crate::ui::mru::WindowMru::new(f.niri());
    f.niri().window_mru_ui.open(clock, wmru, output.clone());
    assert!(f.niri().window_mru_ui.is_open(), "MRU must be open");
    // Settle the open animation so the MRU renders directly (alpha == 1).
    f.niri_complete_animations();

    let (pixels, w, h) = render_output_vulkan_target(&mut f, &output, RenderTarget::Output);

    // The scope panel ("Scope: All Output Workspace") sits in a strip near the top, well above the
    // centered thumbnails and their focus ring. Its white text over the dark backdrop is
    // unambiguous there — white pixels in the top strip ⟹ the CPU panel uploaded and drew on
    // Vulkan.
    let is_white = |p: [u8; 4]| p[0] > 200 && p[1] > 200 && p[2] > 200;
    let top = h / 8;
    let white = (0..top * w)
        .filter(|i| is_white(px(&pixels, w, i % w, i / w)))
        .count();
    eprintln!("vulkan_mru_draws_the_scope_panel: {white} white px in the top strip");
    // The blank (old) behavior leaves ~0 white in this strip, so a low threshold still
    // discriminates clearly while tolerating font-metric variation.
    assert!(
        white > 40,
        "the MRU scope panel text did not draw on Vulkan (blank overlay?): {white} white px"
    );
}

/// During the alt-tab closing fade the MRU renders itself into an offscreen (to avoid transparent
/// compositing artifacts), then composites that at a fading alpha. The offscreen was GLES-only, so
/// on the owned Vulkan renderer the fade fell through to just the dark backdrop — the thumbnails
/// and scope panel vanished the instant the fade began. Now it renders into a `VkTexture`
/// offscreen. Open the MRU, start the close, step a little into the spring so `alpha < 1` (the
/// offscreen path), and composite the Output target through Vulkan: the white scope-panel text must
/// still be present (the old blank offscreen would have left only the backdrop).
#[test]
fn vulkan_mru_closing_fade_draws_through_the_offscreen() {
    let Some(mut f) = green_window_fixture() else {
        return;
    };
    let output = f.niri_output(1);

    let clock = f.niri().clock.clone();
    let wmru = crate::ui::mru::WindowMru::new(f.niri());
    f.niri().window_mru_ui.open(clock, wmru, output.clone());
    // Settle the open animation so the MRU is fully open — a close before that skips the fade.
    f.niri_complete_animations();
    assert!(f.niri().window_mru_ui.is_open(), "MRU must be open");

    // Start the close, then advance ~16 ms into the critically-damped fade spring so `alpha` is a
    // little below 1 (≈0.9) — enough to take the offscreen path yet keep the text bright.
    f.niri()
        .window_mru_ui
        .close(crate::ui::mru::MruCloseRequest::Cancel);
    let now = f.niri().clock.now_unadjusted();
    f.niri()
        .clock
        .set_unadjusted(now + std::time::Duration::from_millis(16));
    f.niri().advance_animations();

    let (pixels, w, h) = render_output_vulkan_target(&mut f, &output, RenderTarget::Output);

    // Same white-text discriminator as the open case: at ~0.9 alpha the scope-panel text stays well
    // above 200, while the old blank-offscreen fade leaves ~0 white here (only the dark backdrop
    // faded over the desktop).
    let is_white = |p: [u8; 4]| p[0] > 200 && p[1] > 200 && p[2] > 200;
    let top = h / 8;
    let white = (0..top * w)
        .filter(|i| is_white(px(&pixels, w, i % w, i / w)))
        .count();
    eprintln!(
        "vulkan_mru_closing_fade_draws_through_the_offscreen: {white} white px in the top strip"
    );
    assert!(
        white > 40,
        "the MRU closing fade did not render through the Vulkan offscreen (blank fade?): {white} white px"
    );
}

/// The screenshot UI freezes the screen into a GLES texture the owned Vulkan renderer can't sample,
/// so on a Vulkan session it reads that capture back and uploads it to a `VkTexture` for the Output
/// target. Open the UI over a green-window scene, then composite the Output target through Vulkan:
/// with the UI open the compositor draws only the UI, so the frozen green screen must be present (a
/// blank no-op overlay — the old behavior — would leave only the dark backdrop).
#[test]
fn vulkan_screenshot_ui_draws_the_frozen_screen() {
    let Some(mut f) = green_window_fixture() else {
        return;
    };
    let output = f.niri_output(1);

    f.niri_state().open_screenshot_ui(false, None);
    assert!(
        f.niri().screenshot_ui.is_open(),
        "screenshot UI must be open"
    );
    // Settle the open animation so the UI is at full opacity.
    f.niri_complete_animations();

    let (pixels, w, h) = render_output_vulkan_target(&mut f, &output, RenderTarget::Output);

    // Green-dominant, to catch both the bright (selected) and the 0.5-darkened (unselected) regions
    // of the frozen screenshot; the dark backdrop is not green-dominant.
    let is_greenish = |p: [u8; 4]| {
        let (r, g, b) = (p[0] as i16, p[1] as i16, p[2] as i16);
        g > 60 && g > r + 30 && g > b + 30
    };
    let green = (0..w * h)
        .filter(|i| is_greenish(px(&pixels, w, i % w, i / w)))
        .count();
    eprintln!("vulkan_screenshot_ui_draws_the_frozen_screen: {green} greenish px");
    assert!(
        green > 1000,
        "the frozen screenshot did not draw on Vulkan (blank overlay?): {green} greenish px"
    );
}

/// The screen-transition crossfade captures the screen through GLES (which the owned Vulkan
/// renderer can't sample), so on a Vulkan session it uploads that neutral capture to a `VkTexture`
/// for the Output target. Freeze a green-window screen, recolor the live window red, then composite
/// the Output target through Vulkan: the frozen green frame must draw and occlude the live red
/// window (a blank no-op overlay — the old behavior — would leak the live red window through
/// instead).
#[test]
fn vulkan_screen_transition_draws_the_captured_frame() {
    if VulkanRenderer::new().is_err() {
        eprintln!("skipping vulkan_screen_transition_draws_the_captured_frame: no Vulkan device");
        return;
    }

    let mut f = Fixture::with_config_and_renderer(Config::default(), RendererKind::Vulkan);
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

    let window = f.client(id).window(&surface);
    window.attach_solid_buffer(GREEN[0], GREEN[1], GREEN[2], GREEN[3]);
    window.set_size(WIN, WIN);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.niri_complete_animations();
    f.double_roundtrip(id);

    let output = f.niri_output(1);

    // Freeze the green-window screen into a transition (delay 0 → alpha ≈ 1, fully the capture).
    f.niri_state()
        .do_action(Action::DoScreenTransition(Some(0)), false);
    assert!(
        f.niri()
            .output_state
            .values()
            .any(|s| s.screen_transition.is_some()),
        "screen transition must be active after DoScreenTransition"
    );

    // The Output target must take the Vulkan upload path; the screencast/screen-capture targets
    // keep the GLES texture (a degraded no-op on the Vulkan renderer — in production they
    // render through GLES, so only Output needs the upload).
    {
        let state = f.niri_state();
        state
            .backend
            .headless()
            .with_vulkan_renderer(|vk| {
                let transition = state
                    .niri
                    .output_state
                    .values()
                    .find_map(|s| s.screen_transition.as_ref())
                    .expect("active transition");
                assert!(
                    matches!(
                        transition.render(vk, RenderTarget::Output),
                        DualTextureRenderElement::Vulkan(_)
                    ),
                    "Output target did not upload the capture to a VkTexture"
                );
                assert!(
                    matches!(
                        transition.render(vk, RenderTarget::ScreenCapture),
                        DualTextureRenderElement::Gles(_)
                    ),
                    "ScreenCapture target should keep the GLES texture"
                );
            })
            .expect("headless backend must hold a Vulkan renderer");
    }

    // Recolor the live window red. The frozen transition still holds the green capture.
    let window = f.client(id).window(&surface);
    window.attach_solid_buffer(RED[0], RED[1], RED[2], RED[3]);
    window.commit();
    f.double_roundtrip(id);

    // Composite the Output target: the frozen green frame must occlude the live red window.
    let (pixels, w, h) = render_output_vulkan_target(&mut f, &output, RenderTarget::Output);
    let is_green = |p: [u8; 4]| p[0] < 40 && p[1] > 200 && p[2] < 40 && p[3] > 200;
    let is_red = |p: [u8; 4]| p[0] > 200 && p[1] < 40 && p[2] < 40;
    let green = (0..w * h)
        .filter(|i| is_green(px(&pixels, w, i % w, i / w)))
        .count();
    let red = (0..w * h)
        .filter(|i| is_red(px(&pixels, w, i % w, i / w)))
        .count();
    eprintln!("vulkan_screen_transition_draws_the_captured_frame: {green} green px, {red} red px");
    assert!(
        green > 0,
        "the captured (green) transition frame did not draw on Vulkan (blank overlay?)"
    );
    assert!(
        red < 100,
        "the live red window leaked through the frozen transition ({red} red px)"
    );
}

/// Import `pattern` (an `out_size`-shaped `Abgr8888` buffer... actually `tex`-shaped) as a texture
/// with buffer transform `src_transform`, render it full-screen through `render_texture_from_to`
/// at output transform `out_transform`, and return the four (inset) corner pixels of the readback.
/// Generic so it drives GLES and the Vulkan renderer identically.
#[cfg(feature = "vulkan")]
fn buffer_transform_corners<R, T>(
    renderer: &mut R,
    pattern: &[u8],
    tex: Size<i32, BufferCoord>,
    out_size: Size<i32, Physical>,
    src_transform: Transform,
    out_transform: Transform,
) -> [[u8; 4]; 4]
where
    R: Renderer<TextureId = T>
        + smithay::backend::renderer::Offscreen<T>
        + ExportMem
        + smithay::backend::renderer::ImportMem,
    T: smithay::backend::renderer::Texture + Clone + 'static,
    R::Error: std::fmt::Debug + Send + Sync + 'static,
{
    use smithay::backend::renderer::element::Kind;

    use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};

    let tb = TextureBuffer::from_memory(
        renderer,
        pattern,
        Fourcc::Abgr8888,
        tex,
        false,
        1.0,
        src_transform,
        Vec::new(),
    )
    .expect("import pattern");
    let elem =
        TextureRenderElement::from_texture_buffer(tb, (0., 0.), 1.0, None, None, Kind::Unspecified);
    let pixels = render_to_vec(
        renderer,
        out_size,
        Scale::from(1.0),
        out_transform,
        Fourcc::Abgr8888,
        [elem].into_iter(),
    )
    .expect("render");
    let (w, h) = (out_size.w, out_size.h);
    let m = 6;
    [
        px(&pixels, w, m, m),
        px(&pixels, w, w - 1 - m, m),
        px(&pixels, w, m, h - 1 - m),
        px(&pixels, w, w - 1 - m, h - 1 - m),
    ]
}

/// Buffer-transform (`src_transform`) conformance: a texture with a non-Normal buffer transform
/// must sample identically to the GLES oracle — the second, independent transform axis from the
/// output projection. This is what un-blanks the DualTexture capture overlays (screen-transition /
/// screenshot / MRU) on a rotated output, where they carry `src_transform == output_transform`.
/// Renders a 4-colour-quadrant texture through `render_texture_from_to` via BOTH renderers at all 8
/// buffer transforms (Normal output) plus a few buffer×output crosses, on a non-square target.
#[test]
fn vulkan_buffer_transform_matches_the_gles_oracle() {
    use smithay::utils::Transform::*;

    let Some(mut f) = window_fixture(GREEN) else {
        return;
    };

    // Non-square texture + output so a w/h swap can't cancel.
    const W: i32 = 160;
    const H: i32 = 100;
    let tex = Size::<i32, BufferCoord>::from((W, H));
    let out_size = Size::<i32, Physical>::from((W, H));

    // 4-quadrant pattern in buffer (top-left origin) space: TL red, TR green, BL blue, BR white.
    let red = [255u8, 0, 0, 255];
    let green = [0u8, 255, 0, 255];
    let blue = [0u8, 0, 255, 255];
    let white = [255u8, 255, 255, 255];
    let mut pattern = Vec::with_capacity((W * H * 4) as usize);
    for y in 0..H {
        for x in 0..W {
            let c = match (x < W / 2, y < H / 2) {
                (true, true) => red,
                (false, true) => green,
                (true, false) => blue,
                (false, false) => white,
            };
            pattern.extend_from_slice(&c);
        }
    }

    let sweep = [
        Normal, _90, _180, _270, Flipped, Flipped90, Flipped180, Flipped270,
    ];
    let combos: Vec<(Transform, Transform)> = sweep
        .iter()
        .map(|t| (*t, Normal))
        // Crossed with a rotated output: buffer and output transforms are independent, so these
        // must still match the oracle (catches any proj × tex_transform interaction).
        .chain([(_90, _270), (Flipped90, _90), (_180, Flipped)])
        .collect();

    let state = f.niri_state();
    let mut normal_out = Vec::new();
    for (src_t, out_t) in combos {
        let gles = state
            .backend
            .headless()
            .with_primary_renderer(|g| {
                buffer_transform_corners(g, &pattern, tex, out_size, src_t, out_t)
            })
            .expect("GLES renderer present");
        let vk = state
            .backend
            .headless()
            .with_vulkan_renderer(|v| {
                buffer_transform_corners(v, &pattern, tex, out_size, src_t, out_t)
            })
            .expect("Vulkan renderer present");
        assert_eq!(
            gles, vk,
            "Vulkan corners != GLES at src_transform={src_t:?} out_transform={out_t:?}"
        );
        eprintln!("vulkan_buffer_transform src={src_t:?} out={out_t:?}: corners {vk:?} match GLES");
        if out_t == Normal {
            normal_out.push((src_t, vk));
        }
    }

    // Absolute anchors (no GLES) so a shared misunderstanding can't pass: Normal keeps the source
    // top-left (red) at the physical top-left; a _90 *buffer* rotation makes the physical top-left
    // sample the source bottom-left quadrant (blue).
    let corners = |t: Transform| normal_out.iter().find(|(s, _)| *s == t).unwrap().1;
    let near = |p: [u8; 4], c: [u8; 4]| (0..3).all(|i| (p[i] as i32 - c[i] as i32).abs() < 50);
    assert!(near(corners(Normal)[0], red), "Normal TL should be red");
    assert!(
        near(corners(_90)[0], blue),
        "_90 buffer transform: physical TL should sample source bottom-left (blue), got {:?}",
        corners(_90)[0]
    );
}

/// Output-transform conformance: the owned Vulkan renderer must place geometry identically to the
/// GLES oracle under every rotation/flip. Render an asymmetric marker (a wide red rect anchored at
/// the logical top-left) through *both* renderers at all 8 transforms and compare — GLES is the
/// production-correct renderer, so byte-for-agreement proves the Vulkan `proj` projection +
/// logical-`target` math. The framebuffer is **non-square** (240×120), so 90/270 genuinely swap
/// logical w/h — that's what exercises `target_dims`/`output_size` returning the logical size.
#[test]
fn vulkan_output_transform_matches_the_gles_oracle() {
    use smithay::backend::renderer::element::Kind;

    use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};

    let Some(mut f) = window_fixture(GREEN) else {
        return;
    };

    // Non-square so logical != physical under 90/270 (catches a physical-vs-logical `target` bug).
    const W: i32 = 240;
    const H: i32 = 120;
    let size = Size::<i32, Physical>::from((W, H));
    let scale = Scale::from(1.0);

    // A wide (2:1) red marker at the logical top-left: asymmetric in both axes, so its rendered
    // position/shape is distinct for each of the 8 transforms (no accidental symmetry). 80×40 fits
    // inside every logical orientation of a 240×120 output (portrait 120×240 included).
    let marker = SolidColorBuffer::new(Size::from((80.0, 40.0)), [1.0, 0.0, 0.0, 1.0]);
    let build = || {
        vec![SolidColorRenderElement::from_buffer(
            &marker,
            (0.0, 0.0),
            1.0,
            Kind::Unspecified,
        )]
    };

    // The tight bounding box of the red marker in an `Abgr8888` readback, or `None` if absent.
    let red_bbox = |pixels: &[u8]| -> Option<(i32, i32, i32, i32)> {
        let is_red = |p: [u8; 4]| p[0] > 200 && p[1] < 50 && p[2] < 50 && p[3] > 200;
        let (mut x0, mut y0, mut x1, mut y1) = (W, H, -1, -1);
        for y in 0..H {
            for x in 0..W {
                if is_red(px(pixels, W, x, y)) {
                    x0 = x0.min(x);
                    y0 = y0.min(y);
                    x1 = x1.max(x);
                    y1 = y1.max(y);
                }
            }
        }
        (x1 >= 0).then_some((x0, y0, x1, y1))
    };
    let red_count = |pixels: &[u8]| -> usize {
        let is_red = |p: [u8; 4]| p[0] > 200 && p[1] < 50 && p[2] < 50 && p[3] > 200;
        (0..W * H)
            .filter(|i| is_red(px(pixels, W, i % W, i / W)))
            .count()
    };

    let all = [
        Transform::Normal,
        Transform::_90,
        Transform::_180,
        Transform::_270,
        Transform::Flipped,
        Transform::Flipped90,
        Transform::Flipped180,
        Transform::Flipped270,
    ];

    let state = f.niri_state();
    let mut vk_boxes = Vec::new();
    for t in all {
        let gles = state
            .backend
            .headless()
            .with_primary_renderer(|g| {
                render_to_vec(g, size, scale, t, Fourcc::Abgr8888, build().into_iter())
            })
            .expect("GLES renderer present")
            .expect("GLES render must succeed");
        let vk = state
            .backend
            .headless()
            .with_vulkan_renderer(|v| {
                render_to_vec(v, size, scale, t, Fourcc::Abgr8888, build().into_iter())
            })
            .expect("Vulkan renderer present")
            .expect("Vulkan render must succeed");

        let gbox = red_bbox(&gles).unwrap_or_else(|| panic!("GLES marker missing at {t:?}"));
        let vbox = red_bbox(&vk).unwrap_or_else(|| panic!("Vulkan marker missing at {t:?}"));
        // Oracle agreement: the marker must land at the same place and cover the same area as the
        // production GLES renderer. Placement (bbox) catches a wrong rotation/flip; area (count)
        // catches a wrong aspect (e.g. logical w/h not swapped for 90/270).
        assert_eq!(
            gbox, vbox,
            "Vulkan marker bbox {vbox:?} != GLES {gbox:?} at {t:?}"
        );
        assert_eq!(
            red_count(&gles),
            red_count(&vk),
            "Vulkan red-pixel count != GLES at {t:?}"
        );
        eprintln!("vulkan_output_transform {t:?}: marker bbox {vbox:?} matches GLES");
        vk_boxes.push(vbox);
    }

    // Absolute anchor: Normal places the 80×40 marker flush in the physical top-left. `x1`/`y1` are
    // the inclusive max coords, hence 79/39.
    assert_eq!(
        vk_boxes[0],
        (0, 0, 79, 39),
        "Normal marker is not at the physical top-left"
    );
    // The transform is genuinely applied (not silently identity for all): the 8 placements are not
    // all the same box.
    assert!(
        vk_boxes.iter().any(|b| *b != vk_boxes[0]),
        "every transform produced the same marker box — proj not applied?"
    );
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

/// Damage-preserving (partial-damage) rendering: render a full RED frame into an Argb scanout
/// dmabuf, then a second frame that clears only a sub-rect to GREEN, then a third frame that does
/// an EMPTY clear and nothing else. The un-cleared region must read back RED (frame 1, preserved by
/// the LOAD pass), the sub-rect GREEN, and frame 3 must change nothing. Two things are proven: the
/// LOAD path is layout-correct end to end (right initial layout, no validation error, `begin`'s
/// preserve-gate fires and a preserved scanout reads back correctly), AND `clear` with an empty
/// rect slice is a no-op, not a whole-target wipe. That empty-clear case is the one the smithay
/// damage tracker hits every frame whose damage is fully covered by opaque elements (e.g. the
/// cursor over an opaque window); a "clear whole on empty" bug wiped the LOAD-preserved shadow
/// there and flickered the live desktop, and frame 3 catches it *independently* of whether the
/// driver discards on DONT_CARE (this Venus stack happens to retain the reused shadow's bits over a
/// short run, so the LOAD-vs-DONT_CARE distinction itself can only be confirmed live). Frame 4 then
/// draws a FULLSCREEN solid with only a small damage rect and asserts the rest of the scene
/// survives — proof that draws scissor to their per-element damage instead of repainting the whole
/// element (an unscissored fullscreen background repainting over everything the damage tracker
/// skipped was the dominant live-blanking cause). Venus-only (needs GBM). See `VulkanFrame::begin`,
/// `::clear`, `::draw_quad`.
#[test]
fn vulkan_preserves_undamaged_regions_across_frames() {
    use std::fs::File;

    use smithay::backend::allocator::dmabuf::AsDmabuf;
    use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
    use smithay::backend::allocator::{Allocator, Modifier};

    let mut vk = match VulkanRenderer::new() {
        Ok(vk) => vk,
        Err(e) => {
            eprintln!("skipping vulkan_preserves_undamaged_regions_across_frames: no Vulkan ({e})");
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
                "skipping vulkan_preserves_undamaged_regions_across_frames: no render node ({e})"
            );
            return;
        }
    };
    let gbm = match GbmDevice::new(file) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("skipping vulkan_preserves_undamaged_regions_across_frames: no GBM ({e})");
            return;
        }
    };
    const S: i32 = 128;
    let mut alloc = GbmAllocator::new(gbm, GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT);
    let bo = match alloc.create_buffer(S as u32, S as u32, Fourcc::Argb8888, &[Modifier::Linear]) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skipping vulkan_preserves_undamaged_regions_across_frames: GBM alloc ({e})");
            return;
        }
    };
    let mut dmabuf = bo.export().expect("export scanout dmabuf");

    let size = Size::<i32, Physical>::from((S, S));
    let region = Rectangle::<i32, BufferCoord>::from_size(Size::from((S, S)));

    // Frame 1: whole shadow RED. Leaves the shadow in TRANSFER_SRC_OPTIMAL (a valid prior frame).
    {
        let mut fb = vk.bind(&mut dmabuf).expect("bind");
        let mut frame = vk.render(&mut fb, size, Transform::Normal).expect("render");
        frame
            .clear(
                Color32F::from([1., 0., 0., 1.]),
                &[Rectangle::from_size(size)],
            )
            .expect("clear");
        let _ = frame.finish().expect("finish");
    }

    // Frame 2: clear ONLY a 32×32 top-left sub-rect to GREEN; touch nothing else. `begin` sees a
    // valid shadow and picks the LOAD pass, so the rest must survive as frame 1's RED.
    let patch = Rectangle::<i32, Physical>::from_size(Size::from((32, 32)));
    {
        let mut fb = vk.bind(&mut dmabuf).expect("bind");
        let mut frame = vk.render(&mut fb, size, Transform::Normal).expect("render");
        frame
            .clear(Color32F::from([0., 1., 0., 1.]), &[patch])
            .expect("clear");
        let _ = frame.finish().expect("finish");
    }

    // Frame 3: an EMPTY clear (the exact call the smithay damage tracker makes when the frame's
    // damage is fully covered by opaque elements) and nothing else. It must be a NO-OP — clear
    // nothing, not the whole target — so the readback is unchanged from frame 2. A "clear whole on
    // empty" bug would instead wipe the scene to this blue; that is what flickered the live
    // desktop.
    let mut fb = vk.bind(&mut dmabuf).expect("bind");
    {
        let mut frame = vk.render(&mut fb, size, Transform::Normal).expect("render");
        frame
            .clear(Color32F::from([0., 0., 1., 1.]), &[])
            .expect("clear");
        let _ = frame.finish().expect("finish");
    }
    let mapping = vk
        .copy_framebuffer(&fb, region, Fourcc::Argb8888)
        .expect("copy_framebuffer");
    let pixels = vk.map_texture(&mapping).expect("map_texture").to_vec();

    let near = |a: u8, b: u8| (i16::from(a) - i16::from(b)).abs() < 40;
    let is = |p: [u8; 4], want: [u8; 4]| p.iter().zip(want).all(|(a, b)| near(*a, b));

    // Inside the patch: GREEN (Argb8888/BGRA [0, 255, 0, 255]).
    let inside = px(&pixels, S, 16, 16);
    assert!(
        is(inside, [0, 255, 0, 255]),
        "the damaged sub-rect should be green, got {inside:?}"
    );
    // Outside the patch: RED preserved from frame 1 (Argb8888/BGRA [0, 0, 255, 255]). A DONT_CARE
    // load op would leave this undefined (black/garbage) on a tiler.
    let outside = px(&pixels, S, S / 2, S / 2);
    assert!(
        is(outside, [0, 0, 255, 255]),
        "undamaged region not preserved (render pass must LOAD): expected red [0,0,255,255], \
         got {outside:?}"
    );

    // Frame 4: draw a FULLSCREEN blue solid but with only a small 16×16 damage rect. The draw must
    // scissor to that rect; repainting the whole element would erase everything the damage tracker
    // legitimately skipped. This is the second half of the partial-damage bug — an unscissored
    // fullscreen background/backdrop element repainting over the preserved scene every frame.
    let hit = Rectangle::<i32, Physical>::new((S / 2, S / 2).into(), (16, 16).into());
    let mut fb = vk.bind(&mut dmabuf).expect("bind");
    {
        let mut frame = vk.render(&mut fb, size, Transform::Normal).expect("render");
        frame
            .draw_solid(
                Rectangle::from_size(size),
                &[hit],
                Color32F::from([0., 0., 1., 1.]),
            )
            .expect("draw");
        let _ = frame.finish().expect("finish");
    }
    let mapping = vk
        .copy_framebuffer(&fb, region, Fourcc::Argb8888)
        .expect("copy_framebuffer");
    let pixels = vk.map_texture(&mapping).expect("map_texture").to_vec();
    // The damaged rect is now BLUE (Argb8888/BGRA [255, 0, 0, 255]).
    let hit_px = px(&pixels, S, S / 2 + 8, S / 2 + 8);
    assert!(
        is(hit_px, [255, 0, 0, 255]),
        "the damaged rect should be blue, got {hit_px:?}"
    );
    // The GREEN patch (untouched by frame 4's damage) survived a FULLSCREEN draw — proof the draw
    // scissored to its damage instead of repainting the whole element.
    let patch_after = px(&pixels, S, 16, 16);
    assert!(
        is(patch_after, [0, 255, 0, 255]),
        "a fullscreen draw with small damage repainted undamaged pixels (green patch lost): \
         got {patch_after:?}"
    );

    eprintln!(
        "vulkan_preserves_undamaged_regions_across_frames: patch green, undamaged region preserved \
         red, fullscreen draw scissored to its damage"
    );
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

/// The GNOME top panel renders on the owned Vulkan renderer. It used to `try_as_gles_renderer()?`
/// and return `None` on Vulkan (a blank bar); now it uploads its CPU-rendered bar through the
/// active renderer's `ImportMem`. Assert `render` yields an element and that compositing it
/// produces the opaque bar (alpha 255), rather than nothing.
#[test]
fn vulkan_renders_the_top_panel() {
    let Some(mut f) = window_fixture(GREEN) else {
        return;
    };
    let output = f.niri_output(1);
    let scale = Scale::from(output.current_scale().fractional_scale());
    let width = to_physical_precise_round(scale.x, output_size(&output).w);
    let bar_h = to_physical_precise_round(scale.x, crate::ui::panel::PANEL_HEIGHT);

    let state = f.niri_state();
    let opaque = state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| {
            let elem = state
                .niri
                .panel
                .render(vk, &output)
                .expect("panel produced no element on Vulkan (still blank)");
            let pixels = render_to_vec(
                vk,
                Size::<i32, Physical>::from((width, bar_h)),
                scale,
                Transform::Normal,
                Fourcc::Abgr8888,
                [elem].into_iter(),
            )
            .expect("render panel");
            pixels.chunks_exact(4).filter(|p| p[3] == 255).count()
        })
        .expect("vulkan renderer");

    assert!(
        opaque > 0,
        "the panel bar did not composite any opaque pixels on Vulkan"
    );
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
    // The tile open animation renders the window through an offscreen, scaling and fading it in.
    // It used to be GLES-only — on the owned Vulkan renderer it degraded to a plain full-alpha
    // render (the window popped in at once). Now it runs on Vulkan via a `VkTexture` offscreen.
    // Compose an unsettled frame (mid-open, near progress 0 → faded toward invisible) and a settled
    // frame (full window): the open animation must make the early frame markedly dimmer than the
    // settled one. The old degraded path showed FULL alpha even unsettled, so this also proves the
    // offscreen animation path is taken (not a fallback plain render) — and that it doesn't panic.
    let Some(mut f) = window_fixture_settled(GREEN, false) else {
        return;
    };
    let output = f.niri_output(1);
    assert!(
        f.niri().layout.are_animations_ongoing(Some(&output)),
        "expected the window open animation to be active"
    );

    let is_green = |p: [u8; 4]| p[0] < 40 && p[1] > 200 && p[2] < 40 && p[3] > 200;
    let count_green = |pixels: &[u8], w: i32, h: i32| {
        (0..w * h)
            .filter(|i| is_green(px(pixels, w, i % w, i / w)))
            .count()
    };

    // Unsettled: the fade-in is near its start, so the window is barely there.
    let (early, w, h) = render_output_vulkan(&mut f, &output);
    let green_early = count_green(&early, w, h);

    // Settled: the window is fully opaque.
    f.niri_complete_animations();
    let (settled, _, _) = render_output_vulkan(&mut f, &output);
    let green_settled = assert_window_and_background(&settled, w, h);

    eprintln!(
        "vulkan_renders_a_window_mid_open_animation: green early={green_early} settled={green_settled}"
    );
    assert!(green_settled > 0, "the settled window is absent");
    assert!(
        green_early < green_settled / 2,
        "the open animation did not fade the window in on Vulkan (degraded to a plain render?): \
         early={green_early} settled={green_settled}"
    );
}

/// A `FramebufferEffectElement` (GNOME background blur / postprocess) reports
/// `is_framebuffer_effect() == true`, so the render loop (`render_helpers::render_elements`) calls
/// `capture_framebuffer()` on it before `draw()`. This drives the **real** owned-Vulkan
/// capture+blur path through that loop (not manual frame calls), proving the integration: the
/// mid-frame render-pass split, the cached `BackdropBlur`, and the postprocess composite all run
/// without panicking or erroring. (Smithay's default `capture_framebuffer` is `unimplemented!()` —
/// a panic — so before the Vulkan override this crashed the compositor.) The blur here is over the
/// cleared transparent backdrop, so nothing visible composites; the *visible* softening is asserted
/// by `vulkan_backdrop_blur_softens_a_hard_edge`.
#[test]
fn vulkan_framebuffer_effect_captures_and_blurs_through_the_render_loop() {
    let mut vk = match VulkanRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "skipping vulkan_framebuffer_effect_captures_and_blurs_through_the_render_loop: \
                 no Vulkan ({e})"
            );
            return;
        }
    };

    let fbe = crate::render_helpers::framebuffer_effect::FramebufferEffect::new();
    let params = crate::render_helpers::background_effect::RenderParams {
        geometry: Rectangle::from_size(Size::from((200., 200.))),
        subregion: None,
        clip: None,
        scale: 1.0,
    };
    // Blur on (`passes > 0`) so the effect is non-trivial; noise 0, saturation 1.
    let blur = crate::render_helpers::blur::BlurOptions {
        passes: 3,
        offset: 5.0,
    };
    let elem = fbe.render(None, params, Some(blur), 0.0, 1.0);

    // render_to_vec invokes capture_framebuffer() for is_framebuffer_effect elements, then draw().
    // Reaching a readback proves the whole owned-Vulkan capture+blur+postprocess path ran cleanly.
    let size = Size::<i32, Physical>::from((256, 256));
    let pixels = render_to_vec(
        &mut vk,
        size,
        Scale::from(1.0),
        Transform::Normal,
        Fourcc::Abgr8888,
        std::iter::once(elem),
    )
    .expect("compositing a framebuffer-effect element through Vulkan must not panic or error");
    assert_eq!(
        pixels.len(),
        (256 * 256 * 4) as usize,
        "unexpected readback size"
    );
    eprintln!(
        "vulkan_framebuffer_effect_captures_and_blurs_through_the_render_loop: {} bytes, no panic",
        pixels.len()
    );
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

/// Directly exercise the mid-frame render-pass split behind the backdrop-blur port
/// (`VulkanFrame::capture_region`): clear the target red, capture it, then overwrite with green on
/// the continuation pass. The **capture** must read back red (the scene as it was when captured)
/// and the **target** must read back green (the continuation pass LOAD-preserved the red, then the
/// second clear replaced it) — proving the pass ended, the blit grabbed the mid-frame contents, and
/// the LOAD-variant continuation pass resumed compositing correctly. Runs anywhere Vulkan is
/// present (offscreen only, no GBM), so it validates the split on lavapipe too.
#[test]
fn vulkan_capture_region_splits_the_render_pass() {
    use smithay::backend::renderer::Offscreen;

    let mut vk = match VulkanRenderer::new() {
        Ok(vk) => vk,
        Err(e) => {
            eprintln!("skipping vulkan_capture_region_splits_the_render_pass: no Vulkan ({e})");
            return;
        }
    };

    const S: i32 = 64;
    let size = Size::<i32, Physical>::from((S, S));
    let region = Rectangle::<i32, BufferCoord>::from_size(Size::from((S, S)));
    let phys_region = Rectangle::<i32, Physical>::from_size(Size::from((S, S)));
    let buf_size = Size::<i32, BufferCoord>::from((S, S));

    // The capture destination (a SAMPLED | TRANSFER_DST offscreen) and the render target.
    let mut dest = vk
        .create_buffer(Fourcc::Abgr8888, buf_size)
        .expect("create capture dest");
    let mut target = vk
        .create_buffer(Fourcc::Abgr8888, buf_size)
        .expect("create target");

    {
        let mut fb = vk.bind(&mut target).expect("bind target");
        {
            let mut frame = vk.render(&mut fb, size, Transform::Normal).expect("render");
            frame
                .clear(
                    Color32F::from([1., 0., 0., 1.]),
                    &[Rectangle::from_size(size)],
                )
                .expect("clear red");
            frame.capture_region(phys_region, &dest).expect("capture");
            // Draw (not clear) over the whole target so a real graphics pipeline binds *inside* the
            // continuation pass — this is what proves the continuation pass is render-pass
            // *compatible* with the pipelines (built against the base pass), which Phase 2's
            // postprocess draw relies on.
            frame
                .draw_solid(
                    phys_region,
                    &[Rectangle::from_size(size)],
                    Color32F::from([0., 1., 0., 1.]),
                )
                .expect("draw green");
            let _ = frame.finish().expect("finish");
        }

        // The target read back green: the continuation pass preserved the captured red on LOAD,
        // then the second clear overwrote it.
        let tmap = vk
            .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
            .expect("copy target");
        let tpx = vk.map_texture(&tmap).expect("map target").to_vec();
        let g = px(&tpx, S, S / 2, S / 2);
        assert!(
            g[0] < 40 && g[1] > 200 && g[2] < 40 && g[3] > 200,
            "target should be green after the continuation pass, got {g:?}"
        );
    }

    // The capture read back red: the pass ended and the blit grabbed the mid-frame contents.
    let dfb = vk.bind(&mut dest).expect("bind capture dest");
    let dmap = vk
        .copy_framebuffer(&dfb, region, Fourcc::Abgr8888)
        .expect("copy capture");
    let dpx = vk.map_texture(&dmap).expect("map capture").to_vec();
    let r = px(&dpx, S, S / 2, S / 2);
    assert!(
        r[0] > 200 && r[1] < 40 && r[2] < 40 && r[3] > 200,
        "capture should hold the pre-capture red, got {r:?}"
    );

    eprintln!("vulkan_capture_region_splits_the_render_pass: capture=red, target=green");
}

/// End-to-end backdrop blur on the owned Vulkan renderer: draw a hard red|green vertical edge, then
/// run a `FramebufferEffectElement` (blur enabled) over the whole frame — `capture_framebuffer`
/// grabs the scene (mid-frame render-pass split), blurs it, and `draw` composites the result. A
/// blurred edge produces **blended** pixels (both R and G raised) that the sharp scene never has,
/// so finding one proves the capture→blur→postprocess path ran. Offscreen-only, so it runs on
/// lavapipe too. This is the payoff of the mid-frame render-pass split.
#[test]
fn vulkan_backdrop_blur_softens_a_hard_edge() {
    use niri_config::CornerRadius;
    use smithay::backend::renderer::Offscreen;
    use smithay::utils::user_data::UserDataMap;

    use crate::render_helpers::background_effect::RenderParams;
    use crate::render_helpers::blur::BlurOptions;
    use crate::render_helpers::framebuffer_effect::FramebufferEffect;
    use crate::render_helpers::vulkan::VulkanRenderer as Vk;

    let mut vk = match Vk::new() {
        Ok(vk) => vk,
        Err(e) => {
            eprintln!("skipping vulkan_backdrop_blur_softens_a_hard_edge: no Vulkan ({e})");
            return;
        }
    };

    const S: i32 = 64;
    let size = Size::<i32, Physical>::from((S, S));
    let mut target = vk
        .create_buffer(Fourcc::Abgr8888, Size::from((S, S)))
        .expect("create target");

    // A whole-output framebuffer effect with blur on (no clip, no rounding, no desaturation).
    let effect = FramebufferEffect::new();
    let params = RenderParams {
        geometry: Rectangle::from_size(Size::from((S as f64, S as f64))),
        subregion: None,
        clip: None,
        scale: 1.0,
    };
    let element = effect.render(
        None,
        params,
        Some(BlurOptions {
            passes: 3,
            offset: 2.0,
        }),
        0.0, // noise
        1.0, // saturation (identity)
    );
    let _ = CornerRadius::default();
    let cache = UserDataMap::new();
    let src = element.src();
    let dst = element.geometry(Scale::from(1.0));

    {
        let mut fb = vk.bind(&mut target).expect("bind");
        let mut frame = vk.render(&mut fb, size, Transform::Normal).expect("render");
        // Scene behind: left half red, right half green — a hard vertical edge at x = S/2.
        frame
            .draw_solid(
                Rectangle::new((0, 0).into(), (S / 2, S).into()),
                &[Rectangle::from_size((S / 2, S).into())],
                Color32F::from([1., 0., 0., 1.]),
            )
            .expect("draw red");
        frame
            .draw_solid(
                Rectangle::new((S / 2, 0).into(), (S / 2, S).into()),
                &[Rectangle::from_size((S / 2, S).into())],
                Color32F::from([0., 1., 0., 1.]),
            )
            .expect("draw green");
        // Capture the scene (render-pass split), blur it, composite it back.
        RenderElement::<Vk>::capture_framebuffer(&element, &mut frame, src, dst, &cache)
            .expect("capture_framebuffer");
        RenderElement::<Vk>::draw(
            &element,
            &mut frame,
            src,
            dst,
            &[Rectangle::from_size(size)],
            &[],
            Some(&cache),
        )
        .expect("draw");
        let _ = frame.finish().expect("finish");
    }

    let fb = vk.bind(&mut target).expect("rebind");
    let region = Rectangle::<i32, BufferCoord>::from_size(Size::from((S, S)));
    let mapping = vk
        .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
        .expect("copy_framebuffer");
    let pixels = vk.map_texture(&mapping).expect("map").to_vec();

    // Scan the middle row for a blended pixel (both channels raised) — impossible in the sharp
    // red|green scene, so its presence proves the blur ran.
    let y = S / 2;
    let mut best_blend = 0u8;
    for x in 0..S {
        let p = px(&pixels, S, x, y);
        best_blend = best_blend.max(p[0].min(p[1]));
    }
    assert!(
        best_blend > 40,
        "no blended pixel on the middle row (max min(R,G) = {best_blend}); blur did not composite"
    );

    // The composite is still the (blurred) scene, not garbage: far left stays red-dominant, far
    // right green-dominant.
    let left = px(&pixels, S, 2, y);
    let right = px(&pixels, S, S - 3, y);
    assert!(
        left[0] > 150 && left[1] < 110,
        "far-left should stay red-dominant, got {left:?}"
    );
    assert!(
        right[1] > 150 && right[0] < 110,
        "far-right should stay green-dominant, got {right:?}"
    );

    eprintln!(
        "vulkan_backdrop_blur_softens_a_hard_edge: edge blend min(R,G)={best_blend}, left={left:?} right={right:?}"
    );
}

/// A blur-off, saturation-1, noise-0, unclipped framebuffer effect over the *whole* output is a
/// no-op: it captures the backdrop and redraws it unchanged. That invariant must hold under **any**
/// output transform — the capture blit grabs the scene in physical orientation, so the postprocess
/// draw has to sample it back with the matching orientation (else a rotated output redraws the
/// backdrop rotated/stretched). Render a hard red|green scene with and without the effect at a
/// rotation and a transposing flip, and assert the effect leaves the corners unchanged. Guards the
/// postprocess sampling-vs-geometry split under rotation.
#[test]
fn vulkan_backdrop_effect_roundtrips_under_rotation() {
    use smithay::backend::renderer::Offscreen;
    use smithay::utils::user_data::UserDataMap;

    use crate::render_helpers::background_effect::RenderParams;
    use crate::render_helpers::framebuffer_effect::FramebufferEffect;
    use crate::render_helpers::vulkan::VulkanRenderer as Vk;

    let mut vk = match Vk::new() {
        Ok(vk) => vk,
        Err(e) => {
            eprintln!("skipping vulkan_backdrop_effect_roundtrips_under_rotation: no Vulkan ({e})");
            return;
        }
    };

    const S: i32 = 64;
    let size = Size::<i32, Physical>::from((S, S));

    // Render the red|green scene at `transform`, optionally with a whole-output blur-off effect on
    // top, and read it back.
    let render_scene = |vk: &mut Vk, transform: Transform, with_effect: bool| -> Vec<u8> {
        let mut target = vk
            .create_buffer(Fourcc::Abgr8888, Size::from((S, S)))
            .expect("create target");
        {
            let mut fb = vk.bind(&mut target).expect("bind");
            let mut frame = vk.render(&mut fb, size, transform).expect("render");
            // Logical left half red, right half green (a hard vertical edge in logical space).
            frame
                .draw_solid(
                    Rectangle::new((0, 0).into(), (S / 2, S).into()),
                    &[Rectangle::from_size((S / 2, S).into())],
                    Color32F::from([1., 0., 0., 1.]),
                )
                .expect("draw red");
            frame
                .draw_solid(
                    Rectangle::new((S / 2, 0).into(), (S / 2, S).into()),
                    &[Rectangle::from_size((S / 2, S).into())],
                    Color32F::from([0., 1., 0., 1.]),
                )
                .expect("draw green");

            if with_effect {
                let effect = FramebufferEffect::new();
                let params = RenderParams {
                    geometry: Rectangle::from_size(Size::from((S as f64, S as f64))),
                    subregion: None,
                    clip: None,
                    scale: 1.0,
                };
                // Blur OFF, noise 0, saturation 1 → the effect should reproduce the backdrop.
                let element = effect.render(None, params, None, 0.0, 1.0);
                let cache = UserDataMap::new();
                let src = element.src();
                let dst = element.geometry(Scale::from(1.0));
                RenderElement::<Vk>::capture_framebuffer(&element, &mut frame, src, dst, &cache)
                    .expect("capture_framebuffer");
                RenderElement::<Vk>::draw(
                    &element,
                    &mut frame,
                    src,
                    dst,
                    &[Rectangle::from_size(size)],
                    &[],
                    Some(&cache),
                )
                .expect("draw");
            }
            let _ = frame.finish().expect("finish");
        }
        let fb = vk.bind(&mut target).expect("rebind");
        let region = Rectangle::<i32, BufferCoord>::from_size(Size::from((S, S)));
        let mapping = vk
            .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
            .expect("copy_framebuffer");
        vk.map_texture(&mapping).expect("map").to_vec()
    };

    // Sample the four corners (inset a few px to dodge the LINEAR-sampled edge softening).
    let corners = |pixels: &[u8]| -> [[u8; 4]; 4] {
        let m = 4;
        [
            px(pixels, S, m, m),
            px(pixels, S, S - 1 - m, m),
            px(pixels, S, m, S - 1 - m),
            px(pixels, S, S - 1 - m, S - 1 - m),
        ]
    };
    let close = |a: [u8; 4], b: [u8; 4]| (0..4).all(|i| (a[i] as i32 - b[i] as i32).abs() <= 24);

    // _90 rotates the vertical edge to horizontal; Flipped90 is a transposing flip (the anti-
    // diagonal case a plain rotation wouldn't catch). Normal is the trivial baseline.
    for t in [Transform::Normal, Transform::_90, Transform::Flipped90] {
        let plain = render_scene(&mut vk, t, false);
        let effected = render_scene(&mut vk, t, true);
        let (pc, ec) = (corners(&plain), corners(&effected));
        for (i, (p, e)) in pc.iter().zip(ec.iter()).enumerate() {
            assert!(
                close(*p, *e),
                "{t:?}: corner {i} changed by the no-op effect: plain={p:?} effected={e:?} \
                 (backdrop sampled with the wrong orientation)"
            );
        }
        eprintln!(
            "vulkan_backdrop_effect_roundtrips_under_rotation {t:?}: corners {pc:?} preserved"
        );
    }
}

/// Composite `output` through the fixture's GLES oracle renderer (the `with_primary_renderer`
/// path), returning the tight `Abgr8888` readback and its dimensions. The GLES sibling of
/// [`render_output_vulkan`], for oracle comparisons of the full `Niri::render_to_vec` scene.
fn render_output_gles(f: &mut Fixture, output: &Output) -> (Vec<u8>, i32, i32) {
    let state = f.niri_state();
    state
        .backend
        .headless()
        .with_primary_renderer(|g| -> anyhow::Result<(Vec<u8>, i32, i32)> {
            let niri = &mut state.niri;
            niri.update_render_elements(Some(output));

            let size: Size<i32, Physical> = output.current_mode().unwrap().size;
            let size = output.current_transform().transform_size(size);
            let scale = Scale::from(output.current_scale().fractional_scale());

            let ctx = RenderCtx {
                renderer: g,
                target: RenderTarget::ScreenCapture,
                xray: None,
            };
            let elements = niri.render_to_vec(ctx, output, false);
            let elements = elements.iter().rev();
            let pixels = render_to_vec(
                g,
                size,
                scale,
                Transform::Normal,
                Fourcc::Abgr8888,
                elements,
            )?;
            Ok((pixels, size.w, size.h))
        })
        .expect("headless backend must hold a GLES renderer")
        .expect("compositing through GLES must not error")
}

/// A green window with a corner radius + `clip-to-geometry`, so the tile clips it to a rounded
/// rectangle — exercising [`ClippedSurfaceRenderElement`]. Mirrors [`window_fixture`] but installs
/// a matches-all window rule. `None` (skip) when there is no Vulkan device.
fn clipped_window_fixture() -> Option<Fixture> {
    if let Err(e) = VulkanRenderer::new() {
        eprintln!("skipping: no Vulkan device ({e})");
        return None;
    }

    let mut config = Config::default();
    // Strip the decorations (focus ring, border, shadow) so the only things on screen are the
    // clipped window and the backdrop — otherwise the focus ring's accent color bleeds into the
    // corners we sample and confounds the clip check.
    config.layout.focus_ring.off = true;
    config.layout.border.off = true;
    config.window_rules.push(WindowRule {
        geometry_corner_radius: Some(CornerRadius::from(CLIP_RADIUS)),
        clip_to_geometry: Some(true),
        ..Default::default()
    });

    let mut f = Fixture::with_config_and_renderer(config, RendererKind::Vulkan);
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

    // A real shm-textured window (not a single-pixel solid buffer): a solid buffer renders as a
    // SolidColorRenderElement (the SolidColor arm, never clipped), while a textured buffer renders
    // as a WaylandSurfaceRenderElement — the arm the clip closure actually rounds.
    let window = f.client(id).window(&surface);
    window.attach_shm_buffer(WIN as i32, WIN as i32, 0, 255, 0, 255);
    window.set_size(WIN, WIN);
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    f.niri_complete_animations();
    f.double_roundtrip(id);

    Some(f)
}

const CLIP_RADIUS: f32 = 30.;

/// The clip material must both **sample** the window (rounded corners are not blank) and **round**
/// it (the geometry corners are cut away), matching the GLES oracle. This drives the full
/// `Niri::render_to_vec` scene — a green window with a corner radius + `clip-to-geometry` — through
/// both renderers and asserts, without pixel-exact AA comparison, that: the window center is green
/// (sampled), the corners of the green region are **not** green (rounded away to the backdrop), and
/// the mid-edges **are** green (only the corners were cut — it is rounding, not a full clip).
#[test]
fn vulkan_clips_a_window_to_rounded_geometry() {
    let Some(mut f) = clipped_window_fixture() else {
        return;
    };
    let output = f.niri_output(1);

    let is_green = |p: [u8; 4]| p[0] < 40 && p[1] > 200 && p[2] < 40 && p[3] > 200;

    // The tight bounding box of the green (window) pixels.
    let green_bbox = |pixels: &[u8], w: i32, h: i32| -> (i32, i32, i32, i32) {
        let (mut x0, mut y0, mut x1, mut y1) = (w, h, -1, -1);
        for y in 0..h {
            for x in 0..w {
                if is_green(px(pixels, w, x, y)) {
                    x0 = x0.min(x);
                    y0 = y0.min(y);
                    x1 = x1.max(x);
                    y1 = y1.max(y);
                }
            }
        }
        assert!(x1 >= 0, "the mapped green window is absent from the frame");
        (x0, y0, x1, y1)
    };

    let check = |name: &str, pixels: &[u8], w: i32, h: i32| {
        let (x0, y0, x1, y1) = green_bbox(pixels, w, h);
        let (cx, cy) = ((x0 + x1) / 2, (y0 + y1) / 2);

        // Center is green — the clip pipeline sampled the window (not blanked).
        assert!(
            is_green(px(pixels, w, cx, cy)),
            "{name}: window center ({cx},{cy}) is not green — the clip blanked the surface"
        );

        // The four extreme corners of the green box are rounded away (a corner radius of 30 cuts
        // the corner pixel well outside the quarter-circle), so they show the backdrop, not
        // the window.
        for (x, y) in [(x0, y0), (x1, y0), (x1, y1), (x0, y1)] {
            assert!(
                !is_green(px(pixels, w, x, y)),
                "{name}: geometry corner ({x},{y}) is still green — not clipped/rounded (square \
                 corners)"
            );
        }

        // The mid-edges reach the box extremes — only the corners were cut, proving rounding rather
        // than a wholesale clip. (Sample a few px inside the edge to dodge AA at the very border.)
        assert!(
            is_green(px(pixels, w, cx, y0 + 2)) && is_green(px(pixels, w, x0 + 2, cy)),
            "{name}: mid-edges are not green — the window was over-clipped"
        );
        (x0, y0, x1, y1)
    };

    let (vk, vw, vh) = render_output_vulkan(&mut f, &output);
    let (gl, gw, gh) = render_output_gles(&mut f, &output);
    assert_eq!((vw, vh), (gw, gh), "readback dimensions differ");

    // Print both before asserting, so a failure shows the oracle side too.
    let gbox = check("gles", &gl, gw, gh);
    let vbox = check("vulkan", &vk, vw, vh);

    // Oracle agreement: the clipped window lands at the same place and size on both renderers (a
    // few px of AA slack on the rounded edges).
    for (v, g) in [
        (vbox.0, gbox.0),
        (vbox.1, gbox.1),
        (vbox.2, gbox.2),
        (vbox.3, gbox.3),
    ] {
        assert!(
            (v - g).abs() <= 2,
            "clipped window bbox disagrees with the GLES oracle: vulkan={vbox:?} gles={gbox:?}"
        );
    }
    eprintln!("vulkan_clips_a_window_to_rounded_geometry: bbox vulkan={vbox:?} gles={gbox:?}");
}

/// A clipped window must render correctly through an outer placement wrapper. The overview draws
/// every window through a `RelocateRenderElement<RescaleRenderElement<…>>`, which forwards a
/// rescaled/relocated `dst` (smithay `element/utils/elements.rs`) to the inner clipped surface. The
/// clip's `input_to_geo` is built from the element's **creation-space** geometry, not that `dst`,
/// so the zoomed-out window still samples and rounds correctly. This exercises exactly that
/// wrapped- clip path on Vulkan: open the overview and assert the clipped window renders at its
/// zoomed-out overview slot (green content present, and smaller than the full-size desktop window —
/// i.e. it is the wrapped render, not the desktop showing through).
#[test]
fn vulkan_clips_a_window_through_the_overview_wrapper() {
    let Some(mut f) = clipped_window_fixture() else {
        return;
    };
    let output = f.niri_output(1);

    f.niri_state().do_action(Action::OpenOverview, false);
    assert!(
        f.niri().layout.is_overview_open(),
        "the overview must be open"
    );
    f.niri_complete_animations();

    let (pixels, w, h) = render_output_vulkan_target(&mut f, &output, RenderTarget::Output);

    // The overview dims windows to ~[40,65,40], so match green-dominant (not bright-green): any
    // pixel where green clearly leads red/blue is the (dimmed) window content vs the gray backdrop.
    let is_green =
        |p: [u8; 4]| p[1] as i32 > p[0] as i32 + 15 && p[1] as i32 > p[2] as i32 + 15 && p[1] > 45;
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, -1, -1);
    let green = (0..w * h)
        .filter(|i| is_green(px(&pixels, w, i % w, i / w)))
        .inspect(|i| {
            let (x, y) = (i % w, i / w);
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        })
        .count();
    eprintln!(
        "vulkan_clips_a_window_through_the_overview_wrapper: {green} green px bbox=({x0},{y0})..({x1},{y1})"
    );
    // Present (the wrapped clip sampled the window, not blanked)...
    assert!(
        green > 200,
        "the clipped window did not render through the overview's rescale/relocate wrapper: \
         {green} green px"
    );
    // ...and zoomed out (smaller than the WIN×WIN desktop window), proving it is the wrapped
    // overview render rather than the unwrapped desktop window showing through.
    assert!(
        (x1 - x0) < WIN as i32 && (y1 - y0) < WIN as i32,
        "the green region ({}×{}) is not zoomed out — not the overview's wrapped render",
        x1 - x0,
        y1 - y0,
    );
}

/// The focus ring / border outline must be rounded on Vulkan, not pointy. It is drawn by
/// `BorderRenderElement` (a procedural rounded/gradient SDF) when the renderer can render it, else
/// it falls back to plain `SolidColorRenderElement` quads (square corners). That choice hinges on
/// `BorderRenderElement::has_shader`, which was GLES-only (`Shaders::get`) — so a Vulkan session
/// got pointy quads even though the owned renderer draws borders procedurally (rounded). Now
/// `has_shader` is true on Vulkan too. Map a focused window with a thick blue focus ring + corner
/// radius and assert the ring is present but its extreme outer corner is rounded away to the
/// backdrop.
#[test]
fn vulkan_draws_a_rounded_focus_ring() {
    if VulkanRenderer::new().is_err() {
        eprintln!("skipping: no Vulkan device");
        return;
    }

    let mut config = Config::default();
    config.layout.border.off = true; // isolate the focus ring
    config.layout.focus_ring.off = false;
    config.layout.focus_ring.width = 12.;
    // active_color defaults to [127,200,255]; make the ring's presence unambiguous.
    config.window_rules.push(WindowRule {
        geometry_corner_radius: Some(CornerRadius::from(20.)),
        clip_to_geometry: Some(true),
        ..Default::default()
    });

    let mut f = Fixture::with_config_and_renderer(config, RendererKind::Vulkan);
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
    window.attach_shm_buffer(WIN as i32, WIN as i32, 0, 255, 0, 255);
    window.set_size(WIN, WIN);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.niri_complete_animations();
    f.double_roundtrip(id);

    let output = f.niri_output(1);
    let (pixels, w, h) = render_output_vulkan_target(&mut f, &output, RenderTarget::Output);

    // The active focus ring: blue-dominant, high blue (~[127,200,255]); distinct from the green
    // window and gray backdrop.
    let is_blue = |p: [u8; 4]| p[2] > 200 && p[0] > 80 && p[0] < 180 && p[1] as i32 > p[0] as i32;
    let is_green = |p: [u8; 4]| p[0] < 40 && p[1] > 200 && p[2] < 40 && p[3] > 200;

    // Anchor on the (dense, unambiguous) green window rather than the blue bbox — stray blue AA
    // elsewhere would inflate a whole-frame blue bbox.
    let (mut gx0, mut gy0, mut gx1, mut gy1) = (w, h, -1, -1);
    for y in 0..h {
        for x in 0..w {
            if is_green(px(&pixels, w, x, y)) {
                gx0 = gx0.min(x);
                gy0 = gy0.min(y);
                gx1 = gx1.max(x);
                gy1 = gy1.max(y);
            }
        }
    }
    assert!(gx1 >= 0, "the green window is absent from the frame");
    eprintln!("vulkan_draws_a_rounded_focus_ring: window bbox=({gx0},{gy0})..({gx1},{gy1})");

    let my = (gy0 + gy1) / 2;
    // The ring drew (present just outside the window's left edge, within the 12px band).
    assert!(
        is_blue(px(&pixels, w, gx0 - 6, my)),
        "the focus ring did not draw on Vulkan (no ring at the window's left edge)"
    );
    // Rounded, not pointy: a point diagonally outside the window's rounded top-left corner is
    // outside a *rounded* ring's arc (→ backdrop), but inside a *pointy* square ring (→ ring
    // color).
    assert!(
        !is_blue(px(&pixels, w, gx0 - 10, gy0 - 10)),
        "the focus-ring corner is ring-colored — pointy (square), not rounded"
    );
}
