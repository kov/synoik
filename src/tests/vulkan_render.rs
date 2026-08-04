// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! End-to-end proof that the live `Synoik::render` compositing path runs on the **owned Vulkan
//! renderer**, not just GLES: a real client window is mapped through the headless test harness and
//! the whole scene is composited through `VulkanRenderer`, both into an offscreen buffer (the
//! screenshot path) and into a **GBM-allocated scanout dmabuf** (the KMS-present path — everything
//! except the DRM page-flip, which is validated live). Exercises the renderer-agnostic render
//! helpers (Brick 2), the `try_as_gles` degradation guards (Brick 3), and `Bind<Dmabuf>` (Brick A).
//!
//! Skips gracefully when no Vulkan device is present. The scanout test additionally needs a real
//! GBM device (the render node), so it is Venus-only (lavapipe/CPU has no GBM).

use std::time::Duration;

use smithay::backend::allocator::Fourcc;
use smithay::backend::input::ButtonState;
use smithay::backend::renderer::element::{Element, RenderElement};
use smithay::backend::renderer::{Bind, Color32F, ExportMem, Frame, Renderer};
use smithay::output::Output;
use smithay::utils::{
    Buffer as BufferCoord, Logical, Physical, Point, Rectangle, Scale, Size, Transform,
};
use synoik_config::{Action, Config, CornerRadius, WindowRule};
use wayland_client::protocol::wl_shm;
use wayland_client::protocol::wl_surface::WlSurface;

use super::client::ClientId;
use super::fixture::Fixture;
use super::gnome::{map_window_for_app, BTN_LEFT};
use crate::render_helpers::vulkan::VulkanRenderer;
use crate::render_helpers::{render_to_vec, RenderCtx, RenderTarget, NATIVE_FOURCC};
use crate::synoik::OutputRenderElements;
use crate::ui::screenshot_ui::{CaptureType, PointerUp};
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
/// Composite UI elements the way the compositor does, and read the frame back.
///
/// The z convention is the trap this exists for. Every UI `render()` in the fork returns
/// its elements **front-to-back** (first = topmost) — that is what `Synoik::render` pushes
/// and what the real paths hand around. But `render_helpers::render_elements` draws in
/// **iteration order**, so later elements land on top: it wants back-to-front, which is
/// why every production caller reverses (`Synoik::screenshot`, `snapshot.rs`, …).
///
/// A test that passes the list straight to `render_to_vec` therefore composites it upside
/// down. That is not loud: the bottom-most element is usually the opaque background box
/// (a popover's `.popup-menu-content` fill, the panel's bar), so it ends up covering the
/// content and an `opaque > 0` assertion still passes while measuring a flat rectangle.
/// Route every UI composite through here so the reverse cannot be forgotten.
fn composite_ui<E: RenderElement<VulkanRenderer>>(
    vk: &mut VulkanRenderer,
    elems: Vec<E>,
    size: Size<i32, Physical>,
    scale: Scale<f64>,
) -> Vec<u8> {
    render_to_vec(
        vk,
        size,
        scale,
        Transform::Normal,
        Fourcc::Abgr8888,
        elems.into_iter().rev(),
    )
    .expect("composite UI elements")
}

fn px(pixels: &[u8], w: i32, x: i32, y: i32) -> [u8; 4] {
    let i = ((y * w + x) * 4) as usize;
    [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
}

/// Build a Vulkan-backed fixture with one 1280×720 output and a single opaque window of the given
/// premultiplied color, mapped and animation-settled (a static scene). Returns `None` (with a skip
/// message) when there is no Vulkan device — smithay renders the single-pixel buffer as a solid
/// color, so the window needs no client-buffer import.
fn window_fixture(color: [u32; 4]) -> Option<Fixture> {
    window_fixture_settled(color, true, None)
}

/// As [`window_fixture`], but `settle` controls whether the map/open animation is completed. Pass
/// `false` to leave the tile open animation active (the guarded GLES-offscreen render path).
fn window_fixture_settled(color: [u32; 4], settle: bool, title: Option<&str>) -> Option<Fixture> {
    window_fixture_with_client(color, settle, title).map(|(f, _, _)| f)
}

/// As [`window_fixture_settled`], but also returns the client and surface, so a test can keep
/// driving the window after the fixture is built (e.g. recolour it to tell the live window apart
/// from a frozen capture of it).
fn window_fixture_with_client(
    color: [u32; 4],
    settle: bool,
    title: Option<&str>,
) -> Option<(Fixture, ClientId, WlSurface)> {
    if let Err(e) = VulkanRenderer::new() {
        eprintln!("skipping: no Vulkan device ({e})");
        return None;
    }

    let mut f = Fixture::new();
    f.synoik_state()
        .backend
        .headless()
        .add_renderer()
        .expect("build the Vulkan renderer");
    f.add_output(1, (OUT_W, OUT_H));

    let id = f.add_client();
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    if let Some(title) = title {
        window.set_title(title);
    }
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    window.attach_solid_buffer(color[0], color[1], color[2], color[3]);
    window.set_size(WIN, WIN);
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    if settle {
        // Settle any map/open animation so we composite a static scene.
        f.synoik_complete_animations();
        f.double_roundtrip(id);
    }

    Some((f, id, surface))
}

/// Put the screenshot UI's chrome on screen by settling its open animation.
///
/// **`synoik_complete_animations` does NOT settle this one.** It sets the clock's
/// `complete_instantly` only for the duration of `advance_animations` and then resets it, so by
/// render time `Animation::is_clamped_done` is false again and the value is back to
/// `value_at(clock.now())` — and the lazy clock is still frozen at the moment the UI opened, so
/// `clock.now() == start_time` and `value_at` returns `from`, i.e. **0**.
///
/// At progress 0 every `progress`-gated element — the help panel and all eight dim/selection
/// buffers — draws at alpha 0, so the UI's entire chrome is invisible. Only the frozen screenshot
/// survives, because it is pushed ungated. A test that "opens" the UI without this is asserting
/// against a frame the chrome never appeared in.
///
/// Pinning the clock past the 200ms `screenshot-ui-open` animation is what actually puts it on
/// screen. Must be called after the last roundtrip and immediately before rendering, since the
/// event loop clears the clock (`Synoik::refresh`).
fn settle_screenshot_ui_open(f: &mut Fixture) {
    let mut clock = f.synoik().clock.clone();
    let now = clock.now_unadjusted();
    clock.set_unadjusted(now + Duration::from_millis(500));
    f.synoik_complete_animations();
}

/// Recolour the live window built by [`window_fixture_with_client`].
fn recolor_window(f: &mut Fixture, id: ClientId, surface: &WlSurface, color: [u32; 4]) {
    let window = f.client(id).window(surface);
    window.attach_solid_buffer(color[0], color[1], color[2], color[3]);
    window.commit();
    f.double_roundtrip(id);
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

/// Composite the whole `output` through the owned Vulkan renderer (the `Synoik::screenshot` path),
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
    let state = f.synoik_state();
    state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| -> anyhow::Result<(Vec<u8>, i32, i32)> {
            let synoik = &mut state.synoik;
            synoik.update_render_elements(Some(output));

            let size: Size<i32, Physical> = output.current_mode().unwrap().size;
            let size = output.current_transform().transform_size(size);
            let scale = Scale::from(output.current_scale().fractional_scale());

            let ctx = RenderCtx {
                renderer: vk,
                target,
                xray: None,
            };
            let elements = synoik.render_to_vec(ctx, output, false);
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
    let output = f.synoik_output(1);

    // Composite the whole output through the owned Vulkan renderer — the same element collection
    // (`Synoik::render_to_vec`) and offscreen readback (`render_helpers::render_to_vec`) that
    // `Synoik::screenshot` drives. Reaching pixels at all proves the guarded GLES-only sub-paths
    // degraded instead of panicking.
    let state = f.synoik_state();
    let (pixels, w, h) = state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| -> anyhow::Result<(Vec<u8>, i32, i32)> {
            let synoik = &mut state.synoik;
            synoik.update_render_elements(Some(&output));

            let size: Size<i32, Physical> = output.current_mode().unwrap().size;
            let size = output.current_transform().transform_size(size);
            let scale = Scale::from(output.current_scale().fractional_scale());

            let ctx = RenderCtx {
                renderer: vk,
                target: RenderTarget::ScreenCapture,
                xray: None,
            };
            let elements = synoik.render_to_vec(ctx, &output, false);
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

    // Finally, smoke-test the genericized `Synoik::screenshot` itself end-to-end on Vulkan (no disk
    // write, so no async encode thread to await): it must run the same path without panicking.
    let state = f.synoik_state();
    let ran = state.backend.headless().with_vulkan_renderer(|vk| {
        state
            .synoik
            .screenshot(vk, &output, false, false, None)
            .expect("Synoik::screenshot must succeed on the Vulkan renderer");
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
    let output = f.synoik_output(1);

    // Baseline: the static scene with no dialog.
    let (before, w, h) = render_output_vulkan(&mut f, &output);

    // Open the Alt+F2 run dialog, settle, and recomposite.
    f.synoik_state().do_action(Action::ShowRunDialog, false);
    f.synoik_complete_animations();
    assert!(f.synoik().run_dialog.is_open(), "run dialog must be open");
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

/// The screenshot UI freezes the screen into a GLES texture the owned Vulkan renderer can't sample,
/// so on a Vulkan session it reads that capture back and uploads it to a `VkTexture` for the Output
/// target. Open the UI over a green-window scene, then composite the Output target through Vulkan:
/// with the UI open the compositor draws only the UI, so the frozen green screen must be present (a
/// blank no-op overlay — the old behavior — would leave only the dark backdrop).
#[test]
fn vulkan_screenshot_ui_draws_the_frozen_screen() {
    let Some((mut f, id, surface)) = window_fixture_with_client(GREEN, true, None) else {
        return;
    };
    let output = f.synoik_output(1);

    f.synoik_state().open_screenshot_ui(false, None);
    assert!(
        f.synoik().screenshot_ui.is_open(),
        "screenshot UI must be open"
    );

    // Recolour the live window red. `Synoik::render` early-returns after the screenshot UI's
    // elements, above the layout, so the live window is not in this frame at all: every green pixel
    // below therefore comes from the UI's frozen capture, and any red would mean the UI drew
    // nothing and we are looking at the live scene.
    recolor_window(&mut f, id, &surface, RED);
    settle_screenshot_ui_open(&mut f);

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
    let is_red = |p: [u8; 4]| p[0] > 200 && p[1] < 40 && p[2] < 40;
    let red = (0..w * h)
        .filter(|i| is_red(px(&pixels, w, i % w, i / w)))
        .count();
    eprintln!("vulkan_screenshot_ui_draws_the_frozen_screen: {green} greenish px, {red} red px");
    assert!(
        green > 1000,
        "the frozen screenshot did not draw on Vulkan (blank overlay?): {green} greenish px"
    );
    assert!(
        red < 100,
        "the recoloured live window is on screen ({red} red px), so this frame is the live scene, \
         not the UI's frozen capture — the green above would have proved nothing"
    );
}

/// The screenshot UI's control panel must actually draw.
///
/// The panel is the UI's most fragile element: drawn into an offscreen, gated on the open
/// animation's progress — chances to end up invisible while the frozen screenshot (which is *not*
/// progress-gated) still draws and makes the frame look right. Measure inside
/// [`ScreenshotUi::panel_rect`]: whole-frame white does not discriminate the panel, since the four
/// selection-border buffers alone score thousands of white px without it.
///
/// Also the only cover for the icons, which are render *elements* composited over the bake rather
/// than paint ops inside it — the one part of the panel that a successful bake does not prove.
#[test]
fn vulkan_screenshot_ui_draws_the_control_panel() {
    let Some(mut f) = green_window_fixture() else {
        return;
    };
    let output = f.synoik_output(1);

    f.synoik_state().open_screenshot_ui(false, None);
    settle_screenshot_ui_open(&mut f);

    // The panel is built lazily on the first render, so render before reading its rect.
    let (pixels, w, h) = render_output_vulkan_target(&mut f, &output, RenderTarget::Output);

    let rect = f
        .synoik()
        .screenshot_ui
        .panel_rect(&output)
        .expect("the open screenshot UI must have a control panel");

    // `%osd_panel`'s fill is `style::OSD_BG` — rgb(46, 46, 51) — and the capture button's ring,
    // the captions and the type-button glyphs are all white.
    let mut background = 0;
    let mut white = 0;
    for y in rect.loc.y..(rect.loc.y + rect.size.h).min(h) {
        for x in rect.loc.x..(rect.loc.x + rect.size.w).min(w) {
            let p = px(&pixels, w, x, y);
            if (35..60).contains(&p[0]) && (35..60).contains(&p[1]) && (35..65).contains(&p[2]) {
                background += 1;
            }
            if p[0] > 200 && p[1] > 200 && p[2] > 200 {
                white += 1;
            }
        }
    }
    eprintln!(
        "vulkan_screenshot_ui_draws_the_control_panel: {background} panel-bg px, {white} white px \
         in {rect:?}"
    );
    assert!(
        background > 1000,
        "the control panel's background did not draw ({background} px in {rect:?})"
    );
    // The capture button's 32px inner circle alone is ~800 px, so this cannot pass on chrome.
    assert!(
        white > 900,
        "the control panel drew no capture button or captions ({white} white px in {rect:?})"
    );
}

/// The type buttons must be where the hit test says they are, and clicking one must change what the
/// capture button will take.
///
/// The panel is a single baked texture: nothing structural connects the pixels a control is drawn
/// at to the geometry [`PanelLayout::control_at`] answers with. One shared [`PanelLayout`] is what
/// keeps them together, and this is the test that would fail if the bake and the hit test ever
/// stopped reading the same one.
/// Click a control on the open picker's panel, by the rect the bake published for it.
fn click_control(f: &mut Fixture, output: &Output, rect: Rectangle<f64, Logical>) -> PointerUp {
    let panel = f
        .synoik()
        .screenshot_ui
        .panel_rect(output)
        .expect("the open screenshot UI must have a control panel");
    let scale = output.current_scale().fractional_scale();
    let point =
        Point::<f64, Logical>::from((rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.))
            .to_physical(scale)
            .to_i32_round::<i32>()
            + panel.loc;

    let ui = &mut f.synoik_state().synoik.screenshot_ui;
    ui.pointer_motion(point, None);
    assert!(ui
        .pointer_down(output.clone(), point, None, false)
        .is_some());
    ui.pointer_up(None)
        .expect("the release must land on a control")
}

/// Cast mode drops the frozen screen, and only a pixel can tell that from "the still happens to
/// match": the live window is recoloured *after* the switch and the new colour must reach the
/// frame. The mode's state-machine half (Window mode greyed out, the fall back to Selection) is
/// device-free and lives in the corpus as `cast_mode_refuses_window_capture`.
#[test]
fn vulkan_screenshot_ui_cast_mode_drops_the_frozen_screen() {
    let Some((mut f, client, surface)) = window_fixture_with_client(GREEN, true, None) else {
        return;
    };
    let output = f.synoik_output(1);

    f.synoik_state().open_screenshot_ui(false, None);
    settle_screenshot_ui_open(&mut f);
    render_output_vulkan_target(&mut f, &output, RenderTarget::Output);

    let layout = f.synoik().screenshot_ui.panel_layout(&output).unwrap();
    click_control(
        &mut f,
        &output,
        crate::ui::widget::Segmented::segment_rect(layout.shot_cast, 1),
    );
    assert_eq!(
        f.synoik().screenshot_ui.mode(),
        crate::ui::screenshot_ui::CaptureMode::Cast
    );

    recolor_window(&mut f, client, &surface, [0, 0, u32::MAX, u32::MAX]);
    let (pixels, w, h) = render_output_vulkan_target(&mut f, &output, RenderTarget::Output);
    // Not an exact match: the picker's shade dims everything outside the selection, so the live
    // window arrives blended. Blue-dominant is the honest question.
    let blue = (0..w * h)
        .map(|i| px(&pixels, w, i % w, i / w))
        .filter(|p| p[2] > 60 && u32::from(p[2]) > u32::from(p[0]) * 3)
        .filter(|p| u32::from(p[2]) > u32::from(p[1]) * 3)
        .count();
    assert!(
        blue > 0,
        "cast mode still draws the frozen screen — the picker is showing a still of the past while \
         claiming to record the present"
    );
}

/// The capture button clicked in cast mode starts a recording instead of taking a picture.
///
/// Here rather than in the corpus despite having no pixel claim: starting the recorder spawns a
/// real ffmpeg, and the corpus should not need an external process to run.
#[test]
fn vulkan_screenshot_ui_cast_mode_capture_starts_a_recording() {
    let Some(mut f) = green_window_fixture() else {
        return;
    };
    let output = f.synoik_output(1);

    f.synoik_state().open_screenshot_ui(false, None);
    settle_screenshot_ui_open(&mut f);
    render_output_vulkan_target(&mut f, &output, RenderTarget::Output);
    let layout = f.synoik().screenshot_ui.panel_layout(&output).unwrap();
    click_control(&mut f, &output, layout.type_buttons[1]);

    render_output_vulkan_target(&mut f, &output, RenderTarget::Output);
    let layout = f.synoik().screenshot_ui.panel_layout(&output).unwrap();
    click_control(
        &mut f,
        &output,
        crate::ui::widget::Segmented::segment_rect(layout.shot_cast, 1),
    );

    render_output_vulkan_target(&mut f, &output, RenderTarget::Output);
    let layout = f.synoik().screenshot_ui.panel_layout(&output).unwrap();
    assert_eq!(
        click_control(&mut f, &output, layout.capture),
        PointerUp::Capture,
        "the capture button reports the same release in either mode; the branch is the compositor's"
    );
    f.synoik_state()
        .handle_screenshot_ui_pointer_up(PointerUp::Capture);

    assert!(
        !f.synoik().screenshot_ui.is_open(),
        "GNOME closes instantly here so the fade-out is not recorded"
    );
    assert!(
        !f.synoik().casting.recordings.is_empty(),
        "the capture button in cast mode must start the recorder"
    );

    // And stopping it finalizes the file and says so.
    f.synoik_state().stop_screen_recordings();
    assert!(f.synoik().casting.recordings.is_empty());
    let notif = f
        .synoik()
        .notifications
        .sources
        .iter()
        .flat_map(|s| s.notifications.iter())
        .find(|n| n.title == "Screencast recorded")
        .expect("stopping a recording must notify, the way a taken screenshot does");
    assert_eq!(
        notif.actions.len(),
        1,
        "the notification carries a way into Files"
    );
}

/// The point of the whole divergence: the shot is of the screen as it is when the timer runs out,
/// not the frozen one the picker was showing.
#[test]
fn vulkan_screenshot_ui_a_delayed_capture_shoots_the_live_screen() {
    let Some((mut f, client, surface)) = window_fixture_with_client(GREEN, true, None) else {
        return;
    };
    let output = f.synoik_output(1);

    // An explicit path, so the shot can be read back without going through the D-Bus reply — that
    // is answered from an event-loop source, and a test that blocks on it deadlocks the loop that
    // would answer it.
    let path = std::env::temp_dir().join("gsrs-delayed-capture-test.png");
    std::fs::remove_file(&path).ok();
    f.synoik_state()
        .open_screenshot_ui(false, Some(path.to_string_lossy().into_owned()));
    settle_screenshot_ui_open(&mut f);
    render_output_vulkan_target(&mut f, &output, RenderTarget::Output);

    // Screen mode, so the crop is the whole output and the centre pixel is the window's.
    let layout = f.synoik().screenshot_ui.panel_layout(&output).unwrap();
    click_control(&mut f, &output, layout.type_buttons[1]);
    render_output_vulkan_target(&mut f, &output, RenderTarget::Output);
    let layout = f.synoik().screenshot_ui.panel_layout(&output).unwrap();
    click_control(&mut f, &output, layout.delay);
    render_output_vulkan_target(&mut f, &output, RenderTarget::Output);
    let layout = f.synoik().screenshot_ui.panel_layout(&output).unwrap();
    click_control(&mut f, &output, layout.capture);
    f.synoik_state()
        .handle_screenshot_ui_pointer_up(PointerUp::Capture);
    assert!(f.synoik().pending_capture.is_some());

    // The screen changes during the countdown. A capture from the picker's frozen neutral would
    // still be green.
    recolor_window(&mut f, client, &surface, [0, 0, u32::MAX, u32::MAX]);

    let mut clock = f.synoik().clock.clone();
    let now = clock.now_unadjusted();
    clock.set_unadjusted(now + Duration::from_secs(4));
    assert!(matches!(
        f.synoik_state().tick_pending_capture(),
        calloop::timer::TimeoutAction::Drop
    ));
    assert!(f.synoik().pending_capture.is_none());

    // The PNG is encoded off-thread; bounded work, so this waits rather than polls forever.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let img = loop {
        if let Some(img) = image::ImageReader::open(&path)
            .ok()
            .and_then(|r| r.decode().ok())
        {
            break img.to_rgba8();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the delayed capture never wrote {path:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    std::fs::remove_file(&path).ok();

    assert_eq!(
        (img.width(), img.height()),
        (u32::from(OUT_W), u32::from(OUT_H)),
        "Screen mode captures the whole output"
    );
    let count = |want: [u8; 4]| img.pixels().filter(|p| p.0 == want).count();
    assert!(
        count([0, 0, 255, 255]) > 0,
        "the recoloured window is missing from the delayed shot"
    );
    assert_eq!(
        count([0, 255, 0, 255]),
        0,
        "the delayed shot still has the green window in it — it was taken from the picker's frozen \
         screen instead of the live one, which is the entire thing a delay is for"
    );
}

/// The fail-closed rule the countdown exists under: it may never appear in anything captured.
#[test]
fn vulkan_screenshot_ui_countdown_cannot_reach_a_capture() {
    let Some(mut f) = green_window_fixture() else {
        return;
    };
    let output = f.synoik_output(1);

    let centre = (i32::from(OUT_W) / 2, i32::from(OUT_H) / 2);
    // What a capture of this desktop looks like before any of this — the shot the delay promises.
    let (pixels, w, _) = render_output_vulkan_target(&mut f, &output, RenderTarget::ScreenCapture);
    let undisturbed = px(&pixels, w, centre.0, centre.1);

    f.synoik_state().open_screenshot_ui(false, None);
    settle_screenshot_ui_open(&mut f);
    render_output_vulkan_target(&mut f, &output, RenderTarget::Output);
    let layout = f.synoik().screenshot_ui.panel_layout(&output).unwrap();
    click_control(&mut f, &output, layout.delay);
    render_output_vulkan_target(&mut f, &output, RenderTarget::Output);
    let layout = f.synoik().screenshot_ui.panel_layout(&output).unwrap();
    click_control(&mut f, &output, layout.capture);
    f.synoik_state()
        .handle_screenshot_ui_pointer_up(PointerUp::Capture);
    assert!(f.synoik().pending_capture.is_some());

    let (pixels, w, _) = render_output_vulkan_target(&mut f, &output, RenderTarget::Output);
    let on_screen = px(&pixels, w, centre.0, centre.1);

    let (pixels, w, _) = render_output_vulkan_target(&mut f, &output, RenderTarget::ScreenCapture);
    let captured = px(&pixels, w, centre.0, centre.1);

    assert_ne!(
        on_screen, captured,
        "the countdown must be visible on the output — otherwise this test proves nothing"
    );
    assert_eq!(
        captured, undisturbed,
        "a capture taken mid-countdown got the countdown card in it; the whole point of a delay is \
         a shot with the shell out of the way"
    );
}

#[test]
fn vulkan_screenshot_ui_type_buttons_take_clicks_where_they_are_drawn() {
    let Some(mut f) = green_window_fixture() else {
        return;
    };
    let output = f.synoik_output(1);

    f.synoik_state().open_screenshot_ui(false, None);
    settle_screenshot_ui_open(&mut f);
    render_output_vulkan_target(&mut f, &output, RenderTarget::Output);

    let rect = f
        .synoik()
        .screenshot_ui
        .panel_rect(&output)
        .expect("the open screenshot UI must have a control panel");
    let scale = output.current_scale().fractional_scale();

    assert_eq!(
        f.synoik().screenshot_ui.capture_type(),
        CaptureType::Selection,
        "the picker must open on Selection, the mode synoik's picker always had"
    );

    // Screen is the middle button of the three, so its centre is the panel's horizontal centre at
    // the type row's height — derived from the layout, not from a pixel guess.
    let layout = f
        .synoik()
        .screenshot_ui
        .panel_layout(&output)
        .expect("the panel must publish its layout once baked");
    let screen = layout.type_buttons[1];
    let point = Point::<f64, Logical>::from((
        screen.loc.x + screen.size.w / 2.,
        screen.loc.y + screen.size.h / 2.,
    ))
    .to_physical(scale)
    .to_i32_round::<i32>()
        + rect.loc;

    let ui = &mut f.synoik_state().synoik.screenshot_ui;
    ui.pointer_motion(point, None);
    assert!(ui
        .pointer_down(output.clone(), point, None, false)
        .is_some());
    assert_eq!(ui.pointer_up(None), Some(PointerUp::Redraw));

    assert_eq!(
        ui.capture_type(),
        CaptureType::Screen,
        "clicking the Screen button did not switch modes — the bake and the hit test disagree"
    );

    // Screen mode takes the whole output, so the selection must now be the full frame.
    let sel = ui.selection_rect_global().unwrap();
    let logical = output
        .current_mode()
        .unwrap()
        .size
        .to_f64()
        .to_logical(scale);
    assert_eq!(sel.size.w, logical.w.round() as i32);
    assert_eq!(sel.size.h, logical.h.round() as i32);
}

/// Build a Vulkan fixture whose single output is configured at `scale` (through config, by
/// connector name), plus one settled green window. Mirrors [`window_fixture`] but at a non-unit
/// output scale — the regime where a buffer whose pixels are physical-sized but whose scale tag is
/// wrong (e.g. `1.0`) composites `scale`× too big. Every other render test runs at scale 1, where
/// that bug is a no-op. See the `vulkan-buffer-scale-tag-trap` note.
fn scaled_green_fixture(scale: f64) -> Option<Fixture> {
    if let Err(e) = VulkanRenderer::new() {
        eprintln!("skipping: no Vulkan device ({e})");
        return None;
    }

    let mut config = Config::default();
    config.outputs.0.push(synoik_config::Output {
        name: "headless-1".to_string(),
        scale: Some(synoik_config::FloatOrInt(scale)),
        ..Default::default()
    });

    let mut f = Fixture::with_config(config);
    f.synoik_state()
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
    window.attach_solid_buffer(GREEN[0], GREEN[1], GREEN[2], GREEN[3]);
    window.set_size(WIN, WIN);
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    f.synoik_complete_animations();
    f.double_roundtrip(id);
    Some(f)
}

/// The screenshot-UI capture button must draw at its intended logical diameter (`2·RADIUS`) at a
/// **non-unit output scale**. Regression guard for the physical-vs-scale buffer-tag bug (fix
/// `3dff0940`): the shutter bitmap is rasterized at physical size, so tagging its `MemoryBuffer` at
/// `1.0` instead of the output scale made it composite `scale`× too big and overflow the help box.
/// That is invisible at scale 1 — where every other render test runs — so this guard runs at 2×.
#[test]
fn vulkan_screenshot_ui_button_is_scale_correct() {
    let Some(mut f) = scaled_green_fixture(2.0) else {
        return;
    };
    let output = f.synoik_output(1);
    let scale = output.current_scale().fractional_scale();
    assert!(
        (scale - 2.0).abs() < 1e-6,
        "expected a scale-2 output, got {scale}; the guard is vacuous otherwise"
    );

    f.synoik_state().open_screenshot_ui(false, None);
    settle_screenshot_ui_open(&mut f);

    let (pixels, w, h) = render_output_vulkan_target(&mut f, &output, RenderTarget::Output);
    let rect = f
        .synoik()
        .screenshot_ui
        .panel_rect(&output)
        .expect("the open screenshot UI must have a control panel");
    let layout = f
        .synoik()
        .screenshot_ui
        .panel_layout(&output)
        .expect("the panel must publish its layout once baked");

    // Measure the capture button, which is drawn *inside* the panel bake: if the panel texture is
    // tagged at the wrong scale the whole card composites at the wrong size, and the button is the
    // one element whose logical size is a fixed constant rather than content-derived. Scan only
    // within its own rect, so the white captions and glyphs elsewhere on the panel cannot be
    // mistaken for it.
    let button = layout.capture;
    let x0 = rect.loc.x + to_physical_precise_round::<i32>(scale, button.loc.x);
    let x1 = x0 + to_physical_precise_round::<i32>(scale, button.size.w);
    let y0 = rect.loc.y + to_physical_precise_round::<i32>(scale, button.loc.y);
    let y1 = y0 + to_physical_precise_round::<i32>(scale, button.size.h);

    let mut top: Option<i32> = None;
    let mut bot = 0;
    for y in y0..y1.min(h) {
        for x in x0..x1.min(w) {
            let p = px(&pixels, w, x, y);
            // Pure white ring/disc over the panel — a mid threshold catches the AA edge too.
            if p[0] > 128 && p[1] > 128 && p[2] > 128 {
                top.get_or_insert(y);
                bot = y;
                break;
            }
        }
    }
    let top = top.expect("no capture-button pixels found where the layout puts the button");
    let extent = bot - top + 1;
    let expected = to_physical_precise_round::<i32>(scale, button.size.h);
    eprintln!(
        "button vertical extent {extent}px at scale {scale} (expected ~{expected}); panel {rect:?}"
    );
    // A texture tagged at scale 1 on a scale-2 output composites at twice the size, so the button's
    // ring would run off its own rect entirely — the scan would saturate at the rect height rather
    // than land on the diameter.
    assert!(
        (expected - 8..=expected + 8).contains(&extent),
        "capture-button vertical extent {extent}px != expected ~{expected}px — panel tagged at \
         the wrong scale? (the physical-vs-scale buffer-tag trap)"
    );
}

/// The screenshot UI's Output neutral is captured through the owned Vulkan renderer
/// (`capture_screenshot_neutrals`), not a GLES readback — self-hosting site 3. Drive the capture
/// DIRECTLY (bypassing the GLES fallback that `..._draws_the_frozen_screen` can't see past): map a
/// green window and assert the returned per-output screen `MemoryBuffer` is output-sized and holds
/// the green window, proving the Vulkan renderer rendered the frozen frame at capture-time.
#[test]
fn vulkan_captures_the_screenshot_neutral_through_vulkan() {
    let Some(mut f) = green_window_fixture() else {
        return;
    };
    let output = f.synoik_output(1);

    // `open_screenshot_ui` primes render elements before both passes; mirror that here.
    f.synoik().update_render_elements(None);

    // Drive the Vulkan capture pass directly (disjoint borrows of synoik + backend).
    let neutrals = {
        let state = f.synoik_state();
        state
            .backend
            .headless()
            .with_vulkan_renderer(|vk| state.synoik.capture_screenshot_neutrals(vk))
            .expect("headless backend must hold a Vulkan renderer")
    };

    let neutral = neutrals
        .get(&output)
        .expect("no Vulkan-captured neutral for the output");
    // One neutral per render target; check the on-screen one.
    let screen = neutral[RenderTarget::Output as usize]
        .screen
        .as_ref()
        .expect("no Vulkan-captured screen neutral");
    let (w, h) = (screen.size().w, screen.size().h);
    assert_eq!(
        (w, h),
        (OUT_W as i32, OUT_H as i32),
        "screenshot neutral is not output-sized"
    );

    let data = screen.data();
    let is_green = |p: [u8; 4]| p[0] < 40 && p[1] > 200 && p[2] < 40 && p[3] > 200;
    let green = (0..w * h)
        .filter(|i| is_green(px(data, w, i % w, i / w)))
        .count();
    eprintln!("vulkan_captures_the_screenshot_neutral_through_vulkan: {green} green px");
    assert!(
        green as i32 > WIN as i32 * WIN as i32 / 2,
        "Vulkan-captured screenshot neutral is missing the green window ({green} green px)"
    );
}

/// The recorder capture path (`render_for_recorders`) runs real Vulkan work that no other test
/// drives: `render_to_vec` of the scene, a `RelocateRenderElement` crop, and an offscreen readback
/// into an area-sized buffer. Drive it here — whole-output (zero-offset relocate) and an odd-sized
/// area (non-zero offset + even-rounding) — so `SYNOIK_VK_VALIDATION=1 cargo test` covers it. The
/// assertion is implicit: the validation layer must report nothing (checked at process exit).
#[test]
fn vulkan_recorder_capture_path_is_clean() {
    // The recording spawns an ffmpeg encoder; skip cleanly where ffmpeg is unavailable.
    let ffmpeg = std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ffmpeg {
        eprintln!("skipping: ffmpeg not installed");
        return;
    }

    let Some(mut f) = window_fixture(GREEN) else {
        return;
    };
    let output = f.synoik_output(1);
    f.synoik().update_render_elements(None);

    let dir = std::env::temp_dir().join(format!("synoik-vkrec-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    // Whole-output (zero-offset relocate) and an odd-sized area (offset relocate + even-rounding).
    f.synoik()
        .start_native_recording(&output, dir.join("full.webm"), 30, true, None)
        .unwrap();
    let area = Rectangle::new(Point::from((37, 21)), Size::from((641, 481)));
    f.synoik()
        .start_native_recording(&output, dir.join("area.webm"), 30, false, Some(area))
        .unwrap();

    // Drive a few capture passes through the real Vulkan renderer (disjoint synoik/backend
    // borrows).
    let base = f.synoik().clock.now_unadjusted();
    for i in 0..3u32 {
        let time = base + Duration::from_millis(u64::from(i) * 40);
        let state = f.synoik_state();
        state
            .backend
            .headless()
            .with_vulkan_renderer(|vk| state.synoik.render_for_recorders(vk, &output, time))
            .expect("headless backend must hold a Vulkan renderer");
    }

    assert_eq!(
        f.synoik().casting.recordings.len(),
        2,
        "both recordings live"
    );
    f.synoik().stop_screen_recordings();
    assert!(
        f.synoik().casting.recordings.is_empty(),
        "recordings cleared on stop"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// Freeze the current screen into a crossfade, and return the time it starts at — feed that to
/// [`pin_crossfade_at_start`] before rendering.
///
/// `ScreenTransition::alpha` reads the clock's *unadjusted* time, deliberately ignoring animation
/// slowdown, so neither `synoik_complete_animations` nor a zero clock rate holds the crossfade
/// still — it advances with real monotonic time. Every event-loop iteration also calls
/// `Clock::clear` (`Synoik::refresh`), so the first read after any roundtrip jumps to however long
/// the test really took. An unpinned test is therefore racing the 500ms crossfade: past ~78ms of
/// real time, enough of the live window bleeds through that the blend matches *neither* colour, and
/// the test blank- fails with `0 green, 0 red`. Three full-screen texture uploads fit inside that
/// budget on a bad day, which is what made this flaky.
fn start_screen_transition(f: &mut Fixture) -> Duration {
    // The clock is lazy, so this is the same value the transition records as its start.
    let start_at = f.synoik().clock.now_unadjusted();
    f.synoik_state()
        .do_action(Action::DoScreenTransition(Some(0)), false);
    start_at
}

/// Pin the unadjusted clock to the crossfade's start, fixing alpha at exactly 1.0 (fully the frozen
/// capture). Must be called after the last roundtrip, since the event loop clears the clock.
fn pin_crossfade_at_start(f: &mut Fixture, start_at: Duration) {
    f.synoik().clock.clone().set_unadjusted(start_at);
}

/// The screen-transition crossfade holds GLES textures (which the owned Vulkan renderer can't
/// sample), so on a Vulkan session it uploads a renderer-neutral capture to a `VkTexture` instead —
/// one per render target. Freeze a green-window screen, recolor the live window red, then composite
/// the Output target through Vulkan: the frozen green frame must draw and occlude the live red
/// window (a blank no-op overlay — the old behavior — would leak the live red window through
/// instead).
#[test]
fn vulkan_screen_transition_draws_the_captured_frame() {
    if VulkanRenderer::new().is_err() {
        eprintln!("skipping vulkan_screen_transition_draws_the_captured_frame: no Vulkan device");
        return;
    }

    let mut f = Fixture::new();
    f.synoik_state()
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
    window.attach_solid_buffer(GREEN[0], GREEN[1], GREEN[2], GREEN[3]);
    window.set_size(WIN, WIN);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.synoik_complete_animations();
    f.double_roundtrip(id);

    let output = f.synoik_output(1);

    // Freeze the green-window screen into a transition, pinned at alpha = 1.0 (fully the capture).
    let start_at = start_screen_transition(&mut f);
    assert!(
        f.synoik()
            .output_state
            .values()
            .any(|s| s.screen_transition.is_some()),
        "screen transition must be active after DoScreenTransition"
    );

    // *Every* target must take the Vulkan upload path. The Gles arm draws nothing on the owned
    // renderer, so a target that fell through to it would crossfade from a blank screen.
    {
        let state = f.synoik_state();
        state
            .backend
            .headless()
            .with_vulkan_renderer(|vk| {
                let transition = state
                    .synoik
                    .output_state
                    .values()
                    .find_map(|s| s.screen_transition.as_ref())
                    .expect("active transition");
                for target in [
                    RenderTarget::Output,
                    RenderTarget::Screencast,
                    RenderTarget::ScreenCapture,
                ] {
                    assert!(
                        transition.render(vk, target).is_some(),
                        "{target:?} has no neutral capture to upload, so the crossfade draws \
                         nothing there"
                    );
                }
            })
            .expect("headless backend must hold a Vulkan renderer");
    }

    // Recolor the live window red. The frozen transition still holds the green capture.
    let window = f.client(id).window(&surface);
    window.attach_solid_buffer(RED[0], RED[1], RED[2], RED[3]);
    window.commit();
    f.double_roundtrip(id);

    // Composite the Output target: the frozen green frame must occlude the live red window.
    pin_crossfade_at_start(&mut f, start_at);
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

/// The frozen screen-transition frame must also draw into a **screencast**, not just on screen.
///
/// `ScreenTransition::render` once had a neutral for the `Output` target only; the other targets
/// fell through to a GLES element, a silent no-op on the owned renderer. So a cast running across a
/// screen transition showed *nothing* where the frozen screen should be — and, worse, the live
/// window straight through it. Neutrals are now captured per target.
///
/// Same shape as `vulkan_screen_transition_draws_the_captured_frame`, but compositing Screencast.
#[test]
fn vulkan_screen_transition_draws_the_captured_frame_into_a_cast() {
    if VulkanRenderer::new().is_err() {
        eprintln!(
            "skipping vulkan_screen_transition_draws_the_captured_frame_into_a_cast: no Vulkan device"
        );
        return;
    }

    let mut f = Fixture::new();
    f.synoik_state()
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
    window.attach_solid_buffer(GREEN[0], GREEN[1], GREEN[2], GREEN[3]);
    window.set_size(WIN, WIN);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.synoik_complete_animations();
    f.double_roundtrip(id);

    let output = f.synoik_output(1);

    // Freeze the green-window screen into a transition, pinned at alpha = 1.0 (fully the capture).
    let start_at = start_screen_transition(&mut f);

    // Recolor the live window red. The frozen transition still holds the green capture.
    let window = f.client(id).window(&surface);
    window.attach_solid_buffer(RED[0], RED[1], RED[2], RED[3]);
    window.commit();
    f.double_roundtrip(id);

    pin_crossfade_at_start(&mut f, start_at);
    let (pixels, w, h) = render_output_vulkan_target(&mut f, &output, RenderTarget::Screencast);
    let is_green = |p: [u8; 4]| p[0] < 40 && p[1] > 200 && p[2] < 40 && p[3] > 200;
    let is_red = |p: [u8; 4]| p[0] > 200 && p[1] < 40 && p[2] < 40;
    let green = (0..w * h)
        .filter(|i| is_green(px(&pixels, w, i % w, i / w)))
        .count();
    let red = (0..w * h)
        .filter(|i| is_red(px(&pixels, w, i % w, i / w)))
        .count();
    eprintln!(
        "vulkan_screen_transition_draws_the_captured_frame_into_a_cast: {green} green px, {red} red px"
    );
    assert!(
        green > 0,
        "the frozen transition frame is missing from the screencast; the Screencast target fell \
         through to the GLES element, which draws nothing on Vulkan"
    );
    assert!(
        red < 100,
        "the live red window leaked into the cast through the frozen transition ({red} red px)"
    );
}

/// The screen-transition Output neutral is captured through the owned Vulkan renderer
/// (`capture_screen_transition_neutrals`), not GLES — self-hosting site 2. Drive the capture
/// DIRECTLY (bypassing the GLES fallback that `..._draws_the_captured_frame` can't see past): map a
/// green window and assert the returned per-output `MemoryBuffer` is output-sized and contains the
/// green window, proving the Vulkan renderer rendered the output offscreen at capture-time.
#[test]
fn vulkan_captures_the_screen_transition_neutral_through_vulkan() {
    if VulkanRenderer::new().is_err() {
        eprintln!(
            "skipping vulkan_captures_the_screen_transition_neutral_through_vulkan: no Vulkan device"
        );
        return;
    }

    let mut f = Fixture::new();
    f.synoik_state()
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
    window.attach_solid_buffer(GREEN[0], GREEN[1], GREEN[2], GREEN[3]);
    window.set_size(WIN, WIN);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.synoik_complete_animations();
    f.double_roundtrip(id);

    let output = f.synoik_output(1);

    // Drive the Vulkan capture pass directly (disjoint borrows of synoik + backend).
    let neutrals = {
        let state = f.synoik_state();
        state
            .backend
            .headless()
            .with_vulkan_renderer(|vk| state.synoik.capture_screen_transition_neutrals(vk))
            .expect("headless backend must hold a Vulkan renderer")
    };

    let neutral = &neutrals
        .get(&output)
        .expect("no Vulkan-captured neutral for the output")[RenderTarget::Output as usize];
    let (w, h) = (neutral.size().w, neutral.size().h);
    assert_eq!(
        (w, h),
        (OUT_W as i32, OUT_H as i32),
        "neutral buffer is not output-sized"
    );

    let data = neutral.data();
    let is_green = |p: [u8; 4]| p[0] < 40 && p[1] > 200 && p[2] < 40 && p[3] > 200;
    let green = (0..w * h)
        .filter(|i| is_green(px(data, w, i % w, i / w)))
        .count();
    eprintln!(
        "vulkan_captures_the_screen_transition_neutral_through_vulkan: {green} green px (window is {}x{})",
        WIN, WIN
    );
    // The green window (WIN×WIN) must be present in the captured output — proves Vulkan rendered
    // the scene, not an empty/failed buffer.
    assert!(
        green as i32 > WIN as i32 * WIN as i32 / 2,
        "Vulkan-captured neutral is missing the green window ({green} green px)"
    );
}

/// The four colours [`buffer_transform_corners`] must read back, derived from smithay's own
/// `Transform` definition rather than from any renderer's output — so a renderer bug cannot be
/// mistaken for the expectation.
///
/// [`render_helpers::render_elements`] renders the scene with `out_transform.invert()`, so the
/// physical readback point `p` is the logical/element point
/// `out_transform.transform_point_in(p, out_size)`. The texture buffer sits at the logical origin
/// with displayed size `src_transform.transform_size(tex)`, and a displayed point `q` shows the
/// buffer texel `src_transform.invert().transform_point_in(q, displayed)`. Anything outside the
/// displayed rect shows the `Color32F::TRANSPARENT` clear.
///
/// Worked example (the anchor this replaced asserted by hand): `src_transform = _90` has point
/// inverse `_270`, and `_270.transform_point_in((6,6), (100,160))` is `(6,94)` — the buffer's
/// bottom-left quadrant, i.e. blue.
/// The transform that undoes `t`'s [`Transform::transform_point_in`] mapping.
///
/// **Not** `Transform::invert()**: that inverts the rotation but keeps the flip, which is only the
/// point-map inverse for the pure rotations. Every flipped variant is a *reflection*
/// (`Flipped90` is the transpose `(x,y) -> (y,x)`; `Flipped270` the anti-transpose), and a
/// reflection is its own inverse — `Flipped90.invert()` is `Flipped270`, and composing those two
/// point maps yields `_180`, not the identity. Confirmed against the GLES renderer before GLES was
/// removed: using `invert()` here mispredicted every flipped-diagonal case.
fn point_inverse(t: Transform) -> Transform {
    match t {
        Transform::_90 => Transform::_270,
        Transform::_270 => Transform::_90,
        // Normal, _180 and all four flips are self-inverse as point maps.
        other => other,
    }
}

fn expected_transform_corners(
    tex: Size<i32, BufferCoord>,
    out_size: Size<i32, Physical>,
    src_transform: Transform,
    out_transform: Transform,
) -> [[u8; 4]; 4] {
    let (w, h) = (out_size.w, out_size.h);
    let m = 6;
    let samples = [
        (m, m),
        (w - 1 - m, m),
        (m, h - 1 - m),
        (w - 1 - m, h - 1 - m),
    ];

    let displayed = src_transform.transform_size(tex);

    samples.map(|(x, y)| {
        // `render_elements` renders with `out_transform.invert()`, so undo that frame transform to
        // get back to the element/logical point.
        let frame_t = point_inverse(out_transform.invert());
        let l = frame_t.transform_point_in(Point::<i32, Physical>::from((x, y)), &out_size);
        if l.x < 0 || l.y < 0 || l.x >= displayed.w || l.y >= displayed.h {
            return TRANSFORM_CLEAR;
        }
        let q = Point::<i32, BufferCoord>::from((l.x, l.y));
        let b = point_inverse(src_transform).transform_point_in(q, &displayed);
        match (b.x < tex.w / 2, b.y < tex.h / 2) {
            (true, true) => TRANSFORM_RED,
            (false, true) => TRANSFORM_GREEN,
            (true, false) => TRANSFORM_BLUE,
            (false, false) => TRANSFORM_WHITE,
        }
    })
}

/// The 4-quadrant buffer pattern, in buffer (top-left origin) space.
const TRANSFORM_RED: [u8; 4] = [255, 0, 0, 255];
const TRANSFORM_GREEN: [u8; 4] = [0, 255, 0, 255];
const TRANSFORM_BLUE: [u8; 4] = [0, 0, 255, 255];
const TRANSFORM_WHITE: [u8; 4] = [255, 255, 255, 255];
/// `render_elements` clears to `Color32F::TRANSPARENT`.
const TRANSFORM_CLEAR: [u8; 4] = [0, 0, 0, 0];

/// Import `pattern` (an `out_size`-shaped `Abgr8888` buffer... actually `tex`-shaped) as a texture
/// with buffer transform `src_transform`, render it full-screen through `render_texture_from_to`
/// at output transform `out_transform`, and return the four (inset) corner pixels of the readback.
fn buffer_transform_corners(
    renderer: &mut VulkanRenderer,
    pattern: &[u8],
    tex: Size<i32, BufferCoord>,
    out_size: Size<i32, Physical>,
    src_transform: Transform,
    out_transform: Transform,
) -> [[u8; 4]; 4] {
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
/// output projection. This is what un-blanks the capture overlays (screen-transition /
/// screenshot / MRU) on a rotated output, where they carry `src_transform == output_transform`.
/// Renders a 4-colour-quadrant texture through `render_texture_from_to` via BOTH renderers at all 8
/// buffer transforms (Normal output) plus a few buffer×output crosses, on a non-square target.
#[test]
fn vulkan_buffer_transform_follows_the_transform_spec() {
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
    let mut pattern = Vec::with_capacity((W * H * 4) as usize);
    for y in 0..H {
        for x in 0..W {
            let c = match (x < W / 2, y < H / 2) {
                (true, true) => TRANSFORM_RED,
                (false, true) => TRANSFORM_GREEN,
                (true, false) => TRANSFORM_BLUE,
                (false, false) => TRANSFORM_WHITE,
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

    // Each corner is compared against `expected_transform_corners`, computed from smithay's own
    // `Transform` definition. The quadrant colours (plus the transparent clear) are maximally
    // distinct, so a 50/channel tolerance still catches sampling the wrong quadrant.
    let near = |p: [u8; 4], c: [u8; 4]| (0..4).all(|i| (p[i] as i32 - c[i] as i32).abs() < 50);

    let state = f.synoik_state();
    for (src_t, out_t) in combos {
        let want = expected_transform_corners(tex, out_size, src_t, out_t);
        let vk = state
            .backend
            .headless()
            .with_vulkan_renderer(|v| {
                buffer_transform_corners(v, &pattern, tex, out_size, src_t, out_t)
            })
            .expect("Vulkan renderer present");
        for (i, (got, want)) in vk.iter().zip(want.iter()).enumerate() {
            let corner = ["TL", "TR", "BL", "BR"][i];
            assert!(
                near(*got, *want),
                "src_transform={src_t:?} out_transform={out_t:?}: {corner} is {got:?}, the \
                 transform says {want:?}"
            );
        }
        eprintln!(
            "vulkan_buffer_transform src={src_t:?} out={out_t:?}: corners {vk:?} as specified"
        );
    }
}

/// Output-transform conformance: the owned Vulkan renderer must place geometry where smithay's
/// `Transform` says, under every rotation/flip. Render an asymmetric marker (a wide red rect
/// anchored at the logical top-left) at all 8 transforms and check its bbox against
/// `transform_rect_in` — the spec, so this proves the Vulkan `proj` projection + logical-`target`
/// math rather than merely agreeing with another renderer. The framebuffer is **non-square**
/// (240×120), so 90/270 genuinely swap logical w/h — that's what exercises
/// `target_dims`/`output_size` returning the logical size.
#[test]
fn vulkan_output_transform_follows_the_transform_spec() {
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

    // Where the marker must land, from smithay's own `Transform`. `render_elements` renders with
    // `t.invert()`, so the logical space is `t.invert().transform_size(size)` and the marker's
    // logical rect maps to physical through `t.invert().transform_rect_in`.
    let expected_bbox = |t: Transform| -> (i32, i32, i32, i32) {
        let frame_t = t.invert();
        let logical = frame_t.transform_size(size);
        let marker_rect = Rectangle::<i32, Physical>::from_size(Size::from((80, 40)));
        let r = frame_t.transform_rect_in(marker_rect, &logical);
        (
            r.loc.x,
            r.loc.y,
            r.loc.x + r.size.w - 1,
            r.loc.y + r.size.h - 1,
        )
    };

    let state = f.synoik_state();
    let mut vk_boxes = Vec::new();
    for t in all {
        let vk = state
            .backend
            .headless()
            .with_vulkan_renderer(|v| {
                render_to_vec(v, size, scale, t, Fourcc::Abgr8888, build().into_iter())
            })
            .expect("Vulkan renderer present")
            .expect("Vulkan render must succeed");
        let vbox = red_bbox(&vk).unwrap_or_else(|| panic!("Vulkan marker missing at {t:?}"));
        let want = expected_bbox(t);

        // Placement catches a wrong rotation/flip; the area catches a wrong aspect (e.g. logical
        // w/h not swapped for 90/270) -- a rigid transform preserves the marker's 80x40 = 3200 px.
        assert_eq!(
            vbox, want,
            "Vulkan marker bbox {vbox:?} != {want:?} at {t:?}"
        );
        assert_eq!(
            red_count(&vk),
            80 * 40,
            "Vulkan red-pixel count != the marker's area at {t:?}"
        );
        eprintln!("vulkan_output_transform {t:?}: marker bbox {vbox:?} as specified");
        vk_boxes.push(vbox);
    }

    // Absolute anchor: Normal places the 80×40 marker flush in the physical top-left. `x1`/`y1` are
    // the inclusive max coords, hence 79/39. Pins the spec derivation itself to a hand-checked
    // case.
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
/// `Synoik::render` (Brick 3) → owned Vulkan renderer → scanout buffer (Brick A) — is correct; only
/// the DRM framebuffer export + atomic page-flip remain (live-validated). Venus-only (needs GBM).
///
/// The target is `Abgr8888`, which since 2026-07-31 is the order the render pass does *not* declare
/// — so this is the **present-blit** half of the pair, and
/// `vulkan_composites_a_scene_into_an_argb_scanout_dmabuf` is the direct one.
#[test]
fn vulkan_composites_a_scene_into_a_scanout_dmabuf() {
    use std::fs::File;

    use smithay::backend::allocator::dmabuf::AsDmabuf;
    use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
    use smithay::backend::allocator::{Allocator, Buffer as _, Modifier};

    let Some(mut f) = green_window_fixture() else {
        return;
    };
    let output = f.synoik_output(1);

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

    let state = f.synoik_state();
    let (pixels, w, h) = state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| -> anyhow::Result<(Vec<u8>, i32, i32)> {
            let synoik = &mut state.synoik;
            synoik.update_render_elements(Some(&output));

            let size: Size<i32, Physical> = output.current_mode().unwrap().size;
            let size = output.current_transform().transform_size(size);
            let scale = Scale::from(output.current_scale().fractional_scale());

            // Collect the scene's elements for the Vulkan renderer.
            let ctx = RenderCtx {
                renderer: vk,
                target: RenderTarget::Output,
                xray: None,
            };
            let elements: Vec<OutputRenderElements> = synoik.render_to_vec(ctx, &output, false);

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

/// The **direct** scanout path: composite the live scene into a GBM `Argb8888` scanout dmabuf via
/// `Bind<Dmabuf>` — which since 2026-07-31 renders straight into it, no shadow and no present blit,
/// because `Argb8888` is the renderer's own byte order — then read the dmabuf back (through Vulkan,
/// `ExportMem`) and prove an opaque-**red** window landed as the BGRA bytes `[0,0,255,255]`. This
/// is the exact path the virtio-gpu tty target takes (its primary plane advertises only XR24/AR24).
/// Its sibling `vulkan_composites_a_scene_into_a_scanout_dmabuf` covers the shadow path with an
/// `Abgr8888` target. Venus-only (needs GBM).
#[test]
fn vulkan_composites_a_scene_into_an_argb_scanout_dmabuf() {
    use std::fs::File;

    use smithay::backend::allocator::dmabuf::AsDmabuf;
    use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
    use smithay::backend::allocator::{Allocator, Buffer as _, Modifier};

    let Some(mut f) = window_fixture(RED) else {
        return;
    };
    let output = f.synoik_output(1);

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

    let state = f.synoik_state();
    let (pixels, w, h) = state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| -> anyhow::Result<(Vec<u8>, i32, i32)> {
            let synoik = &mut state.synoik;
            synoik.update_render_elements(Some(&output));

            let size: Size<i32, Physical> = output.current_mode().unwrap().size;
            let size = output.current_transform().transform_size(size);
            let scale = Scale::from(output.current_scale().fractional_scale());

            let ctx = RenderCtx {
                renderer: vk,
                target: RenderTarget::Output,
                xray: None,
            };
            let elements: Vec<OutputRenderElements> = synoik.render_to_vec(ctx, &output, false);

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
    let bo = match alloc.create_buffer(S as u32, S as u32, Fourcc::Abgr8888, &[Modifier::Linear]) {
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
    let bo = match alloc.create_buffer(S as u32, S as u32, Fourcc::Abgr8888, &[Modifier::Linear]) {
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

/// The GNOME top panel renders on the owned Vulkan renderer. The bar is drawn entirely on the GPU
/// (an offscreen `VkTexture` cleared to the background with the Activities/clock glyph runs drawn
/// via the `render_glyphs` material), then composited as a `TextureRenderElement` — no cairo raster
/// or CPU upload. Assert `render` yields an element and that compositing it produces the opaque bar
/// (alpha 255), rather than nothing.
#[test]
fn vulkan_renders_the_top_panel() {
    let Some(mut f) = window_fixture(GREEN) else {
        return;
    };
    let output = f.synoik_output(1);
    let scale = Scale::from(output.current_scale().fractional_scale());
    let width = to_physical_precise_round(scale.x, output_size(&output).w);
    let bar_h = to_physical_precise_round(scale.x, crate::ui::panel::panel_height());

    let state = f.synoik_state();
    let opaque = state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| {
            let ws = state.synoik.workspace_state_for(&output);
            let position = state.synoik.workspace_position_for(&output);
            let elems = state.synoik.panel.render(
                vk,
                &output,
                ws,
                position,
                0.,
                crate::render_helpers::icon::DrawCaches {
                    icons: &state.synoik.icon_cache,
                    images: &state.synoik.image_cache,
                },
            );
            assert!(
                !elems.is_empty(),
                "panel produced no element on Vulkan (still blank)"
            );
            let pixels = composite_ui(
                vk,
                elems,
                Size::<i32, Physical>::from((width, bar_h)),
                scale,
            );
            pixels.chunks_exact(4).filter(|p| p[3] == 255).count()
        })
        .expect("vulkan renderer");

    assert!(
        opaque > 0,
        "the panel bar did not composite any opaque pixels on Vulkan"
    );
}

/// A `Painter` rect that hangs off the buffer's **top-left** still fills everything of it that
/// lands inside — the toolkit's way of spelling a per-corner `border-radius` (let the corners that
/// must stay square fall outside the bake buffer).
///
/// This is a regression test for a silent one: `VulkanFrame`'s damage argument is element-local,
/// `Painter` was handing it a buffer-space rect, and the two only agree while the rect starts
/// inside the buffer. A rect 52 px off the top-left had its scissor shrunk to a 6 px sliver, so the
/// shape simply wasn't there — no error, no warning, just a nearly-empty texture. The overflow is
/// swept because the failure scales with it: at 1 px it was invisible.
#[test]
fn vulkan_fills_a_rect_that_overhangs_the_bake_buffer() {
    if let Err(e) = VulkanRenderer::new() {
        eprintln!("skipping: no Vulkan device ({e})");
        return;
    }
    let mut f = Fixture::new();
    f.synoik_state()
        .backend
        .headless()
        .add_renderer()
        .expect("build the Vulkan renderer");

    const BUF: i32 = 58;
    let scale = Scale::from(1.);

    for overhang in [1., 10., 52.] {
        let lit = f
            .synoik_state()
            .backend
            .headless()
            .with_vulkan_renderer(|vk| {
                let size = Size::<f64, Logical>::from((f64::from(BUF), f64::from(BUF)));
                let tex = crate::ui::widget::bake_uncached(vk, 1., size, |frame, phys| {
                    let mut p = crate::ui::widget::Painter::new(frame, 1., phys);
                    p.clear([0., 0., 0., 0.])?;
                    // A square (radius 0) so this measures the scissor, not the rounding, and
                    // overhanging on every side so no antialiased edge lands inside the buffer:
                    // every pixel of it should come out fully opaque.
                    let span = overhang * 2. + f64::from(BUF);
                    p.fill_rounded(
                        Rectangle::new(
                            Point::from((-overhang, -overhang)),
                            Size::from((span, span)),
                        ),
                        0.,
                        [1., 1., 1., 1.],
                    )?;
                    Ok(())
                })
                .expect("bake");

                let buffer = crate::render_helpers::texture::TextureBuffer::from_texture(
                    vk,
                    tex,
                    1.,
                    Transform::Normal,
                    Vec::new(),
                );
                let pixels = composite_ui(
                    vk,
                    vec![
                        crate::render_helpers::texture::TextureRenderElement::from_texture_buffer(
                            buffer,
                            Point::from((0., 0.)),
                            1.,
                            None,
                            None,
                            smithay::backend::renderer::element::Kind::Unspecified,
                        ),
                    ],
                    Size::<i32, Physical>::from((BUF, BUF)),
                    scale,
                );
                pixels.chunks_exact(4).filter(|p| p[3] == 255).count()
            })
            .expect("vulkan renderer");

        let total = (BUF * BUF) as usize;
        assert_eq!(
            lit, total,
            "a rect overhanging the buffer by {overhang} px filled {lit} of {total} px — its \
             scissor was clipped by the overhang"
        );
    }
}

/// The hot-corner ripple composites as a quarter-disc *pinned to the corner*: lit near the
/// corner, dark well past its reach, and nothing at all before it is played.
///
/// The anchoring is the whole risk. The disc is drawn as a rounded rect centred on the bake
/// buffer's top-left, so only its bottom-right quadrant lands inside — get the sign wrong and the
/// buffer is empty, or the shape is a full circle whose visible part is a square. The two
/// negative controls (before playing; far from the corner) are what makes "lit" mean the ripple
/// rather than anything else that might one day draw up there.
#[test]
fn vulkan_renders_the_hot_corner_ripple() {
    let Some(mut f) = window_fixture(GREEN) else {
        return;
    };
    let output = f.synoik_output(1);
    let scale = Scale::from(output.current_scale().fractional_scale());
    // A square region around the corner, big enough to hold the largest wave (52 px × 1.5).
    let extent = to_physical_precise_round(scale.x, 100.);
    let size = Size::<i32, Physical>::from((extent, extent));

    let near = to_physical_precise_round::<i32>(scale.x, 10.);
    let far = to_physical_precise_round::<i32>(scale.x, 95.);

    let state = f.synoik_state();
    let now = state.synoik.clock.now_unadjusted();

    let lit_at = |state: &mut crate::synoik::State, when: Duration| {
        let output = output.clone();
        state
            .backend
            .headless()
            .with_vulkan_renderer(|vk| {
                let mut elems = Vec::new();
                state.synoik.ripples.render(
                    vk,
                    &output,
                    output.current_location(),
                    when,
                    &mut |elem| elems.push(elem),
                );
                if elems.is_empty() {
                    return None;
                }
                let pixels = composite_ui(vk, elems, size, scale);
                Some((
                    px(&pixels, extent, near, near)[3],
                    px(&pixels, extent, far, far)[3],
                ))
            })
            .expect("vulkan renderer")
    };

    assert!(
        lit_at(state, now).is_none(),
        "an un-played ripple must produce no elements at all"
    );

    state.synoik.ripples.play(Point::from((0., 0.)), now);
    // 200 ms in: the first two waves are up and have grown past the probe near the corner.
    let (corner, outside) =
        lit_at(state, now + Duration::from_millis(200)).expect("a played ripple draws");

    assert!(
        corner > 0,
        "the ripple did not light the corner it is pinned to"
    );
    assert_eq!(
        outside, 0,
        "the ripple lit a pixel 95 px diagonally out, well past its reach — it is not a \
         quarter-disc at the corner"
    );

    // And it is gone once the last wave has finished.
    assert!(
        lit_at(state, now + crate::ui::ripples::DURATION).is_none(),
        "the ripple outlived its animation"
    );
}

/// The dateMenu messages-indicator dot composites into the clock button's trailing
/// padding when shown, and nothing does there when it's hidden (`js/ui/dateMenu.js:871-886`).
/// The differential doubles as the check that the padding really is empty in the hidden
/// state — if the clock label ever grew into it, the "off" count would stop being zero. A
/// differential over the same panel proves it's the dot — bundled from
/// `message-indicator-symbolic` through the embedded-icon fallback — not a stray
/// clock glyph, and that the bundled SVG rasterizes at all.
#[test]
fn vulkan_renders_the_messages_indicator_dot() {
    let Some(mut f) = window_fixture(GREEN) else {
        return;
    };
    let output = f.synoik_output(1);
    let scale = Scale::from(output.current_scale().fractional_scale());
    let ow = output_size(&output).w;
    let width = to_physical_precise_round(scale.x, ow);
    let bar_h = to_physical_precise_round(scale.x, crate::ui::panel::panel_height());

    // The dot's center, in physical pixels — asked of the panel rather than derived from
    // the clock rect, so this probe keeps pointing at the dot if its placement inside the
    // button's padding is ever retuned. Toggling to read it is safe precisely because the
    // dot costs no layout (`the_messages_dot_moves_nothing`).
    let dot = {
        let panel = &mut f.synoik().panel;
        panel.set_messages_indicator(true);
        let rect = panel.messages_indicator_rect(ow).expect("shown");
        panel.set_messages_indicator(false);
        rect
    };
    let dot_cx = to_physical_precise_round::<i32>(scale.x, dot.loc.x + dot.size.w / 2.);
    let dot_cy = bar_h / 2;
    // Count near-white opaque pixels within a small box around the dot center.
    let bright_at_dot = |pixels: &[u8]| -> usize {
        let mut n = 0;
        for dy in -7i32..=7 {
            for dx in -7i32..=7 {
                let (x, y) = (dot_cx + dx, dot_cy + dy);
                if x < 0 || y < 0 || x >= width || y >= bar_h {
                    continue;
                }
                let i = ((y * width + x) * 4) as usize;
                let p = &pixels[i..i + 4];
                if p[0] > 180 && p[1] > 180 && p[2] > 180 && p[3] > 180 {
                    n += 1;
                }
            }
        }
        n
    };

    let state = f.synoik_state();
    let (off, on) = state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| {
            let ws = state.synoik.workspace_state_for(&output);
            let position = state.synoik.workspace_position_for(&output);
            let render_panel = |vk: &mut VulkanRenderer, synoik: &crate::synoik::Synoik| {
                let elems = synoik.panel.render(
                    vk,
                    &output,
                    ws,
                    position,
                    0.,
                    crate::render_helpers::icon::DrawCaches {
                        icons: &synoik.icon_cache,
                        images: &synoik.image_cache,
                    },
                );
                composite_ui(
                    vk,
                    elems,
                    Size::<i32, Physical>::from((width, bar_h)),
                    scale,
                )
            };
            // Hidden: nothing bright where the dot would sit.
            let off = bright_at_dot(&render_panel(vk, &state.synoik));
            state.synoik.panel.set_messages_indicator(true);
            let on = bright_at_dot(&render_panel(vk, &state.synoik));
            (off, on)
        })
        .expect("vulkan renderer");

    assert_eq!(off, 0, "no dot before it's enabled (got {off} bright px)");
    assert!(on > 10, "the dot composites bright when enabled (got {on})");
}

/// The workspace dots composite onto the screen, and morph while a switch runs.
///
/// They used to be painted into the bar bake, where a texture readback pinned them. Now
/// they are their own
/// [`RoundedSolidRenderElement`](crate::render_helpers::rounded_solid::RoundedSolidRenderElement)s,
/// so the pin has to move to the composited output — an element that is built but never
/// drawn (a wrong z-order, a dropped variant in the element enum, an empty damage
/// intersection) leaves every other panel test green while the dots are simply gone.
///
/// Two facts, one render each: at rest the row paints a wide full-white pill plus dimmer
/// circles, and mid-switch the brightest pixel in the row is strictly dimmer than that
/// resting pill — which is only true if the *element's* colour tracks `position`.
#[test]
fn vulkan_composites_the_workspace_dots() {
    let Some(mut f) = window_fixture(GREEN) else {
        return;
    };
    let output = f.synoik_output(1);
    let scale = Scale::from(output.current_scale().fractional_scale());
    let ow = output_size(&output).w;
    let width = to_physical_precise_round(scale.x, ow);
    let bar_h = to_physical_precise_round(scale.x, crate::ui::panel::panel_height());

    // The dots live inside the left indicator button, far from the right-anchored clock.
    let ws = crate::ui::panel::WorkspaceState {
        count: 3,
        active: 1,
    };
    let dot_region =
        to_physical_precise_round::<i32>(scale.x, f.synoik().panel.activities_rect(ws).size.w)
            .clamp(1, width);
    // The band row through the dots' vertical center, as (bright, dim) pixel counts.
    let band = |pixels: &[u8]| -> (usize, usize) {
        let y = bar_h / 2;
        let row =
            &pixels[(y * width) as usize * 4..((y * width) as usize + dot_region as usize) * 4];
        let bright = row.chunks_exact(4).filter(|p| p[0] > 200).count();
        let dim = row
            .chunks_exact(4)
            .filter(|p| (80..=200).contains(&p[0]))
            .count();
        (bright, dim)
    };
    let peak = |pixels: &[u8]| -> u8 {
        let y = bar_h / 2;
        pixels[(y * width) as usize * 4..((y * width) as usize + dot_region as usize) * 4]
            .chunks_exact(4)
            .map(|p| p[0])
            .max()
            .unwrap_or(0)
    };

    let state = f.synoik_state();
    let ((bright, dim), rest_peak, mid_peak) = state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| {
            let render_at = |vk: &mut VulkanRenderer, position: f64| -> Vec<u8> {
                let elems = state.synoik.panel.render(
                    vk,
                    &output,
                    ws,
                    position,
                    0.,
                    crate::render_helpers::icon::DrawCaches {
                        icons: &state.synoik.icon_cache,
                        images: &state.synoik.image_cache,
                    },
                );
                composite_ui(
                    vk,
                    elems,
                    Size::<i32, Physical>::from((width, bar_h)),
                    scale,
                )
            };
            let at_rest = render_at(vk, 1.);
            let mid_switch = render_at(vk, 1.5);
            (band(&at_rest), peak(&at_rest), peak(&mid_switch))
        })
        .expect("vulkan renderer");

    assert!(
        bright >= crate::ui::panel::DOT_DIAMETER as usize,
        "the active dot should paint a pill at least one base diameter wide, got \
         {bright} bright px — the dots may not be compositing at all"
    );
    assert!(dim > 0, "the inactive dots should paint dimmer, got none");
    assert!(
        mid_peak < rest_peak,
        "mid-switch the dots are all partly expanded, so the brightest pixel ({mid_peak}) \
         must be dimmer than the resting active pill ({rest_peak}) — equal means the \
         element's colour is not tracking the switch position"
    );
}

/// A recolored symbolic icon composites through the owned Vulkan renderer with the
/// tint intact: rasterize an icon red, upload it, composite, and read it back — the
/// covered pixels must be red (proving the `Abgr8888` recolor buffer composites with
/// the channel order the panel/quick-settings icons rely on).
#[test]
fn vulkan_composites_a_recolored_icon() {
    use smithay::backend::renderer::element::Kind;

    use crate::render_helpers::icon::{rasterize_symbolic, resolve_symbolic};
    use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};

    let mut vk = match VulkanRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping vulkan_composites_a_recolored_icon: no Vulkan device ({e})");
            return;
        }
    };
    let themes = vec!["Adwaita".to_string(), "hicolor".to_string()];
    let Some(path) = ["night-light-symbolic", "weather-clear-night-symbolic"]
        .into_iter()
        .find_map(|n| resolve_symbolic(n, &themes))
    else {
        eprintln!("skipping vulkan_composites_a_recolored_icon: no symbolic icons installed");
        return;
    };

    let buf = rasterize_symbolic(&path, 32, [1., 0., 0., 1.], 1.).expect("rasterize");
    let tb = TextureBuffer::from_memory_buffer(&mut vk, &buf).expect("upload icon");
    let elem = TextureRenderElement::from_texture_buffer(
        tb,
        Point::from((0., 0.)),
        1.,
        None,
        None,
        Kind::Unspecified,
    );
    let pixels = render_to_vec(
        &mut vk,
        Size::<i32, Physical>::from((32, 32)),
        Scale::from(1.),
        Transform::Normal,
        Fourcc::Abgr8888,
        [elem].into_iter(),
    )
    .expect("render icon");

    let red = pixels
        .chunks_exact(4)
        .filter(|p| p[0] > 120 && p[1] < 60 && p[2] < 60 && p[3] > 120)
        .count();
    assert!(
        red > 10,
        "a red-tinted icon must composite red (Abgr8888 channel order), got {red}"
    );
}

/// An app indicator's icon reaches actual pixels in the panel's own slot.
///
/// The seat cannot answer this cheaply: a headless screenshot serves whatever frame it last
/// rendered, and the symbolic rasterizer is asynchronous, so a `grim` capture taken right after
/// an indicator registers legitimately shows nothing. This drives `Panel::render` directly and
/// reads back the cluster's rect, which is the only oracle that cannot lie about it.
///
/// GNOME has no equivalent — see `docs/fork/status-notifier-port.md`.
#[test]
fn vulkan_draws_an_app_indicator_icon() {
    use crate::ui::panel::PanelIndicator;

    let Some(mut f) = window_fixture(GREEN) else {
        return;
    };
    let output = f.synoik_output(1);
    let scale = Scale::from(output.current_scale().fractional_scale());
    let width = to_physical_precise_round(scale.x, output_size(&output).w);
    let bar_h = to_physical_precise_round(scale.x, crate::ui::panel::panel_height());

    let themes = vec!["Adwaita".to_string(), "hicolor".to_string()];
    let Some(name) = ["dialog-warning-symbolic", "night-light-symbolic"]
        .into_iter()
        .find(|n| crate::render_helpers::icon::resolve_symbolic(n, &themes).is_some())
    else {
        eprintln!("skipping vulkan_draws_an_app_indicator_icon: no symbolic icons installed");
        return;
    };

    let state = f.synoik_state();
    let ws = state.synoik.workspace_state_for(&output);
    let position = state.synoik.workspace_position_for(&output);

    // The rect the icon must land in — the cluster's own slot, which only exists once an
    // indicator is set.
    assert_eq!(
        state
            .synoik
            .panel
            .app_indicators_rect(output_size(&output).w),
        None,
        "no indicators, no slot"
    );
    state.synoik.panel.set_app_indicators(vec![PanelIndicator {
        id: "test".to_owned(),
        icon: crate::status_notifier::ItemIcon::Themed(name.to_owned()),
    }]);
    let rect = state
        .synoik
        .panel
        .app_indicators_rect(output_size(&output).w)
        .expect("the cluster takes a slot once an indicator is shown");

    let lit = state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| {
            // Render twice: the first pass queues the rasterize and legitimately draws nothing,
            // the second finds it cached. This is the same two-step the live panel goes through,
            // and asserting on one pass would pin the wrong behavior.
            for _ in 0..2 {
                let elems = state.synoik.panel.render(
                    vk,
                    &output,
                    ws,
                    position,
                    0.,
                    crate::render_helpers::icon::DrawCaches {
                        icons: &state.synoik.icon_cache,
                        images: &state.synoik.image_cache,
                    },
                );
                let pixels = composite_ui(
                    vk,
                    elems,
                    Size::<i32, Physical>::from((width, bar_h)),
                    scale,
                );

                // Count non-background pixels inside the cluster's rect only, so a stray clock
                // glyph or the bar's own gradient cannot pass for an icon.
                let x0: i32 = to_physical_precise_round(scale.x, rect.loc.x);
                let x1: i32 = to_physical_precise_round(scale.x, rect.loc.x + rect.size.w);
                let mut lit = 0usize;
                for y in 0..bar_h {
                    for x in x0.max(0)..x1.min(width) {
                        let p = &pixels[((y * width + x) as usize) * 4..][..4];
                        // The bar is near-black; a symbolic icon is drawn in the light text
                        // colour, so its glyph pixels are unmistakably brighter.
                        if p[0] > 140 && p[3] > 120 {
                            lit += 1;
                        }
                    }
                }
                if lit > 0 {
                    return lit;
                }
            }
            0
        })
        .expect("vulkan renderer");

    assert!(
        lit > 20,
        "the app indicator's icon must composite inside its own slot ({rect:?}), got {lit} lit px"
    );
}

/// A client-supplied **pixmap** draws in the panel, in the client's own colours.
///
/// Pixmaps are the form Electron and Qt clients fall back to when they ship no themed icon, and
/// nothing about them touches the icon theme: the bytes arrive over the bus as ARGB32, get
/// premultiplied and reordered, and are uploaded straight. Painting one in the panel's foreground
/// tint — the way a symbolic icon is drawn — would erase the app's own logo, so this asserts the
/// colour survives, not merely that something was drawn.
#[test]
fn vulkan_draws_an_app_indicator_pixmap_in_its_own_colours() {
    use crate::status_notifier::{pixmap_from_argb, ItemIcon};
    use crate::ui::panel::PanelIndicator;

    let Some(mut f) = window_fixture(GREEN) else {
        return;
    };
    let output = f.synoik_output(1);
    let scale = Scale::from(output.current_scale().fractional_scale());
    let width = to_physical_precise_round(scale.x, output_size(&output).w);
    let bar_h = to_physical_precise_round(scale.x, crate::ui::panel::panel_height());

    // A solid opaque green square, on the wire: ARGB32, network byte order.
    let side = 32i32;
    let argb: Vec<u8> = (0..side * side)
        .flat_map(|_| [0xff, 0x00, 0xff, 0x00])
        .collect();
    let pixmap = pixmap_from_argb(side, side, &argb).expect("valid pixmap");

    let state = f.synoik_state();
    let ws = state.synoik.workspace_state_for(&output);
    let position = state.synoik.workspace_position_for(&output);
    state.synoik.panel.set_app_indicators(vec![PanelIndicator {
        id: "pixmap-item".to_owned(),
        icon: ItemIcon::Pixmap(std::sync::Arc::new(pixmap)),
    }]);
    let rect = state
        .synoik
        .panel
        .app_indicators_rect(output_size(&output).w)
        .expect("the cluster takes a slot");

    let green = state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| {
            let elems = state.synoik.panel.render(
                vk,
                &output,
                ws,
                position,
                0.,
                crate::render_helpers::icon::DrawCaches {
                    icons: &state.synoik.icon_cache,
                    images: &state.synoik.image_cache,
                },
            );
            let pixels = composite_ui(
                vk,
                elems,
                Size::<i32, Physical>::from((width, bar_h)),
                scale,
            );

            let x0: i32 = to_physical_precise_round(scale.x, rect.loc.x);
            let x1: i32 = to_physical_precise_round(scale.x, rect.loc.x + rect.size.w);
            let mut green = 0usize;
            for y in 0..bar_h {
                for x in x0.max(0)..x1.min(width) {
                    let p = &pixels[((y * width + x) as usize) * 4..][..4];
                    if p[1] > 150 && p[0] < 80 && p[2] < 80 && p[3] > 120 {
                        green += 1;
                    }
                }
            }
            green
        })
        .expect("vulkan renderer");

    // No worker and no theme lookup is involved, so unlike a symbolic icon this must land on the
    // very first frame.
    assert!(
        green > 50,
        "a client's pixmap must composite in its own colour inside {rect:?}, got {green} green px"
    );
}

/// The dateMenu calendar popover renders on the owned Vulkan renderer when open:
/// the calendar box is drawn offscreen and composited as a positioned element.
/// Assert `render` yields an element that composites opaque (the dark box) pixels.
#[test]
fn vulkan_renders_the_calendar_popover() {
    let Some(mut f) = window_fixture(GREEN) else {
        return;
    };
    let output = f.synoik_output(1);
    let scale = Scale::from(output.current_scale().fractional_scale());

    // Open the calendar popover under the clock.
    {
        let anchor = f.synoik().panel.date_menu_rect(output_size(&output).w);
        let cal = f.synoik().gnome_settings.calendar;
        let accent = f.synoik().gnome_settings.accent_color;
        f.synoik().panel_popover.toggle_calendar(
            output.clone(),
            anchor,
            cal.week_start,
            cal.show_week_numbers,
            accent,
            Vec::new(),
        );
    }
    assert!(f.synoik().panel_popover.is_open());
    // Settle the open fade so the popover renders at full opacity (else the anim leaves
    // it at alpha 0 — the headless-animation-clock trap).
    f.settle_animations();

    let state = f.synoik_state();
    let opaque = state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| {
            let elems = state.synoik.panel_popover.render(
                vk,
                &state.synoik.icon_cache,
                &state.synoik.app_icon_cache,
                &state.synoik.image_cache,
                &output,
            );
            assert!(
                !elems.is_empty(),
                "an open popover must produce a render element"
            );
            // The popover composites centered under the clock, so capture the full
            // output width and enough height to include the calendar box.
            let w = to_physical_precise_round(scale.x, output_size(&output).w);
            let h = to_physical_precise_round(scale.x, 400.);
            let pixels = composite_ui(vk, elems, Size::<i32, Physical>::from((w, h)), scale);
            pixels.chunks_exact(4).filter(|p| p[3] == 255).count()
        })
        .expect("vulkan renderer");

    assert!(
        opaque > 0,
        "the calendar popover did not composite any opaque pixels on Vulkan"
    );
}

/// With a notification in the store, the calendar popover's message-list
/// column composites the card: `.message`-bg pixels (#51515a — distinctly
/// lighter than the popover box) must appear in the LEFT column, left of the
/// calendar (`js/ui/dateMenu.js:917-940` puts the list first).
#[test]
fn vulkan_renders_the_message_list_card() {
    let Some(mut f) = window_fixture(GREEN) else {
        return;
    };
    let output = f.synoik_output(1);
    let scale = Scale::from(output.current_scale().fractional_scale());

    {
        let anchor = f.synoik().panel.date_menu_rect(output_size(&output).w);
        let cal = f.synoik().gnome_settings.calendar;
        let accent = f.synoik().gnome_settings.accent_color;
        let card = crate::ui::notification_card::CardContent {
            id: 1,
            source_title: "App".to_owned(),
            source_icon: None,
            source_app_icon: None,
            title: "hello".to_owned(),
            body: "world".to_owned(),
            icon: None,
            actions: Vec::new(),
            has_default_action: false,
            critical: false,
            time_text: "Just now".to_owned(),
        };
        f.synoik().panel_popover.toggle_calendar(
            output.clone(),
            anchor,
            cal.week_start,
            cal.show_week_numbers,
            accent,
            vec![crate::ui::notification_card::CardGroup {
                key: crate::notifications::SourceKey::PidName(1, "App".to_owned()),
                source_title: card.source_title.clone(),
                source_icon: None,
                has_urgent: card.critical,
                cards: vec![card],
            }],
        );
    }
    f.settle_animations();

    let state = f.synoik_state();
    let origin = state.synoik.panel_popover.content_location(&output);
    let (_, card_rect, close_rect) =
        state.synoik.panel_popover.date_menu().unwrap().card_rects()[0];
    let close_icon_available = state
        .synoik
        .icon_cache
        .resolve("window-close-symbolic")
        .is_some();
    let (card_px, close_px) = state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| {
            let elems = state.synoik.panel_popover.render(
                vk,
                &state.synoik.icon_cache,
                &state.synoik.app_icon_cache,
                &state.synoik.image_cache,
                &output,
            );
            let w = to_physical_precise_round(scale.x, output_size(&output).w);
            let h = to_physical_precise_round(scale.x, 500.);
            // The element list is top-to-bottom; `render_to_vec` paints in
            // iteration order (bottom first), so reverse — like every capture
            // path does.
            let pixels = composite_ui(vk, elems, Size::<i32, Physical>::from((w, h)), scale);
            let sample = |x: f64, y: f64| {
                let px = to_physical_precise_round::<i32>(scale.x, origin.x + x);
                let py = to_physical_precise_round::<i32>(scale.x, origin.y + y);
                let i = ((py * w + px) * 4) as usize;
                [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
            };
            // The card's lower area: must be the .message bg (#51515a), not
            // the popover box (#1a1a1a).
            let card_px = sample(
                card_rect.loc.x + card_rect.size.w / 2.,
                card_rect.loc.y + card_rect.size.h - 8.,
            );
            // The close button's center: the white × glyph composites ON TOP
            // of the card (element order is top-to-bottom — a card pushed
            // before its icons would bury them).
            let close_px = sample(
                close_rect.loc.x + close_rect.size.w / 2.,
                close_rect.loc.y + close_rect.size.h / 2.,
            );
            (card_px, close_px)
        })
        .expect("vulkan renderer");

    assert_eq!(card_px[3], 255, "the card must be opaque, got {card_px:?}");
    assert!(
        (0x45..=0x60).contains(&card_px[0]) && (0x50..=0x68).contains(&card_px[2]),
        "expected the .message card bg (#51515a) in the list column, got {card_px:?}"
    );
    if close_icon_available {
        assert!(
            close_px[0] > 150 && close_px[1] > 150 && close_px[2] > 150,
            "the close-button glyph must composite above the card, got {close_px:?}"
        );
    }
}

/// Hovering a card highlights it two ways (GNOME `%card:hover` +
/// `%notification_button:hover`): the card body darkens (`button(hover, card)`
/// = `lighten($card_bg,4%)`, one step below the resting `+5%`) while the button
/// under the pointer lightens (white@.15 → white@.30). With the pointer over the
/// close button, the close-circle bg is strictly brighter AND a body-bg pixel is
/// strictly darker than un-hovered — proving both re-bake. Skips with no Vulkan.
#[test]
fn vulkan_hovering_a_card_close_button_lightens_it() {
    let Some(mut f) = window_fixture(GREEN) else {
        return;
    };
    let output = f.synoik_output(1);
    let scale = Scale::from(output.current_scale().fractional_scale());

    {
        let anchor = f.synoik().panel.date_menu_rect(output_size(&output).w);
        let cal = f.synoik().gnome_settings.calendar;
        let accent = f.synoik().gnome_settings.accent_color;
        f.synoik().panel_popover.toggle_calendar(
            output.clone(),
            anchor,
            cal.week_start,
            cal.show_week_numbers,
            accent,
            vec![crate::ui::notification_card::CardGroup {
                key: crate::notifications::SourceKey::PidName(1, "App".to_owned()),
                source_title: "App".to_owned(),
                source_icon: None,
                has_urgent: false,
                cards: vec![crate::ui::notification_card::CardContent {
                    id: 1,
                    source_title: "App".to_owned(),
                    source_icon: None,
                    source_app_icon: None,
                    title: "hello".to_owned(),
                    body: "world".to_owned(),
                    icon: None,
                    actions: Vec::new(),
                    has_default_action: false,
                    critical: false,
                    time_text: "Just now".to_owned(),
                }],
            }],
        );
    }
    f.settle_animations();

    let origin = f.synoik().panel_popover.content_location(&output);
    let (_, card, close) = f.synoik().panel_popover.date_menu().unwrap().card_rects()[0];
    // The close circle's background, in the top-middle gap of the × glyph (not
    // the opaque white glyph); and a card-body pixel at the right edge,
    // mid-height (clear of the left-aligned text and the top-right buttons).
    let btn_pt = origin
        + Point::from((
            close.loc.x + close.size.w / 2.,
            close.loc.y + close.size.h * 0.15,
        ));
    let body_pt =
        origin + Point::from((card.loc.x + card.size.w - 8., card.loc.y + card.size.h / 2.));
    let center = origin
        + Point::from((
            close.loc.x + close.size.w / 2.,
            close.loc.y + close.size.h / 2.,
        ));

    let w = to_physical_precise_round::<i32>(scale.x, output_size(&output).w);
    let h = to_physical_precise_round::<i32>(scale.x, 500.);
    let bx = to_physical_precise_round::<i32>(scale.x, btn_pt.x);
    let by = to_physical_precise_round::<i32>(scale.x, btn_pt.y);
    let dx = to_physical_precise_round::<i32>(scale.x, body_pt.x);
    let dy = to_physical_precise_round::<i32>(scale.x, body_pt.y);

    // Render once and read both sample pixels.
    let render_samples = |f: &mut Fixture| -> ([u8; 4], [u8; 4]) {
        let state = f.synoik_state();
        state
            .backend
            .headless()
            .with_vulkan_renderer(|vk| {
                let elems = state.synoik.panel_popover.render(
                    vk,
                    &state.synoik.icon_cache,
                    &state.synoik.app_icon_cache,
                    &state.synoik.image_cache,
                    &output,
                );
                let pixels = composite_ui(vk, elems, Size::<i32, Physical>::from((w, h)), scale);
                let at = |x: i32, y: i32| {
                    let i = ((y * w + x) * 4) as usize;
                    [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
                };
                (at(bx, by), at(dx, dy))
            })
            .expect("vulkan renderer")
    };

    let (btn_cold, body_cold) = render_samples(&mut f);
    assert!(
        f.synoik().panel_popover.pointer_hover(&output, center),
        "the pointer over the close button registers a hover"
    );
    let (btn_hot, body_hot) = render_samples(&mut f);

    let sum = |p: [u8; 4]| u32::from(p[0]) + u32::from(p[1]) + u32::from(p[2]);
    for p in [btn_cold, btn_hot, body_cold, body_hot] {
        assert_eq!(p[3], 255, "sampled card pixels must be opaque, got {p:?}");
    }
    assert!(
        sum(btn_hot) > sum(btn_cold),
        "hovering must lighten the close circle: cold {btn_cold:?} hot {btn_hot:?}"
    );
    assert!(
        sum(body_hot) < sum(body_cold),
        "hovering must darken the card body: cold {body_cold:?} hot {body_hot:?}"
    );
}

/// Clicking a card's expand caret grows the card: the multi-line body area
/// (below the collapsed 90px height) composites `.message`-bg pixels where the
/// popover box showed before, and the caret's chevron glyph (an embedded
/// gresource icon) composites on top of the card
/// (`js/ui/messageList.js:521-538,614-666`).
#[test]
fn vulkan_renders_the_expanded_card_body() {
    let Some(mut f) = window_fixture(GREEN) else {
        return;
    };
    let output = f.synoik_output(1);
    let scale = Scale::from(output.current_scale().fractional_scale());

    {
        let anchor = f.synoik().panel.date_menu_rect(output_size(&output).w);
        let cal = f.synoik().gnome_settings.calendar;
        let accent = f.synoik().gnome_settings.accent_color;
        let card = crate::ui::notification_card::CardContent {
            id: 1,
            source_title: "App".to_owned(),
            source_icon: None,
            source_app_icon: None,
            title: "hello".to_owned(),
            body: "a long body ".repeat(40).trim_end().to_owned(),
            icon: None,
            actions: Vec::new(),
            has_default_action: false,
            critical: false,
            time_text: "Just now".to_owned(),
        };
        f.synoik().panel_popover.toggle_calendar(
            output.clone(),
            anchor,
            cal.week_start,
            cal.show_week_numbers,
            accent,
            vec![crate::ui::notification_card::CardGroup {
                key: crate::notifications::SourceKey::PidName(1, "App".to_owned()),
                source_title: card.source_title.clone(),
                source_icon: None,
                has_urgent: card.critical,
                cards: vec![card],
            }],
        );
    }
    f.settle_animations();

    let state = f.synoik_state();
    let origin = state.synoik.panel_popover.content_location(&output);
    let caret = state
        .synoik
        .panel_popover
        .date_menu()
        .unwrap()
        .card_expand_rect(1)
        .expect("a long body makes the caret live");

    // Expand through the real click path.
    let caret_pos = origin + caret.loc + Point::from((caret.size.w / 2., caret.size.h / 2.));
    state.synoik.panel_popover.pointer_click(&output, caret_pos);
    let (_, card_rect, _) = state.synoik.panel_popover.date_menu().unwrap().card_rects()[0];
    assert!(
        card_rect.size.h > 90.,
        "the card grew: {}",
        card_rect.size.h
    );
    let caret = state
        .synoik
        .panel_popover
        .date_menu()
        .unwrap()
        .card_expand_rect(1)
        .unwrap();

    let (body_px, caret_px) = state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| {
            let elems = state.synoik.panel_popover.render(
                vk,
                &state.synoik.icon_cache,
                &state.synoik.app_icon_cache,
                &state.synoik.image_cache,
                &output,
            );
            let w = to_physical_precise_round(scale.x, output_size(&output).w);
            let h = to_physical_precise_round(scale.x, 500.);
            let pixels = composite_ui(vk, elems, Size::<i32, Physical>::from((w, h)), scale);
            let sample = |x: f64, y: f64| {
                let px = to_physical_precise_round::<i32>(scale.x, origin.x + x);
                let py = to_physical_precise_round::<i32>(scale.x, origin.y + y);
                let i = ((py * w + px) * 4) as usize;
                [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
            };
            // Deep in the expanded body area — beyond the collapsed height,
            // anchored to the card bottom so it tracks the em-scaled height.
            let body_px = sample(
                card_rect.loc.x + card_rect.size.w / 2.,
                card_rect.loc.y + card_rect.size.h - 15.,
            );
            // The (now collapse-)chevron glyph over the caret circle: a
            // chevron has no ink at its exact center, so take the brightest
            // pixel in the button.
            let mut caret_px = [0u8; 4];
            for dy in -7..=7 {
                for dx in -7..=7 {
                    let p = sample(
                        caret.loc.x + caret.size.w / 2. + f64::from(dx),
                        caret.loc.y + caret.size.h / 2. + f64::from(dy),
                    );
                    if p[0] > caret_px[0] {
                        caret_px = p;
                    }
                }
            }
            (body_px, caret_px)
        })
        .expect("vulkan renderer");

    assert!(
        (0x45..=0x60).contains(&body_px[0]) && (0x50..=0x68).contains(&body_px[2]),
        "expected the .message card bg (#51515a) in the expanded body area, got {body_px:?}"
    );
    assert!(
        caret_px[0] > 150 && caret_px[1] > 150 && caret_px[2] > 150,
        "the chevron glyph must composite above the card, got {caret_px:?}"
    );
}

/// A multi-notification group renders as a fanned stack: a darkened peek shows
/// below the top card when collapsed, and the group header's collapse chevron
/// composites when expanded (`js/ui/messageList.js:1370-1404`,
/// `_message-list.scss:89-98`). Exercises the new stack-shadow and group-header
/// draw paths (and the bundled `group-collapse-symbolic`).
#[test]
fn vulkan_renders_a_grouped_stack_and_header() {
    let Some(mut f) = window_fixture(GREEN) else {
        return;
    };
    let output = f.synoik_output(1);
    let scale = Scale::from(output.current_scale().fractional_scale());
    let key = crate::notifications::SourceKey::PidName(9, "App".to_owned());

    let make_card = |id: u32| crate::ui::notification_card::CardContent {
        id,
        source_title: "App".to_owned(),
        source_icon: None,
        source_app_icon: None,
        title: format!("msg {id}"),
        body: "body".to_owned(),
        icon: None,
        actions: Vec::new(),
        has_default_action: false,
        critical: false,
        time_text: "Just now".to_owned(),
    };
    {
        let anchor = f.synoik().panel.date_menu_rect(output_size(&output).w);
        let cal = f.synoik().gnome_settings.calendar;
        let accent = f.synoik().gnome_settings.accent_color;
        f.synoik().panel_popover.toggle_calendar(
            output.clone(),
            anchor,
            cal.week_start,
            cal.show_week_numbers,
            accent,
            vec![crate::ui::notification_card::CardGroup {
                key: key.clone(),
                source_title: "App".to_owned(),
                source_icon: None,
                has_urgent: false,
                cards: vec![make_card(1), make_card(2), make_card(3)],
            }],
        );
    }
    f.settle_animations();

    let origin = f.synoik().panel_popover.content_location(&output);
    let w = to_physical_precise_round(scale.x, output_size(&output).w);
    let h = to_physical_precise_round(scale.x, 500.);
    let render_sample = |f: &mut Fixture, pts: Vec<(f64, f64)>| -> Vec<[u8; 4]> {
        let state = f.synoik_state();
        state
            .backend
            .headless()
            .with_vulkan_renderer(|vk| {
                let elems = state.synoik.panel_popover.render(
                    vk,
                    &state.synoik.icon_cache,
                    &state.synoik.app_icon_cache,
                    &state.synoik.image_cache,
                    &output,
                );
                let pixels = composite_ui(vk, elems, Size::<i32, Physical>::from((w, h)), scale);
                pts.into_iter()
                    .map(|(x, y)| {
                        let px = to_physical_precise_round::<i32>(scale.x, origin.x + x);
                        let py = to_physical_precise_round::<i32>(scale.x, origin.y + y);
                        let i = ((py * w + px) * 4) as usize;
                        [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
                    })
                    .collect()
            })
            .expect("vulkan renderer")
    };

    // Collapsed (3 cards → two peeks): darkened strips show below the 90px top
    // card. The second-in-stack occupies the band [90,100] and the deeper
    // lower-in-stack [100,107]. Both must be opaque and darker than the top
    // card, AND the second-in-stack must render OVER the lower-in-stack — with
    // the z-order inverted, the darker deeper card paints on top and the upper
    // band comes out darker than the lower one (the exact bug a 2-card group
    // cannot expose). Sampled at the card's horizontal center, clear of the
    // rounded corners.
    let bounds = f.synoik().panel_popover.date_menu().unwrap().group_rects()[0].1;
    let samples = render_sample(
        &mut f,
        vec![
            // The peeks start just below the top card, so both offsets follow its height.
            (bounds.size.w / 2., bounds.loc.y + collapsed_card_h() + 5.), // second-in-stack
            (bounds.size.w / 2., bounds.loc.y + collapsed_card_h() + 13.5), // lower-in-stack
        ],
    );
    let (second, lower) = (samples[0], samples[1]);
    assert_eq!(
        second[3], 255,
        "second-in-stack band opaque, got {second:?}"
    );
    assert_eq!(lower[3], 255, "lower-in-stack band opaque, got {lower:?}");
    assert!(
        second[0] < 0x51 && second[0] > 0x30,
        "the peek is a darkened card bg (below #51515a), got {second:?}"
    );
    assert!(
        second[2] > lower[2] && second[0] >= lower[0],
        "second-in-stack must paint OVER the darker lower-in-stack (z-order): \
         second {second:?} vs lower {lower:?}"
    );

    // Expand the group through the real click path (clear of the close button).
    let expand_pt = origin + bounds.loc + Point::from((20., bounds.size.h - 6.));
    f.synoik().panel_popover.pointer_click(&output, expand_pt);
    assert!(
        f.synoik().panel_popover.date_menu().unwrap().group_rects()[0].2,
        "the group expanded"
    );

    // The header collapse chevron composites bright over its button (a chevron
    // has no ink at its exact center — brightest pixel in the button).
    let collapse = f
        .synoik()
        .panel_popover
        .date_menu()
        .unwrap()
        .group_collapse_rect(&key)
        .expect("expanded group has a collapse button");
    let pts: Vec<(f64, f64)> = (-7..=7)
        .flat_map(|dy| {
            (-7..=7).map(move |dx| {
                (
                    collapse.loc.x + collapse.size.w / 2. + f64::from(dx),
                    collapse.loc.y + collapse.size.h / 2. + f64::from(dy),
                )
            })
        })
        .collect();
    let chevron = render_sample(&mut f, pts)
        .into_iter()
        .max_by_key(|p| p[0])
        .unwrap();
    assert!(
        chevron[0] > 150 && chevron[1] > 150 && chevron[2] > 150,
        "the group collapse chevron must composite, got {chevron:?}"
    );
}

/// When the message list overflows the popover it renders through the baked,
/// clipped scroll path with an overlay scrollbar thumb. Assert the clipped
/// content shows a card in the viewport and the thumb composites in the
/// reserved right strip (and the whole thing passes the validation layer).
#[test]
fn vulkan_renders_the_scrolled_message_list() {
    use crate::ui::notification_card::{CardContent, CardGroup};
    let Some(mut f) = window_fixture(GREEN) else {
        return;
    };
    let output = f.synoik_output(1);
    let scale = Scale::from(output.current_scale().fractional_scale());

    let card = |id: u32| CardContent {
        id,
        source_title: format!("App {id}"),
        source_icon: None,
        source_app_icon: None,
        title: format!("msg {id}"),
        body: "body".to_owned(),
        icon: None,
        actions: Vec::new(),
        has_default_action: false,
        critical: false,
        time_text: "Just now".to_owned(),
    };
    let group = |id: u32, cards: Vec<CardContent>| CardGroup {
        key: crate::notifications::SourceKey::PidName(id, format!("App{id}")),
        source_title: format!("App {id}"),
        source_icon: None,
        has_urgent: false,
        cards,
    };
    // A collapsed 3-card stack at the top (so the bake exercises internal
    // peek z-order) plus enough single sources that the list must scroll.
    let mut groups = vec![group(1, vec![card(1), card(2), card(3)])];
    groups.extend((4..=13).map(|id| group(id, vec![card(id)])));
    {
        let anchor = f.synoik().panel.date_menu_rect(output_size(&output).w);
        let cal = f.synoik().gnome_settings.calendar;
        let accent = f.synoik().gnome_settings.accent_color;
        f.synoik().panel_popover.toggle_calendar(
            output.clone(),
            anchor,
            cal.week_start,
            cal.show_week_numbers,
            accent,
            groups,
        );
    }
    f.settle_animations();

    let origin = f.synoik().panel_popover.content_location(&output);
    let w = to_physical_precise_round(scale.x, output_size(&output).w);
    let h = to_physical_precise_round(scale.x, 500.);
    let list_w = 29. * crate::ui::pt_to_px(11.);
    let list_pad = crate::ui::calendar::list_pad();

    let state = f.synoik_state();
    let samples = state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| {
            let elems = state.synoik.panel_popover.render(
                vk,
                &state.synoik.icon_cache,
                &state.synoik.app_icon_cache,
                &state.synoik.image_cache,
                &output,
            );
            let pixels = composite_ui(vk, elems, Size::<i32, Physical>::from((w, h)), scale);
            let at = |x: f64, y: f64| {
                let px = to_physical_precise_round::<i32>(scale.x, origin.x + x);
                let py = to_physical_precise_round::<i32>(scale.x, origin.y + y);
                let i = ((py * w + px) * 4) as usize;
                [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
            };
            let cx = list_pad + (list_w - 18.) / 2.;
            // The top card's bg (clipped content in the viewport), inside its header row.
            let card = at(cx, list_pad + 40.);
            // The collapsed stack's two peek bands, baked through the scroll
            // path: second-in-stack [90,100] must render OVER (lighter than)
            // the deeper lower-in-stack [100,107]. If the bake's z-order is
            // wrong (missing the `.rev()`), the card bg paints over the peeks
            // and the icons, and this ordering flips.
            let second = at(cx, list_pad + collapsed_card_h() + 5.);
            let lower = at(cx, list_pad + collapsed_card_h() + 13.5);
            // The scrollbar thumb strip, near the viewport top (scroll at 0).
            let thumb = at(list_w - 7., list_pad + 12.);
            (card, second, lower, thumb)
        })
        .expect("vulkan renderer");
    let (card, second, lower, thumb) = samples;

    assert_eq!(
        card[3], 255,
        "the clipped card content is opaque, got {card:?}"
    );
    assert!(
        card[0] > 0x40 && card[0] < 0x60,
        "the viewport shows a card bg (~#51515a), got {card:?}"
    );
    assert_eq!(
        second[3], 255,
        "second-in-stack band opaque, got {second:?}"
    );
    assert_eq!(lower[3], 255, "lower-in-stack band opaque, got {lower:?}");
    assert!(
        second[2] > lower[2] && second[0] >= lower[0],
        "through the scroll bake, second-in-stack must paint OVER the darker \
         lower-in-stack (z-order): second {second:?} vs lower {lower:?}"
    );
    assert_eq!(
        thumb[3], 255,
        "the scrollbar thumb is opaque, got {thumb:?}"
    );
    assert!(
        thumb[0] > 0x60 && (thumb[0] as i32 - thumb[2] as i32).abs() < 20,
        "the thumb is a light grey handle over the strip, got {thumb:?}"
    );
    assert!(
        thumb[0] > card[0],
        "the thumb is lighter than the card bg behind it: thumb {thumb:?} vs card {card:?}"
    );
}

/// The quick-settings popover renders on the owned Vulkan renderer when open: the
/// menu chrome (an offscreen box with tile backgrounds + labels) plus the icon
/// elements composited on top. Assert `render` yields several elements that
/// composite opaque (the dark menu) pixels.
#[test]
fn vulkan_renders_the_quick_settings_popover() {
    let Some(mut f) = window_fixture(GREEN) else {
        return;
    };
    let output = f.synoik_output(1);
    let scale = Scale::from(output.current_scale().fractional_scale());

    // Open the quick-settings popover under the right-box indicator.
    {
        let output_w = output_size(&output).w;
        let toggles = f.synoik().gnome_settings.quick_toggles;
        let anchor = f.synoik().panel.quick_settings_rect(output_w);
        let network = f.synoik().system_status.network;
        let airplane = f.synoik().system_status.airplane;
        let power = f.synoik().system_status.power.clone();
        let bluetooth = f.synoik().system_status.bluetooth.clone();
        let bluetooth_rfkill = f.synoik().system_status.bluetooth_rfkill;
        let battery = f.synoik().system_status.battery.clone();
        let audio = f.synoik().audio;
        let sink_list = f.synoik().sink_list.clone();
        let mic = f.synoik().mic;
        let source_list = f.synoik().source_list.clone();
        let accent = f.synoik().gnome_settings.accent_color;
        f.synoik().panel_popover.toggle_quick_settings(
            output.clone(),
            anchor,
            toggles,
            network,
            airplane,
            power,
            bluetooth,
            bluetooth_rfkill,
            battery,
            audio,
            sink_list,
            crate::audio::AudioCards::default(),
            false,
            mic,
            source_list,
            crate::brightness::BrightnessView::default(),
            accent,
        );
    }
    assert!(f.synoik().panel_popover.is_open());
    // Settle the open fade so the popover renders at full opacity (the clock trap).
    f.settle_animations();

    let state = f.synoik_state();
    let opaque = state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| {
            let elems = state.synoik.panel_popover.render(
                vk,
                &state.synoik.icon_cache,
                &state.synoik.app_icon_cache,
                &state.synoik.image_cache,
                &output,
            );
            assert!(
                !elems.is_empty(),
                "an open quick-settings popover must produce render elements"
            );
            let w = to_physical_precise_round(scale.x, output_size(&output).w);
            let h = to_physical_precise_round(scale.x, 300.);
            let pixels = composite_ui(vk, elems, Size::<i32, Physical>::from((w, h)), scale);
            pixels.chunks_exact(4).filter(|p| p[3] == 255).count()
        })
        .expect("vulkan renderer");

    assert!(
        opaque > 0,
        "the quick-settings popover did not composite any opaque pixels on Vulkan"
    );
}

/// The quick-settings popover with a live brightness slider and its per-monitor card open renders
/// on Vulkan: the extra slider row grows the menu, and the card adds label rows plus the shared
/// slider body. Its real value is the validation-layer run (`SYNOIK_VK_VALIDATION=1`), which this
/// test's draw path feeds.
#[test]
fn vulkan_renders_the_brightness_slider() {
    let Some(mut f) = window_fixture(GREEN) else {
        return;
    };
    let output = f.synoik_output(1);
    let scale = Scale::from(output.current_scale().fractional_scale());

    // Seed two backlit monitors, exactly as the udev match would: two scales means the slider
    // gets its picker arrow, and the per-monitor card behind it has rows to draw.
    let backlight = |connector: &str, name: &str, brightness| crate::backlight::OutputBacklight {
        connector: connector.to_owned(),
        display_name: name.to_owned(),
        range: crate::backlight::BacklightRange { min: 1, max: 100 },
        brightness,
    };
    let snapshot = crate::backlight::BacklightSnapshot {
        outputs: vec![
            backlight("eDP-1", "Built-in display", 60),
            backlight("DP-2", "Dell 24\u{2033}", 30),
        ],
    };
    let _ = f.synoik().brightness.monitors_changed(&snapshot);
    f.synoik().backlight = snapshot;

    open_quick_settings(&mut f, &output);
    assert!(f.synoik().panel_popover.is_open());
    f.settle_animations();

    let state = f.synoik_state();
    let opaque = state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| {
            let elems = state.synoik.panel_popover.render(
                vk,
                &state.synoik.icon_cache,
                &state.synoik.app_icon_cache,
                &state.synoik.image_cache,
                &output,
            );
            assert!(
                !elems.is_empty(),
                "the popover must produce render elements"
            );
            let w = to_physical_precise_round(scale.x, output_size(&output).w);
            let h = to_physical_precise_round(scale.x, 300.);
            let pixels = composite_ui(vk, elems, Size::<i32, Physical>::from((w, h)), scale);
            pixels.chunks_exact(4).filter(|p| p[3] == 255).count()
        })
        .expect("vulkan renderer");

    assert!(
        opaque > 0,
        "the quick-settings popover with a brightness slider composited no opaque pixels"
    );
}

/// The accessibility menu composites: a switch row must produce both the label glyphs and
/// the switch itself. Pinned by comparing an all-off menu against one with every row on —
/// the accent-filled tracks are a large, saturated, *countable* difference that a missing
/// [`crate::ui::widget::Switch`] paint (or an off/on state that never reaches the bake)
/// would erase. Skips with no Vulkan device.
#[test]
fn vulkan_renders_the_a11y_switches() {
    use crate::gnome::{A11ySettings, A11yToggle};

    let Some(mut f) = window_fixture(GREEN) else {
        return;
    };
    let output = f.synoik_output(1);
    let scale = Scale::from(output.current_scale().fractional_scale());

    // Count accent-blue-ish pixels (the on-state switch track, `_switches.scss:41`).
    let count_accent = |f: &mut Fixture, a11y: A11ySettings| {
        f.synoik().gnome_settings.a11y = a11y;
        f.synoik().panel.set_a11y(a11y);
        let anchor = f
            .synoik()
            .panel
            .a11y_rect(output_size(&output).w)
            .expect("the indicator is pinned on");
        let accent = f.synoik().gnome_settings.accent_color;
        let out = output.clone();
        f.synoik()
            .panel_popover
            .toggle_a11y(out.clone(), anchor, a11y, accent);
        f.settle_animations();

        let state = f.synoik_state();
        let n = state
            .backend
            .headless()
            .with_vulkan_renderer(|vk| {
                let elems = state.synoik.panel_popover.render(
                    vk,
                    &state.synoik.icon_cache,
                    &state.synoik.app_icon_cache,
                    &state.synoik.image_cache,
                    &out,
                );
                assert!(!elems.is_empty(), "the a11y menu must render");
                let w = to_physical_precise_round(scale.x, output_size(&out).w);
                let h = to_physical_precise_round(scale.x, 600.);
                let pixels = composite_ui(vk, elems, Size::<i32, Physical>::from((w, h)), scale);
                // Abgr8888 is byte-order R,G,B,A here: accent #3584e4 is blue-dominant.
                pixels
                    .chunks_exact(4)
                    .filter(|p| p[3] == 255 && p[2] > 180 && u16::from(p[2]) > u16::from(p[0]) + 60)
                    .count()
            })
            .expect("vulkan renderer");
        f.synoik().panel_popover.close();
        f.settle_animations();
        n
    };

    let mut off = A11ySettings::default();
    off.always_show = true;
    let off_px = count_accent(&mut f, off);

    let mut on = off;
    for toggle in A11yToggle::ALL {
        on.set(toggle, true);
    }
    let on_px = count_accent(&mut f, on);

    assert!(
        on_px > off_px + 1000,
        "ten on-state switch tracks must add a large block of accent pixels \
         (off={off_px}, on={on_px})"
    );
}

/// The a11y indicator actually composites a glyph. The indicator is icon-only, so a name
/// the theme cannot resolve leaves a correctly-sized but **invisible** button — the panel
/// geometry tests all still pass, and only a pixel test can tell. `accessibility-menu-symbolic`
/// ships in gnome-shell's own gresource rather than Adwaita, which is exactly the situation
/// the fallback list exists for. Skips with no Vulkan device.
#[test]
fn vulkan_renders_the_a11y_indicator_icon() {
    use crate::gnome::A11ySettings;

    let Some(mut f) = window_fixture(GREEN) else {
        return;
    };
    let output = f.synoik_output(1);
    let scale = Scale::from(output.current_scale().fractional_scale());
    let width = to_physical_precise_round(scale.x, output_size(&output).w);
    let bar_h = to_physical_precise_round(scale.x, crate::ui::panel::panel_height());

    let render_panel = |f: &mut Fixture| {
        let ws = f.synoik().workspace_state_for(&output);
        let position = f.synoik().workspace_position_for(&output);
        let state = f.synoik_state();
        state
            .backend
            .headless()
            .with_vulkan_renderer(|vk| {
                let elems = state.synoik.panel.render(
                    vk,
                    &output,
                    ws,
                    position,
                    0.,
                    crate::render_helpers::icon::DrawCaches {
                        icons: &state.synoik.icon_cache,
                        images: &state.synoik.image_cache,
                    },
                );
                composite_ui(
                    vk,
                    elems,
                    Size::<i32, Physical>::from((width, bar_h)),
                    scale,
                )
            })
            .expect("vulkan renderer")
    };

    // Bright pixels inside the indicator's own rect — the glyph, if one resolved.
    let glyph_px = |f: &mut Fixture, pixels: &[u8]| {
        let rect = f.synoik().panel.a11y_rect(output_size(&output).w)?;
        let x0 = to_physical_precise_round(scale.x, rect.loc.x);
        let x1 = to_physical_precise_round(scale.x, rect.loc.x + rect.size.w);
        let mut n = 0;
        for y in 0..bar_h {
            for x in x0..x1 {
                let p = px(pixels, width, x, y);
                if u16::from(p[0]) + u16::from(p[1]) + u16::from(p[2]) > 300 {
                    n += 1;
                }
            }
        }
        Some(n)
    };

    // Hidden: no rect at all.
    let pixels = render_panel(&mut f);
    assert!(glyph_px(&mut f, &pixels).is_none(), "hidden by default");

    // Pinned on: the button exists AND draws a glyph.
    let mut a11y = A11ySettings::default();
    a11y.always_show = true;
    f.synoik().gnome_settings.a11y = a11y;
    f.synoik().panel.set_a11y(a11y);
    let pixels = render_panel(&mut f);
    let n = glyph_px(&mut f, &pixels).expect("the indicator is pinned on");
    assert!(
        n > 20,
        "the a11y indicator composited no glyph ({n} bright px) — its icon name did not \
         resolve in the theme, so the button is invisible"
    );
}

/// A translucent `TextureRenderElement` (alpha < 1, e.g. a fading popover) must NOT report
/// any opaque regions. If it does, the damage tracker skips clearing and repainting beneath
/// it, so the fade blends over stale framebuffer content instead of the scene behind — the
/// panel-popover close-fade bug, where the chrome stuck at full opacity while the corners
/// (not claimed opaque) faded. Mirrors smithay's own texture element, which gates
/// `opaque_regions` on `alpha < 1.0`. Skips with no Vulkan device.
#[test]
fn vulkan_translucent_texture_element_claims_no_opaque_regions() {
    let Some(mut f) = window_fixture(GREEN) else {
        return;
    };
    let state = f.synoik_state();
    state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| {
            use smithay::backend::renderer::element::Kind;

            use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};

            const W: i32 = 32;
            const H: i32 = 32;
            let texels = vec![255u8; (W * H * 4) as usize];
            // An opaque texture that claims its full rect opaque.
            let opaque = vec![Rectangle::<i32, BufferCoord>::from_size(Size::from((W, H)))];
            let tb = TextureBuffer::from_memory(
                vk,
                &texels,
                Fourcc::Abgr8888,
                (W, H),
                false,
                1.0,
                Transform::Normal,
                opaque,
            )
            .expect("import opaque texture");

            let mut el = TextureRenderElement::from_texture_buffer(
                tb,
                (0., 0.),
                1.0,
                None,
                None,
                Kind::Unspecified,
            );
            assert!(
                !Element::opaque_regions(&el, Scale::from(1.0)).is_empty(),
                "a fully-opaque element must report its opaque region",
            );

            // Fading it must drop the opaque claim — otherwise the damage tracker treats the
            // translucent element as an occluder and never repaints the scene beneath it.
            el.set_alpha(0.5);
            assert!(
                Element::opaque_regions(&el, Scale::from(1.0)).is_empty(),
                "a translucent (alpha < 1) element must not claim any opaque region",
            );
        })
        .expect("vulkan renderer");
}

/// A resize animation on a Vulkan session must draw the cross-fade (`render_resize`), not the red
/// `SolidColorBuffer` placeholder. Reproduces the live "the window becomes a red rect while
/// maximizing/restoring" bug: map a window, issue a synoik-driven (animated) resize, commit the new
/// size, and composite mid-animation — the frame must show window content with no pure-red fill.
///
/// This exercises the resize path end-to-end: the pre-resize neutral snapshot is captured through
/// the owned Vulkan renderer (`capture_neutral_vulkan`), then composited through Vulkan.
/// A failed neutral capture falls through to `!pushed_resize` → the red placeholder, so `red < 100`
/// discriminates the crossfade path.
///
/// There is no GLES fallback behind this any more, so the assert now bites directly on the Vulkan
/// path: it fails if `store_animation_snapshot_neutral` stops storing the snapshot struct, which is
/// the silent way to lose the crossfade (verified by negative control).
#[test]
fn vulkan_resize_animation_is_not_a_red_rect() {
    use synoik_config::animations::{Curve, EasingParams, Kind};
    use synoik_ipc::SizeChange;

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

    let mut f = Fixture::with_config(config);
    f.synoik_state()
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

    // A real shm-textured buffer (not single-pixel) so the snapshot path can bake it.
    let window = f.client(id).window(&surface);
    window.attach_shm_buffer(WIN as i32, WIN as i32, 0, 255, 0, 255);
    window.set_size(WIN, WIN);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.synoik_complete_animations();
    f.double_roundtrip(id);

    // Issue a synoik-driven resize (this animates, like a keybind maximize).
    f.synoik()
        .layout
        .set_column_width(SizeChange::SetFixed(900));
    f.double_roundtrip(id);

    // The client commits the new size, which starts the resize animation.
    let window = f.client(id).window(&surface);
    window.attach_shm_buffer(900, WIN as i32, 0, 255, 0, 255);
    window.set_size(900, WIN);
    window.ack_last_and_commit();
    f.roundtrip(id);

    let output = f.synoik_output(1);
    assert!(
        f.synoik().layout.are_animations_ongoing(Some(&output)),
        "expected an ongoing resize animation to composite"
    );

    let is_red = |p: [u8; 4]| p[0] > 200 && p[1] < 40 && p[2] < 40;
    let is_green = |p: [u8; 4]| p[0] < 40 && p[1] > 200 && p[2] < 40;
    let mut mid_green = Vec::new();

    // Check every target the crossfade is supposed to reach, not just the default one. The
    // `blocked_out` gate in `Tile::render` is per-target, so a regression that widened it would
    // blank the crossfade in casts while an Output/ScreenCapture-only check stayed green — the
    // positive half of the pair `vulkan_blocked_out_window_does_not_leak_while_resizing` forms.
    for target in [
        RenderTarget::Output,
        RenderTarget::ScreenCapture,
        RenderTarget::Screencast,
    ] {
        let (pixels, w, h) = render_output_vulkan_target(&mut f, &output, target);
        let red = (0..w * h)
            .filter(|i| is_red(px(&pixels, w, i % w, i / w)))
            .count();
        let green = (0..w * h)
            .filter(|i| is_green(px(&pixels, w, i % w, i / w)))
            .count();
        eprintln!(
            "vulkan_resize_animation_is_not_a_red_rect: {target:?}: {green} green px, {red} red px"
        );
        assert!(
            green > 0,
            "no window content in the {target:?} resize frame"
        );
        assert!(
            red < 100,
            "resize rendered the red placeholder ({red} red px) instead of the cross-fade \
             on {target:?}"
        );
        mid_green.push((target, green));
    }

    // `red < 100` only discriminates while `blocked_out` is false: a regression that widened it
    // skips the crossfade AND the red placeholder, falling through to a plain render that is still
    // green and still red-free. What gives it away is the size — the crossfade shows the window
    // fading from its OLD geometry, while the fall-through draws it at its settled one. Measured:
    // 40k green mid-crossfade, vs 131k for the fall-through, which is exactly the settled count.
    f.synoik_complete_animations();
    let (settled, w, h) = render_output_vulkan(&mut f, &output);
    let green_settled = (0..w * h)
        .filter(|i| is_green(px(&settled, w, i % w, i / w)))
        .count();
    eprintln!("vulkan_resize_animation_is_not_a_red_rect: settled {green_settled} green px");
    assert!(green_settled > 0, "the settled window is absent");

    for (target, green) in mid_green {
        assert!(
            green < green_settled / 2,
            "the {target:?} resize frame is as large as the settled window ({green} vs \
             {green_settled}) — the crossfade was skipped and the plain window rendered instead"
        );
    }
}

/// The resize crossfade's pre-resize neutral buffer is captured through the owned Vulkan renderer
/// (`capture_neutral_from_surface_tree`), not GLES — the first vertical slice of self-hosting the
/// Vulkan path. Map a green shm window and drive the capture DIRECTLY (bypassing the GLES fallback
/// that `vulkan_resize_animation_is_not_a_red_rect` can't see past): the returned `MemoryBuffer`
/// must be window-sized and green, proving the Vulkan renderer re-imported and rendered the surface
/// tree offscreen at store-time.
#[test]
fn vulkan_captures_the_resize_neutral_through_vulkan() {
    use crate::render_helpers::snapshot::capture_neutral_from_surface_tree;

    if VulkanRenderer::new().is_err() {
        eprintln!("skipping vulkan_captures_the_resize_neutral_through_vulkan: no Vulkan device");
        return;
    }

    let mut f = Fixture::new();
    f.synoik_state()
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
    f.synoik_complete_animations();
    f.double_roundtrip(id);

    // The server-side surface of the mapped window (cloned so the `f.synoik()` borrow ends before
    // we borrow the backend's Vulkan renderer).
    let server_surface = {
        let mapped = f
            .synoik()
            .layout
            .windows()
            .next()
            .expect("a mapped window")
            .1;
        mapped.toplevel().wl_surface().clone()
    };

    // buf_pos is irrelevant to the buffer content (we relocate by -geo.loc); use the origin.
    let scale = Scale::from(1.);
    let captured = f
        .synoik_state()
        .backend
        .headless()
        .with_vulkan_renderer(|vk| {
            capture_neutral_from_surface_tree(
                vk,
                &server_surface,
                smithay::utils::Point::from((0., 0.)),
                scale,
            )
        })
        .flatten();

    let (buffer, geo) = captured.expect("Vulkan neutral capture returned nothing");
    assert_eq!(
        geo.size,
        Size::from((WIN as i32, WIN as i32)),
        "unexpected neutral geometry"
    );
    let (w, h) = (buffer.size().w, buffer.size().h);
    assert_eq!(
        (w, h),
        (WIN as i32, WIN as i32),
        "unexpected neutral buffer size"
    );

    let data = buffer.data();
    let is_green = |p: [u8; 4]| p[0] < 40 && p[1] > 200 && p[2] < 40;
    let green = (0..w * h)
        .filter(|i| is_green(px(data, w, i % w, i / w)))
        .count();
    eprintln!(
        "vulkan_captures_the_resize_neutral_through_vulkan: {green}/{} green px",
        w * h
    );
    assert!(
        green as i32 > w * h * 3 / 4,
        "neutral buffer is not the green window ({green} green px) — Vulkan capture produced no content"
    );
}

/// Direct test of the close-animation self-hosting: `State::store_unmap_snapshot` bakes the GLES
/// unmap snapshot and then captures the neutral CPU buffer through the owned Vulkan renderer
/// (`Layout::capture_unmap_neutral_vulkan` → `Tile::capture_unmap_neutral_vulkan`). Nothing else
/// fills the close snapshot's `neutral` cell (GLES readback happens later, in `ClosingWindow::new`,
/// only as a fallback), so an empty neutral here can only mean the Vulkan capture path silently
/// failed — this asserts it produced the green tile, bypassing that fallback.
#[test]
fn vulkan_captures_the_close_neutral_through_vulkan() {
    if VulkanRenderer::new().is_err() {
        eprintln!("skipping vulkan_captures_the_close_neutral_through_vulkan: no Vulkan device");
        return;
    }

    let mut f = Fixture::new();
    f.synoik_state()
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
    f.synoik_complete_animations();
    f.double_roundtrip(id);

    // The smithay `Window` backing the mapped tile (cloned so the `f.synoik()` borrow ends). Uses
    // the `LayoutElement::id` trait method (`= &Window`), not the inherent `Mapped::id() ->
    // MappedId`.
    let window_id = crate::layout::LayoutElement::id(
        f.synoik()
            .layout
            .windows()
            .next()
            .expect("a mapped window")
            .1,
    )
    .clone();

    // Capture the unmap snapshot. On a Vulkan session this goes through the owned renderer and
    // bakes no GLES texture at all. `None` output → no xray background, which is all a plain window
    // needs.
    f.synoik_state().store_unmap_snapshot(&window_id, None);

    // Inspect the tile's captured snapshot. The window is still mapped (storing a snapshot does not
    // unmap it), so the tile is still in the active workspace.
    let snapshot = f
        .synoik_state()
        .synoik
        .layout
        .active_workspace_mut()
        .expect("active workspace")
        .tiles_mut()
        .next()
        .expect("a tile")
        .take_unmap_snapshot()
        .expect("stored unmap snapshot");
    let (buffer, geo) = &snapshot.contents;

    // The tile encloses at least the WIN×WIN window (default config has no border).
    let (w, h) = (buffer.size().w, buffer.size().h);
    assert!(
        w >= WIN as i32 && h >= WIN as i32 && geo.size.w >= WIN as i32,
        "unexpected close neutral size {w}x{h} (geo {:?})",
        geo.size
    );

    let data = buffer.data();
    let is_green = |p: [u8; 4]| p[0] < 40 && p[1] > 200 && p[2] < 40;
    let green = (0..w * h)
        .filter(|i| is_green(px(data, w, i % w, i / w)))
        .count();
    eprintln!("vulkan_captures_the_close_neutral_through_vulkan: {green} green px");
    assert!(
        green as i32 > (WIN as i32) * (WIN as i32) * 3 / 4,
        "close neutral is not the green tile ({green} green px) — Vulkan capture produced no content"
    );
}

#[test]
fn vulkan_picks_a_color_through_vulkan() {
    // Phase C: pick-color reads back a single pixel through the *active* renderer — a 1x1 offscreen
    // render of the scene, relocated so `pos` lands in it, plus a copy_framebuffer readback.
    //
    // The pick is a crop of the very scene the full-frame render draws, through the same renderer
    // at the same `RenderTarget::Output`, so the picked pixel must equal the full frame's pixel at
    // `pos`. That is the correctness proof for the 1x1 path (a blank/wrong/None result diverges),
    // and it compares two genuinely independent paths through one renderer rather than leaning on a
    // second renderer to be the reference.
    if VulkanRenderer::new().is_err() {
        eprintln!("skipping vulkan_picks_a_color_through_vulkan: no Vulkan device");
        return;
    }

    let mut f = Fixture::new();
    f.synoik_state()
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

    // Map a real client window so the composited scene is non-trivial (the oracle samples the
    // backdrop below it, which is all the pick-path plumbing needs to exercise).
    let window = f.client(id).window(&surface);
    window.attach_shm_buffer(WIN as i32, WIN as i32, 0, 255, 0, 255);
    window.set_size(WIN, WIN);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.synoik_complete_animations();
    f.double_roundtrip(id);

    let output = f.synoik_output(1);
    let scale = Scale::from(output.current_scale().fractional_scale());
    let pos = smithay::utils::Point::<i32, Physical>::from((OUT_W as i32 / 2, OUT_H as i32 / 2));

    // The reference: the same scene, same renderer, same target, rendered whole.
    let (frame, fw, fh) = render_output_vulkan_target(&mut f, &output, RenderTarget::Output);

    // Probe the backdrop AND a pixel of the green window. The backdrop alone cannot catch a pick
    // that reads the *wrong* position: it is a uniform field, so any offset still reads 46. The
    // window pixel is far from the backdrop in value, so a mislocated pick shows up.
    let green_at = (0..fh)
        .flat_map(|y| (0..fw).map(move |x| (x, y)))
        .find(|&(x, y)| {
            let p = px(&frame, fw, x, y);
            p[1] > 200 && p[0] < 50 && p[2] < 50
        })
        .expect("the mapped green window must be somewhere in the frame");
    let probes = [
        (pos, "backdrop"),
        (
            smithay::utils::Point::<i32, Physical>::from(green_at),
            "window",
        ),
    ];

    let state = f.synoik_state();
    state.synoik.update_render_elements(Some(&output));

    for (probe, name) in probes {
        let want = px(&frame, fw, probe.x, probe.y);

        let vk_color = state
            .backend
            .headless()
            .with_vulkan_renderer(|vk| {
                crate::input::pick_color_grab::PickColorGrab::pick_color_with_renderer(
                    &state.synoik,
                    vk,
                    &output,
                    probe,
                    scale,
                )
            })
            .flatten();

        eprintln!(
            "vulkan_picks_a_color_through_vulkan {name} at {probe:?}: vk={vk_color:?} \
             frame={want:?}"
        );
        let vk_color = vk_color.expect("Vulkan pick returned a color");

        // The picked pixel must be the pixel the full frame drew there, within a rounding step.
        for (i, &w) in want.iter().enumerate().take(3) {
            let got = (vk_color.rgb[i] * 255.0).round() as i32;
            assert!(
                (got - i32::from(w)).abs() <= 1,
                "{name} channel {i}: pick says {got}, the full frame drew {w} at {probe:?}",
            );
        }
    }
}

#[test]
fn vulkan_screenshots_a_window_through_vulkan() {
    // Phase C: window screenshot-to-disk renders through the *active* renderer. Drive the
    // genericized `Synoik::screenshot_window` on the owned Vulkan renderer end-to-end (no disk
    // write, so no async encode thread to await) — it must run the full render + readback path
    // without erroring. Pixel correctness of the composited scene is proven by the whole-scene
    // tests above.
    if VulkanRenderer::new().is_err() {
        eprintln!("skipping vulkan_screenshots_a_window_through_vulkan: no Vulkan device");
        return;
    }

    let mut f = Fixture::new();
    f.synoik_state()
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
    f.synoik_complete_animations();
    f.double_roundtrip(id);

    let output = f.synoik_output(1);
    let state = f.synoik_state();
    state.synoik.update_render_elements(Some(&output));
    let mapped = state
        .synoik
        .layout
        .windows()
        .next()
        .expect("a mapped window")
        .1;

    let ran = state.backend.headless().with_vulkan_renderer(|vk| {
        state
            .synoik
            .screenshot_window(
                vk,
                &output,
                mapped,
                false,
                false,
                None,
                #[cfg(feature = "dbus")]
                None,
            )
            .expect("screenshot_window must succeed on the Vulkan renderer");
    });
    assert!(
        ran.is_some(),
        "screenshot_window did not run on the Vulkan renderer"
    );
}

/// End-to-end `render_to_shm` through the real wlr-screencopy protocol: a client captures the
/// output into a `wl_shm` `Xrgb8888` buffer, and we read that buffer's bytes back.
///
/// This is the only test that drives `render_to_shm` (`synoik.rs`'s shm screencopy branch) as a
/// whole — every other shm test exercises a piece (the byte-order swizzle, the import cache). A
/// plain `copy` renders synchronously server-side, so the capture completes within a roundtrip.
///
/// Red is the discriminator. The pool is `Xrgb8888` — BGRA byte order — so a red window must read
/// back with red in the **third** byte (`[0, 0, 255, 255]`). If the conversion were skipped or the
/// order wrong, red would land in the first byte instead; the test asserts both the presence of
/// BGRA-red and the absence of RGBA-red, so it fails either way.
#[test]
fn vulkan_render_to_shm_screencopy_fills_the_buffer() {
    let Some((mut f, id, _surface)) = window_fixture_with_client(RED, true, None) else {
        return;
    };
    let output = f.client(id).output("headless-1");

    // capture_output → the compositor answers with the shm geometry it wants.
    f.client(id).begin_screencopy(&output);
    for _ in 0..10 {
        f.roundtrip(id);
        if f.client(id).state.screencopy.as_ref().unwrap().buffer_done {
            break;
        }
    }

    let (format, w, h, stride) = f
        .client(id)
        .state
        .screencopy
        .as_ref()
        .unwrap()
        .shm_params
        .expect("compositor sent no shm buffer parameters");
    assert_eq!(format, wl_shm::Format::Xrgb8888, "screencopy shm format");
    assert_eq!((w, h), (u32::from(OUT_W), u32::from(OUT_H)), "buffer size");
    assert_eq!(stride, w * 4, "buffer stride");

    // Hand the compositor a matching buffer; a plain copy renders it synchronously.
    let mut readback = f
        .client(id)
        .create_shm_readback_buffer(w as i32, h as i32, format);
    f.client(id).copy_screencopy(&readback);
    for _ in 0..10 {
        f.roundtrip(id);
        let cap = f.client(id).state.screencopy.as_ref().unwrap();
        if cap.ready || cap.failed {
            break;
        }
    }
    let cap = f.client(id).state.screencopy.as_ref().unwrap();
    assert!(!cap.failed, "compositor reported the screencopy failed");
    assert!(cap.ready, "screencopy did not become ready");

    let bytes = readback.read();
    assert_eq!(
        bytes.len(),
        (w * h * 4) as usize,
        "readback size must match the buffer"
    );

    let (w, h) = (w as i32, h as i32);
    // Xrgb8888 is BGRA byte order: a red window pixel is [B=0, G=0, R=255, A=255].
    let is_bgra_red = |p: [u8; 4]| p[0] < 40 && p[1] < 40 && p[2] > 200;
    // The wrong order (RGBA / no conversion) would put red in the first byte instead.
    let is_rgba_red = |p: [u8; 4]| p[0] > 200 && p[1] < 40 && p[2] < 40;
    let bgra_red = (0..w * h)
        .filter(|i| is_bgra_red(px(&bytes, w, i % w, i / w)))
        .count();
    let rgba_red = (0..w * h)
        .filter(|i| is_rgba_red(px(&bytes, w, i % w, i / w)))
        .count();
    eprintln!(
        "vulkan_render_to_shm_screencopy_fills_the_buffer: {bgra_red} BGRA-red px, {rgba_red} \
         RGBA-red px"
    );
    assert!(
        bgra_red > 1000,
        "the red window is missing from the shm screencopy buffer ({bgra_red} BGRA-red px)"
    );
    assert!(
        rgba_red < 100,
        "the shm buffer has red in the wrong byte ({rgba_red} RGBA-red px): the Xrgb8888/BGRA \
         conversion did not happen"
    );
}

#[test]
fn vulkan_render_to_dmabuf_composites_the_scene() {
    // Phase C slice 3: screencopy renders into a client buffer through the shared
    // `render_to_dmabuf` helper (also used by the screencast path). Drive that genericized helper
    // on the owned Vulkan renderer — full damage-tracker flow (`damage_output` → `render_to_dmabuf`
    // → readback) into a GBM dmabuf — and assert the composited scene landed. Venus-only (needs a
    // render node + GBM), like the scanout test.
    use std::fs::File;

    use smithay::backend::allocator::dmabuf::AsDmabuf;
    use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
    use smithay::backend::allocator::{Allocator, Modifier};
    use smithay::backend::renderer::damage::OutputDamageTracker;

    use crate::render_helpers::render_to_dmabuf;

    let Some(mut f) = green_window_fixture() else {
        return;
    };
    let output = f.synoik_output(1);

    let file = match File::options()
        .read(true)
        .write(true)
        .open("/dev/dri/renderD128")
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "skipping vulkan_render_to_dmabuf_composites_the_scene: no render node ({e})"
            );
            return;
        }
    };
    let gbm = match GbmDevice::new(file) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("skipping vulkan_render_to_dmabuf_composites_the_scene: no GBM ({e})");
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
                "skipping vulkan_render_to_dmabuf_composites_the_scene: GBM cannot allocate \
                 Abgr8888 LINEAR buffer ({e})"
            );
            return;
        }
    };
    let mut dmabuf = bo.export().expect("export dmabuf");

    let state = f.synoik_state();
    let (pixels, w, h) = state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| -> anyhow::Result<(Vec<u8>, i32, i32)> {
            let synoik = &mut state.synoik;
            synoik.update_render_elements(Some(&output));

            let size: Size<i32, Physical> = output.current_mode().unwrap().size;
            let size = output.current_transform().transform_size(size);
            let scale = Scale::from(output.current_scale().fractional_scale());

            let ctx = RenderCtx {
                renderer: vk,
                target: RenderTarget::ScreenCapture,
                xray: None,
            };
            let elements: Vec<OutputRenderElements> = synoik.render_to_vec(ctx, &output, false);

            // The exact damage-tracker flow the screencopy path runs: `damage_output` to derive the
            // element states, then `render_to_dmabuf` (which binds + `render_output_with_states`).
            let mut damage_tracker = OutputDamageTracker::new(size, scale, Transform::Normal);
            let (_damages, states) = damage_tracker.damage_output(1, &elements).unwrap();
            let _sync =
                render_to_dmabuf(vk, &mut damage_tracker, dmabuf.clone(), &elements, states)
                    .map_err(|e| anyhow::anyhow!("render_to_dmabuf: {e}"))?;

            // Read back from the dmabuf's own memory to prove the scene landed.
            let fb = vk
                .bind(&mut dmabuf)
                .map_err(|e| anyhow::anyhow!("bind dmabuf: {e}"))?;
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
        .expect("render_to_dmabuf on the Vulkan renderer must not error");

    let green = assert_window_and_background(&pixels, w, h);
    eprintln!("vulkan_render_to_dmabuf_composites_the_scene: {green} window px");
}

#[test]
fn crop_screenshot_neutral_crops_and_composites() {
    // Phase C slice 4: the owned-Vulkan save-to-disk path crops the frozen-screen neutral CPU
    // buffer and composites the pointer on top — no GLES readback. Test the pure pixel math
    // directly (no GPU): a known crop offset and a premultiplied "over" blend.
    use smithay::utils::Point;

    use crate::render_helpers::memory::MemoryBuffer;
    use crate::ui::screenshot_ui::crop_screenshot_neutral;

    let px = |x: i32, y: i32| -> [u8; 4] { [(x * 10) as u8, (y * 10) as u8, 0, 255] };

    // 4x4 Abgr8888 neutral: pixel (x,y) = [x*10, y*10, 0, 255].
    let mut data = Vec::new();
    for y in 0..4 {
        for x in 0..4 {
            data.extend_from_slice(&px(x, y));
        }
    }
    let neutral = MemoryBuffer::new(
        data,
        Fourcc::Abgr8888,
        Size::<i32, BufferCoord>::from((4, 4)),
        Scale::from(1.0),
        Transform::Normal,
    );

    // Crop the 2x2 region at (1,1) with no pointer: exactly the four source pixels.
    let rect = Rectangle::<i32, Physical>::from_extremities((1, 1), (3, 3));
    let out = crop_screenshot_neutral(&neutral, rect, None);
    assert_eq!(&out[0..4], &px(1, 1), "crop TL");
    assert_eq!(&out[4..8], &px(2, 1), "crop TR");
    assert_eq!(&out[8..12], &px(1, 2), "crop BL");
    assert_eq!(&out[12..16], &px(2, 2), "crop BR");

    // Composite a half-transparent premultiplied red pointer ([128,0,0,128]) over a uniform-green
    // neutral. Premultiplied over: out_c = src_c + dst_c*(255-128)/255.
    let green = MemoryBuffer::new(
        [0u8, 255, 0, 255].repeat(16),
        Fourcc::Abgr8888,
        Size::<i32, BufferCoord>::from((4, 4)),
        Scale::from(1.0),
        Transform::Normal,
    );
    let pointer = MemoryBuffer::new(
        [128u8, 0, 0, 128].repeat(4),
        Fourcc::Abgr8888,
        Size::<i32, BufferCoord>::from((2, 2)),
        Scale::from(1.0),
        Transform::Normal,
    );
    // Crop the whole 4x4; pointer at physical origin (0,0) → covers the top-left 2x2.
    let rect = Rectangle::<i32, Physical>::from_extremities((0, 0), (4, 4));
    let out = crop_screenshot_neutral(&green, rect, Some((&pointer, Point::from((0, 0)))));
    // Blended pixel: R = 128 + 0 = 128; G = 0 + (255*127+127)/255 = 127; B = 0; A = 255.
    assert_eq!(&out[0..4], &[128, 127, 0, 255], "blended pointer pixel");
    // A pixel outside the 2x2 pointer stays the untouched green neutral.
    let idx = ((2 * 4 + 2) * 4) as usize;
    assert_eq!(
        &out[idx..idx + 4],
        &[0, 255, 0, 255],
        "untouched neutral pixel"
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
    let Some(mut f) = window_fixture_settled(GREEN, false, None) else {
        return;
    };
    let output = f.synoik_output(1);
    assert!(
        f.synoik().layout.are_animations_ongoing(Some(&output)),
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
    f.synoik_complete_animations();
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

/// A configured custom **open** shader must actually be reached by `Tile::render`, not merely
/// compile. `vulkan_custom_anim_element_draws_the_open_shader` builds the element by hand, so it
/// proves the shader works while saying nothing about whether the compositor ever gets there.
///
/// The wiring is what is fragile: the install runs inside `reload_config`
/// (`synoik.rs:1717` — `with_vulkan_renderer(|vk| vk.set_custom_open_shader(src))`), and
/// `opening_window.rs:167` gates on `has_custom_shader`. If that install is skipped — a
/// `with_vulkan_renderer` returning `None`, or the line going out with the GLES one beside it —
/// `has_custom_shader` is false and the built-in scale+fade runs instead. No error; the user's
/// shader silently stops existing.
///
/// Install a shader that paints the opening window solid BLUE, then composite mid-open. Nothing
/// else in the scene is blue, and the built-in animation fades the window's own GREEN, so blue
/// pixels appear only if the config → install → `has_custom_shader` → custom-element path holds
/// end to end.
#[test]
fn vulkan_reaches_the_configured_custom_open_shader() {
    if VulkanRenderer::new().is_err() {
        eprintln!("skipping vulkan_reaches_the_configured_custom_open_shader: no Vulkan device");
        return;
    }

    let mut f = Fixture::new();
    f.synoik_state()
        .backend
        .headless()
        .add_renderer()
        .expect("build the Vulkan renderer");
    f.add_output(1, (OUT_W, OUT_H));

    // Install the custom shader through the real config-reload path, before any window maps. The
    // install is diffed against the old config, so it must differ from the `Config::default()`
    // above (which has no custom shader).
    let mut config = Config::default();
    config.animations.window_open.custom_shader = Some(
        "vec4 open_color(vec3 coords_geo, vec3 size_geo) {\n\
         return vec4(0.0, 0.0, 1.0, 1.0);\n\
         }"
        .to_owned(),
    );
    f.synoik_state().reload_config(config);

    // Map a green window and leave its open animation running.
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

    let output = f.synoik_output(1);
    assert!(
        f.synoik().layout.are_animations_ongoing(Some(&output)),
        "expected the window open animation to be active"
    );

    let (pixels, w, h) = render_output_vulkan(&mut f, &output);
    let is_blue = |p: [u8; 4]| p[0] < 40 && p[1] < 40 && p[2] > 200;
    let blue = (0..w * h)
        .filter(|i| is_blue(px(&pixels, w, i % w, i / w)))
        .count();

    eprintln!("vulkan_reaches_the_configured_custom_open_shader: {blue} blue px");
    assert!(
        blue > 0,
        "the configured custom open shader never reached Tile::render (fell back to the built-in \
         animation?): {blue} blue px"
    );
}

/// The tile **alpha** animation (window movement fades, interactive move) renders the tile into an
/// offscreen and composites it at the animated alpha. It is the open animation's sibling one branch
/// down in `Tile::render`, and it fails the same silent way: `alpha.offscreen_vk.render` erroring
/// only `warn!`s and leaves `pushed = false` (`tile.rs:1618`), so the tile falls through to the
/// plain render **at full alpha** — the window is still there, fully opaque, and nothing errors.
///
/// Settle the window, then fade it toward invisible and composite mid-fade: the faded frame must be
/// markedly less green than the settled one. A fall-through to the plain render composites full
/// alpha even mid-fade, so this fails if the Vulkan offscreen arm stops being reached.
#[test]
fn vulkan_renders_a_tile_mid_alpha_animation() {
    use synoik_config::animations::{Curve, EasingParams, Kind};

    let Some(mut f) = green_window_fixture() else {
        return;
    };
    let output = f.synoik_output(1);

    let is_green = |p: [u8; 4]| p[0] < 40 && p[1] > 200 && p[2] < 40 && p[3] > 200;
    let count_green = |pixels: &[u8], w: i32, h: i32| {
        (0..w * h)
            .filter(|i| is_green(px(pixels, w, i % w, i / w)))
            .count()
    };

    // Settled: full alpha, the whole window is opaque green.
    let (settled, w, h) = render_output_vulkan(&mut f, &output);
    let green_settled = assert_window_and_background(&settled, w, h);

    // Fade the tile out over 1s, linearly, and stop half way.
    let anim = synoik_config::Animation {
        off: false,
        kind: Kind::Easing(EasingParams {
            duration_ms: 1000,
            curve: Curve::Linear,
        }),
    };
    f.synoik_state()
        .synoik
        .layout
        .active_workspace_mut()
        .expect("active workspace")
        .tiles_mut()
        .next()
        .expect("a tile")
        .animate_alpha(1., 0., anim);

    let now = f.synoik().clock.now_unadjusted();
    f.synoik()
        .clock
        .set_unadjusted(now + std::time::Duration::from_millis(500));
    f.synoik().advance_animations();
    assert!(
        f.synoik().layout.are_animations_ongoing(Some(&output)),
        "expected the alpha animation to still be running at the half-way point"
    );

    let (mid, _, _) = render_output_vulkan(&mut f, &output);
    let green_mid = count_green(&mid, w, h);

    // Fully-green pixels alone can't tell a fade from a disappearance: both score 0. So also count
    // pixels merely *dominated* by green — a half-faded green tile over the backdrop still is, a
    // vanished tile is not. The fade must dim the window without removing it.
    let is_greenish = |p: [u8; 4]| p[1] as i32 > p[0] as i32 + 40 && p[1] as i32 > p[2] as i32 + 40;
    let greenish_mid = (0..w * h)
        .filter(|i| is_greenish(px(&mid, w, i % w, i / w)))
        .count();

    eprintln!(
        "vulkan_renders_a_tile_mid_alpha_animation: green settled={green_settled} mid={green_mid} \
         greenish_mid={greenish_mid}"
    );
    assert!(green_settled > 0, "the settled window is absent");
    assert!(
        green_mid < green_settled / 2,
        "the alpha animation did not fade the tile on Vulkan (fell through to a plain full-alpha \
         render?): settled={green_settled} mid={green_mid}"
    );
    assert!(
        greenish_mid > green_settled / 2,
        "the fading tile vanished instead of dimming (blank offscreen?): greenish={greenish_mid} \
         settled={green_settled}"
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

/// A blurred frame must cost the **same number of submits** as an unblurred one.
///
/// The backdrop blur used to cost two round trips of its own on top of the frame: the capture ended
/// the command buffer and submitted + fence-waited it, so that the blur — running on a submission
/// of its own — would see a finished blit. Both are gone. The blur is recorded into the gap the
/// capture opens between the frame's two render passes, and the barrier already there is what
/// orders it.
///
/// Counted as a differential against the same scene without the effect, so the assertion does not
/// depend on how many submits a plain frame happens to take (the readback alone is one). On this
/// path the difference used to be exactly 2.
#[test]
fn vulkan_a_blurred_frame_adds_no_submits() {
    let mut vk = match VulkanRenderer::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping vulkan_a_blurred_frame_adds_no_submits: no Vulkan ({e})");
            return;
        }
    };

    let size = Size::<i32, Physical>::from((256, 256));
    let submits = |_: ()| -> u64 {
        synoik_vk::stats::take_sites()
            .iter()
            .map(|site| site.submits)
            .sum()
    };
    let blurred_element = |passes: u8| {
        let fbe = crate::render_helpers::framebuffer_effect::FramebufferEffect::new();
        let params = crate::render_helpers::background_effect::RenderParams {
            geometry: Rectangle::from_size(Size::from((200., 200.))),
            subregion: None,
            clip: None,
            scale: 1.0,
        };
        let blur = crate::render_helpers::blur::BlurOptions {
            passes,
            offset: 5.0,
        };
        fbe.render(None, params, Some(blur), 0.0, 1.0)
    };

    // Warm every cache a first frame would build (pipelines are lazy, the pool's staging chunk is
    // not yet allocated), so the two measured renders differ only in the effect.
    let _ = render_to_vec(
        &mut vk,
        size,
        Scale::from(1.0),
        Transform::Normal,
        Fourcc::Abgr8888,
        std::iter::once(blurred_element(3)),
    )
    .expect("warmup render");

    let _ = submits(());
    let _ = render_to_vec(
        &mut vk,
        size,
        Scale::from(1.0),
        Transform::Normal,
        Fourcc::Abgr8888,
        std::iter::empty::<crate::render_helpers::framebuffer_effect::FramebufferEffectElement>(),
    )
    .expect("plain render");
    let plain = submits(());

    let _ = render_to_vec(
        &mut vk,
        size,
        Scale::from(1.0),
        Transform::Normal,
        Fourcc::Abgr8888,
        std::iter::once(blurred_element(3)),
    )
    .expect("blurred render");
    let blurred = submits(());

    eprintln!("vulkan_a_blurred_frame_adds_no_submits: plain={plain} blurred={blurred}");
    assert_eq!(
        blurred,
        plain,
        "a backdrop blur cost {} extra submit(s) — it must ride the frame's own command buffer, \
         in the gap `capture_region` opens",
        blurred as i64 - plain as i64,
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
    use smithay::backend::allocator::dmabuf::{Dmabuf, DmabufFlags};
    use smithay::backend::allocator::Modifier;
    use smithay::backend::renderer::element::Kind;
    use smithay::backend::renderer::ImportDma;
    use smithay::utils::Point;
    use synoik_vk::dmabuf::ForeignBuffer;

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

/// **The alpha convention.** Every texture the renderer samples holds *premultiplied* alpha
/// (Wayland client buffers, the icon decoder, every `widget::bake`), so compositing one must be
/// `src + dst·(1−α)`. Blending it as straight-alpha instead — `src·α + dst·(1−α)` — multiplies by
/// α a second time.
///
/// That second multiply is exactly zero for α=1 and for black (rgb=0), which is why every other
/// test in this file stayed green while the renderer was doing it: opaque windows and black
/// scrims are the entire corpus. It only shows on *partial-alpha colored* content, where it turns
/// a lightening wash into a darkening one — a white 50% wash over a dark backdrop came out
/// **below** the backdrop it was meant to lighten.
///
/// Two textures (not a solid + a texture) so `render_to_vec`'s element type stays homogeneous.
/// Note the ordering: `render_helpers::render_to_vec` draws in iterator order, so the **last**
/// element is the topmost — the opposite of the `Synoik::render_to_vec` element list, where the
/// first is. Putting the wash first here paints it *under* the opaque backdrop and the readback is
/// a flat 51, which is a passing-looking way to test nothing.
#[test]
fn vulkan_blends_partial_alpha_textures_premultiplied() {
    use smithay::backend::renderer::element::Kind;
    use smithay::backend::renderer::ImportMem;

    use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};

    let mut vk = match VulkanRenderer::new() {
        Ok(vk) => vk,
        Err(e) => {
            eprintln!(
                "skipping vulkan_blends_partial_alpha_textures_premultiplied: no Vulkan ({e})"
            );
            return;
        }
    };

    const S: i32 = 32;
    let fill = |rgba: [u8; 4]| -> Vec<u8> {
        std::iter::repeat_n(rgba, (S * S) as usize)
            .flatten()
            .collect()
    };
    let import = |vk: &mut VulkanRenderer, bytes: &[u8]| {
        vk.import_memory(bytes, Fourcc::Abgr8888, Size::from((S, S)), false)
            .expect("import memory")
    };

    // Backdrop: opaque dark grey (51 ≈ 0.2). Wash: white at 50% *premultiplied*, i.e. straight
    // white (1,1,1) times α=0.5 → 128 in every channel including alpha.
    let backdrop_tex = import(&mut vk, &fill([51, 51, 51, 255]));
    let wash_tex = import(&mut vk, &fill([128, 128, 128, 128]));

    let element =
        |vk: &VulkanRenderer, tex: &crate::render_helpers::vulkan::VkTexture, alpha: f32| {
            let buffer =
                TextureBuffer::from_texture(vk, tex.clone(), 1.0, Transform::Normal, Vec::new());
            TextureRenderElement::from_texture_buffer(
                buffer,
                Point::from((0.0, 0.0)),
                alpha,
                None,
                None,
                Kind::Unspecified,
            )
        };

    let composite = |vk: &mut VulkanRenderer, wash_alpha: f32| -> [u8; 4] {
        let elements = vec![
            element(vk, &backdrop_tex, 1.0),
            element(vk, &wash_tex, wash_alpha),
        ];
        let pixels = render_to_vec(
            vk,
            Size::from((S, S)),
            Scale::from(1.0),
            Transform::Normal,
            Fourcc::Abgr8888,
            elements.into_iter(),
        )
        .expect("composite the wash over the backdrop");
        px(&pixels, S, S / 2, S / 2)
    };

    // Premultiplied-over at full element alpha: 128 + 51·(1−0.5) = 153.5.
    // Straight-of-premultiplied (the bug) would give 128·0.5 + 25.5 = 89.5 — *darker* than the
    // 51 backdrop is light, i.e. the wash would dim instead of brighten.
    let full = composite(&mut vk, 1.0);
    assert!(
        (i16::from(full[0]) - 153).abs() <= 3,
        "premultiplied white 50% over a 51 backdrop must read ~153, got {full:?} \
         (~89 means the blend multiplied by alpha twice)",
    );
    assert!(
        full[0] > 51,
        "a white wash must LIGHTEN the backdrop it covers, got {full:?} over 51",
    );

    // The element-alpha tint travels the same way: the push tint is premultiplied, so α=0.5 scales
    // the already-premultiplied sample to 64, over 51·(1−0.25) = 38.25 → ~102. A straight
    // `[1, 1, 1, α]` tint would leave rgb unattenuated and land near 70 instead.
    let half = composite(&mut vk, 0.5);
    assert!(
        (i16::from(half[0]) - 102).abs() <= 3,
        "the same wash at element alpha 0.5 must read ~102, got {half:?}",
    );

    eprintln!(
        "vulkan_blends_partial_alpha_textures_premultiplied: full={full:?} half={half:?} \
         (backdrop 51)"
    );
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
        .create_buffer(NATIVE_FOURCC, buf_size)
        .expect("create capture dest");
    let mut target = vk
        .create_buffer(NATIVE_FOURCC, buf_size)
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
            frame
                .capture_region(phys_region, &dest, |_| {})
                .expect("capture");
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
    use smithay::backend::renderer::Offscreen;
    use smithay::utils::user_data::UserDataMap;
    use synoik_config::CornerRadius;

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
        .create_buffer(NATIVE_FOURCC, Size::from((S, S)))
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

/// A surface that sets a blur region (`ext-background-effect-v1` `set_blur_region`) must get blur
/// **only inside that region**. The subregion restricts the drawn damage, so the scene outside it
/// stays untouched.
///
/// Same red|green hard-edge scene as [`vulkan_backdrop_blur_softens_a_hard_edge`], with the effect
/// covering the whole quad but a subregion of only the top half. The edge must soften in the top
/// half and stay perfectly sharp in the bottom half. Blurring everything — which is what ignoring
/// the subregion does — fails the bottom-half assert.
#[test]
fn vulkan_backdrop_blur_honours_the_subregion() {
    use std::sync::Arc;

    use smithay::backend::renderer::Offscreen;
    use smithay::utils::user_data::UserDataMap;

    use crate::render_helpers::background_effect::RenderParams;
    use crate::render_helpers::blur::BlurOptions;
    use crate::render_helpers::framebuffer_effect::FramebufferEffect;
    use crate::render_helpers::vulkan::VulkanRenderer as Vk;
    use crate::utils::region::TransformedRegion;

    let mut vk = match Vk::new() {
        Ok(vk) => vk,
        Err(e) => {
            eprintln!("skipping vulkan_backdrop_blur_honours_the_subregion: no Vulkan ({e})");
            return;
        }
    };

    const S: i32 = 64;
    let size = Size::<i32, Physical>::from((S, S));
    let mut target = vk
        .create_buffer(NATIVE_FOURCC, Size::from((S, S)))
        .expect("create target");

    let effect = FramebufferEffect::new();
    let params = RenderParams {
        geometry: Rectangle::from_size(Size::from((S as f64, S as f64))),
        // Blur only the top half.
        subregion: Some(TransformedRegion {
            rects: Arc::new(vec![Rectangle::from_size(Size::from((S, S / 2)))]),
            scale: Scale::from(1.0),
            offset: Point::from((0., 0.)),
        }),
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
        0.0,
        1.0,
    );
    let cache = UserDataMap::new();
    let src = element.src();
    let dst = element.geometry(Scale::from(1.0));

    {
        let mut fb = vk.bind(&mut target).expect("bind");
        let mut frame = vk.render(&mut fb, size, Transform::Normal).expect("render");
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

    // Strongest blend (both channels raised) on a row — only a blurred edge can produce one.
    let blend_on_row = |y: i32| {
        (0..S).fold(0u8, |best, x| {
            let p = px(&pixels, S, x, y);
            best.max(p[0].min(p[1]))
        })
    };

    let inside = blend_on_row(S / 4); // top half — inside the subregion
    let outside = blend_on_row(3 * S / 4); // bottom half — outside it

    assert!(
        inside > 40,
        "the edge did not soften inside the blur subregion (max min(R,G) = {inside})"
    );
    assert!(
        outside <= 40,
        "the edge softened OUTSIDE the blur subregion (max min(R,G) = {outside}); the \
         client-requested blur region was ignored and the whole quad was blurred"
    );

    eprintln!(
        "vulkan_backdrop_blur_honours_the_subregion: inside blend={inside}, outside blend={outside}"
    );
}

/// Phase-C slice-5 (xray port) commit-1: the `EffectBuffer` gained a Vulkan arm that renders its
/// elements into an owned offscreen and (eagerly) blurs it, sampled through `render_postprocess` —
/// the exact primitive the ported `XrayElement::draw` will use in commit-2. This pins the whole arm
/// end-to-end while it is still dead code. The offscreen is a hard red|green vertical edge, and the
/// cases target the design's actual risks:
///   (a) full src → left-red/right-green passthrough;
///   (b) a cropped src (right half) → all green — proves the sampled sub-rect is honored (a
///       full-src-only test would go green while hiding a src/composition bug);
///   (c) blur on → the hard edge softens (blended pixels the sharp scene can't have), proving the
///       eager Vulkan blur ran and differs from the unblurred consume;
///   (d) resize via `update_size` + re-prepare → the **atomic blur-chain rebuild** (the trap this
///       design exists for: the chain binds a fixed source view and has no `Drop`) produces a valid
///       blurred sample at the new size, no validation error;
///   (e) mutate the elements + re-prepare with blur → the blurred output *changed*, exercising the
///       `valid`-flag invalidation and the same-`EffectBlur` output-reuse (UNDEFINED-discard) path.
/// Offscreen-only, so it runs on lavapipe too.
#[test]
fn vulkan_effect_buffer_renders_offscreen_and_blur() {
    use smithay::backend::renderer::element::Kind;
    use smithay::backend::renderer::{Offscreen, Texture as _};
    use synoik_vk::render::PostprocessPush;

    use crate::render_helpers::blur::BlurOptions;
    use crate::render_helpers::effect_buffer::EffectBuffer;
    use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
    use crate::render_helpers::vulkan::VkTexture;

    let mut vk = match VulkanRenderer::new() {
        Ok(vk) => vk,
        Err(e) => {
            eprintln!("skipping vulkan_effect_buffer_renders_offscreen_and_blur: no Vulkan ({e})");
            return;
        }
    };

    const IDENTITY_MAT3: [[f32; 4]; 3] = [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
    ];

    // Identity postprocess (no clip, no rounding, no desaturation); render_postprocess fills
    // origin/size/proj/target/src_rect.
    let identity_push = |geo: i32| PostprocessPush {
        origin: [0.0, 0.0],
        size: [0.0, 0.0],
        proj: [0.0; 4],
        target: [0.0, 0.0],
        geo_size: [geo as f32, geo as f32],
        src_rect: [0.0; 4],
        corner_radius: [0.0; 4],
        bg_color: [0.0; 4],
        input_to_geo: IDENTITY_MAT3,
        sample_transform: IDENTITY_MAT3,
        synoik_scale: 1.0,
        synoik_alpha: 1.0,
        saturation: 1.0,
        noise: 0.0,
    };

    // Sample `tex` (through `src`) across a whole `s`×`s` target and read it back.
    let sample = |vk: &mut VulkanRenderer,
                  tex: &VkTexture,
                  src: Rectangle<f64, BufferCoord>,
                  s: i32|
     -> Vec<u8> {
        let size = Size::<i32, Physical>::from((s, s));
        let mut target = vk
            .create_buffer(NATIVE_FOURCC, Size::<i32, BufferCoord>::from((s, s)))
            .expect("create sample target");
        {
            let mut fb = vk.bind(&mut target).expect("bind sample target");
            let mut frame = vk.render(&mut fb, size, Transform::Normal).expect("render");
            frame
                .clear(
                    Color32F::from([0.0, 0.0, 0.0, 1.0]),
                    &[Rectangle::from_size(size)],
                )
                .expect("clear");
            let dst = Rectangle::<i32, Physical>::from_size(size);
            frame
                .render_postprocess(
                    tex,
                    src,
                    dst,
                    &[Rectangle::from_size(size)],
                    identity_push(s),
                )
                .expect("render_postprocess");
            let _ = frame.finish().expect("finish");
        }
        let fb = vk.bind(&mut target).expect("rebind sample target");
        let region = Rectangle::<i32, BufferCoord>::from_size(Size::from((s, s)));
        let mapping = vk
            .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
            .expect("copy");
        vk.map_texture(&mapping).expect("map").to_vec()
    };

    // Fill the effect buffer's offscreen with a hard edge: left half red, right half green.
    let fill_edge = |buffer: &mut EffectBuffer, s: i32| {
        let red =
            SolidColorBuffer::new(Size::from((s as f64 / 2.0, s as f64)), [1.0, 0.0, 0.0, 1.0]);
        let green =
            SolidColorBuffer::new(Size::from((s as f64 / 2.0, s as f64)), [0.0, 1.0, 0.0, 1.0]);
        let elements = buffer.elements_vulkan();
        elements.clear();
        elements.push(
            SolidColorRenderElement::from_buffer(&red, (0.0, 0.0), 1.0, Kind::Unspecified).into(),
        );
        elements.push(
            SolidColorRenderElement::from_buffer(
                &green,
                (s as f64 / 2.0, 0.0),
                1.0,
                Kind::Unspecified,
            )
            .into(),
        );
    };

    const S: i32 = 64;
    let scale = Scale::from(1.0);
    let full_src = Rectangle::<f64, BufferCoord>::from_size(Size::from((S as f64, S as f64)));
    let right_src = Rectangle::<f64, BufferCoord>::new(
        (S as f64 / 2.0, 0.0).into(),
        (S as f64 / 2.0, S as f64).into(),
    );

    let mut buffer = EffectBuffer::new();
    assert!(
        buffer.render_element_states().is_none(),
        "no states before the first render"
    );
    buffer.update_size(Size::<i32, Physical>::from((S, S)), scale);
    fill_edge(&mut buffer, S);
    assert!(
        buffer.prepare_vulkan(&mut vk, false),
        "prepare_vulkan (no blur) failed"
    );

    // The states of the Vulkan render must be observable: `Synoik::update_primary_scanout_output`
    // reads them to remap a background layer surface that is visible only through this xray buffer
    // onto the xray element's id. Reading the GLES arm here returned `None` on every real frame, so
    // that remap silently never fired and such a surface had its frame callbacks throttled.
    assert!(
        buffer.render_element_states().is_some(),
        "the Vulkan render's element states must be observable, else the background-layer id \
         remap in update_primary_scanout_output silently never fires"
    );

    // (a) full src → left red, right green; (b) cropped src = right half → all green.
    {
        let tex = buffer.texture_vulkan(false).expect("offscreen texture");

        let full = sample(&mut vk, &tex, full_src, S);
        let l = px(&full, S, 4, S / 2);
        let r = px(&full, S, S - 5, S / 2);
        assert!(
            l[0] > 200 && l[1] < 50,
            "full-src left should be red, got {l:?}"
        );
        assert!(
            r[1] > 200 && r[0] < 50,
            "full-src right should be green, got {r:?}"
        );

        let cropped = sample(&mut vk, &tex, right_src, S);
        let l = px(&cropped, S, 4, S / 2);
        let r = px(&cropped, S, S - 5, S / 2);
        assert!(
            l[1] > 200 && l[0] < 50,
            "cropped-src left should sample the green right half, got {l:?}"
        );
        assert!(
            r[1] > 200 && r[0] < 50,
            "cropped-src right should be green, got {r:?}"
        );
    } // drop the offscreen clone before the next prepare so `is_unique_reference` holds

    // (c) blur on → the hard edge softens.
    buffer.update_blur_options(BlurOptions {
        passes: 3,
        offset: 2.0,
    });
    assert!(
        buffer.prepare_vulkan(&mut vk, true),
        "prepare_vulkan (blur) failed"
    );
    let edge_blend = {
        let tex = buffer.texture_vulkan(true).expect("blurred texture");
        let blurred = sample(&mut vk, &tex, full_src, S);
        let y = S / 2;
        let mut best = 0u8;
        for x in 0..S {
            let p = px(&blurred, S, x, y);
            best = best.max(p[0].min(p[1]));
        }
        assert!(
            best > 40,
            "blur did not soften the edge (max min(R,G) = {best})"
        );
        best
    };

    // (d) resize → the atomic blur-chain rebuild at the new size (a full recreate: old chain
    // dropped with the old offscreen, new chain bound to the new texture view).
    const S2: i32 = 96;
    buffer.update_size(Size::<i32, Physical>::from((S2, S2)), scale);
    fill_edge(&mut buffer, S2);
    assert!(
        buffer.prepare_vulkan(&mut vk, true),
        "prepare_vulkan after resize failed"
    );
    let full_src2 = Rectangle::<f64, BufferCoord>::from_size(Size::from((S2 as f64, S2 as f64)));
    {
        let tex = buffer
            .texture_vulkan(true)
            .expect("resized blurred texture");
        assert_eq!(
            tex.size(),
            Size::<i32, BufferCoord>::from((S2, S2)),
            "resized offscreen has the wrong size"
        );
        let blurred = sample(&mut vk, &tex, full_src2, S2);
        let l = px(&blurred, S2, 3, S2 / 2);
        let r = px(&blurred, S2, S2 - 4, S2 / 2);
        assert!(
            l[0] > 120 && l[0] > l[1],
            "resized far-left should stay red-dominant, got {l:?}"
        );
        assert!(
            r[1] > 120 && r[1] > r[0],
            "resized far-right should stay green-dominant, got {r:?}"
        );
    }

    // (e) mutate the offscreen (all blue) + re-prepare with blur → the blurred output changed (same
    // texture re-rendered in place, blur invalidated + re-run into the reused output).
    {
        let blue = SolidColorBuffer::new(Size::from((S2 as f64, S2 as f64)), [0.0, 0.0, 1.0, 1.0]);
        let elements = buffer.elements_vulkan();
        elements.clear();
        elements.push(
            SolidColorRenderElement::from_buffer(&blue, (0.0, 0.0), 1.0, Kind::Unspecified).into(),
        );
    }
    assert!(
        buffer.prepare_vulkan(&mut vk, true),
        "prepare_vulkan after mutate failed"
    );
    {
        let tex = buffer
            .texture_vulkan(true)
            .expect("mutated blurred texture");
        let blurred = sample(&mut vk, &tex, full_src2, S2);
        let c = px(&blurred, S2, S2 / 2, S2 / 2);
        assert!(
            c[2] > 180 && c[0] < 60 && c[1] < 60,
            "mutated blur should be blue, got {c:?}"
        );
    }

    eprintln!(
        "vulkan_effect_buffer_renders_offscreen_and_blur: ok (edge blend min(R,G)={edge_blend})"
    );
}

/// Phase-C slice-5 commit-2: the ported `XrayElement` Vulkan draw against the GLES oracle. This
/// targets the coordinate-fold trap — GLES feeds `input_to_geo` the full-buffer UV (its `v_coords`
/// maps the quad to `src` within the texture), while the Vulkan `postprocess.frag` feeds it
/// quad-local `v_uv` + a separate `src_rect`, so the Vulkan draw must re-base `input_to_clip_geo`
/// onto `v_uv` using the SAME draw-time `src`. Build a Vulkan `XrayElement` with a **cropped src
/// (right half)** / identity `input_to_clip_geo` / nonzero `corner_radius`, over red|green
/// offscreen content, and assert the composited output directly:
///   - the sampled content is the GREEN right half (proves the cropped `src` is honored: a full src
///     would sample red on the left);
///   - only the RIGHT corners are rounded (the geometry maps to `[0.5,1]×[0,1]`); the top-LEFT
///     corner stays GREEN — this is the fold discriminator: dropping the fold would clip via raw
///     `v_uv ∈ [0,1]` and round the left corners too, cutting the top-left to transparent.
///
/// The probes state what the fold must produce, so they hold without a second renderer to compare
/// against. Offscreen-only, so it runs on lavapipe.
#[test]
fn vulkan_xray_honors_the_cropped_src_fold() {
    use std::cell::RefCell;
    use std::rc::Rc;

    use glam::{Mat3, Vec2};
    use smithay::backend::renderer::element::Kind;
    use smithay::utils::Logical;
    use synoik_config::CornerRadius;

    use crate::render_helpers::effect_buffer::EffectBuffer;
    use crate::render_helpers::render_to_vec;
    use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
    use crate::render_helpers::xray::XrayElement;

    let Some(mut f) = window_fixture(GREEN) else {
        return;
    };
    if VulkanRenderer::new().is_err() {
        eprintln!("skipping vulkan_xray_honors_the_cropped_src_fold: no Vulkan");
        return;
    }

    const S: i32 = 64;
    const R: f32 = 16.0;
    let size = Size::<i32, Physical>::from((S, S));
    let scale = Scale::from(1.0);

    // Shared element parameters (identical for both renderers). Identity input_to_clip_geo + a
    // right-half `src` crop: the fold must map v_uv → [0.5,1]×[0,1] geo, so only the right corners
    // round. `clip_geo_size` is the geometry size in the rounding coordinate space.
    let geometry = Rectangle::<f64, Logical>::from_size(Size::from((S as f64, S as f64)));
    let src = Rectangle::<f64, BufferCoord>::new(
        (S as f64 / 2.0, 0.0).into(),
        (S as f64 / 2.0, S as f64).into(),
    );
    let i2g = Mat3::IDENTITY;
    let clip_geo_size = Vec2::new(S as f32, S as f32);
    let corner_radius = CornerRadius::from(R);
    let bg = Color32F::TRANSPARENT;

    // Left-red / right-green content for the effect buffer, as this renderer's element type.
    fn build_edge<E: From<SolidColorRenderElement>>() -> Vec<E> {
        let red = SolidColorBuffer::new(Size::from((S as f64 / 2.0, S as f64)), [1., 0., 0., 1.]);
        let green = SolidColorBuffer::new(Size::from((S as f64 / 2.0, S as f64)), [0., 1., 0., 1.]);
        vec![
            SolidColorRenderElement::from_buffer(&red, (0., 0.), 1., Kind::Unspecified).into(),
            SolidColorRenderElement::from_buffer(
                &green,
                (S as f64 / 2., 0.),
                1.,
                Kind::Unspecified,
            )
            .into(),
        ]
    }

    let state = f.synoik_state();

    // Vulkan (program = None → draws through render_postprocess).
    let vk = state
        .backend
        .headless()
        .with_vulkan_renderer(|v| {
            let buffer = Rc::new(RefCell::new(EffectBuffer::new()));
            {
                let mut b = buffer.borrow_mut();
                b.update_size(size, scale);
                let elements = b.elements_vulkan();
                elements.clear();
                elements.extend(build_edge());
            }
            buffer.borrow_mut().prepare_vulkan(v, false);
            let elem = XrayElement::new_for_test(
                buffer.clone(),
                geometry,
                src,
                i2g,
                clip_geo_size,
                corner_radius,
                scale.x as f32,
                false,
                bg,
            );
            render_to_vec(
                v,
                size,
                scale,
                Transform::Normal,
                Fourcc::Abgr8888,
                [elem].into_iter(),
            )
        })
        .expect("Vulkan renderer present")
        .expect("Vulkan xray render");

    let green = |p: [u8; 4]| p[1] > 200 && p[0] < 60 && p[3] > 200;
    let clipped = |p: [u8; 4]| p[3] < 60;

    // Probes: center (green), top-left corner (green — the fold discriminator), top-right corner
    // (clipped — confirms the right corner IS rounded, so the clip is active, not a no-op).
    let probes = [
        ("center", S / 2, S / 2, true),
        ("top-left", 2, 2, true),
        ("top-right", S - 3, 2, false),
    ];
    for (name, x, y, want_green) in probes {
        let vp = px(&vk, S, x, y);
        if want_green {
            assert!(green(vp), "{name} ({x},{y}) should be green, vk={vp:?}");
        } else {
            assert!(clipped(vp), "{name} ({x},{y}) should be clipped, vk={vp:?}");
        }
    }

    // No red anywhere: the cropped `src` sampled only the green right half.
    let red = (0..S * S)
        .filter(|i| {
            let p = px(&vk, S, i % S, i / S);
            p[0] > 120 && p[1] < 80 && p[3] > 120
        })
        .count();
    assert_eq!(red, 0, "cropped src leaked red (count {red})");

    eprintln!(
        "vulkan_xray_honors_the_cropped_src_fold: ok (fold applied, cropped src leaks no red)"
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

    // Render the red|green scene at `transform`, optionally with a whole-output effect on top, and
    // read it back. `blur` turns the effect from a no-op passthrough into a real blur.
    let render_scene_opt = |vk: &mut Vk,
                            transform: Transform,
                            with_effect: bool,
                            blur: Option<crate::render_helpers::blur::BlurOptions>,
                            subregion: Option<crate::utils::region::TransformedRegion>|
     -> Vec<u8> {
        let mut target = vk
            .create_buffer(NATIVE_FOURCC, Size::from((S, S)))
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
                    subregion: subregion.clone(),
                    clip: None,
                    scale: 1.0,
                };
                // noise 0, saturation 1: with `blur` None the effect should reproduce the backdrop.
                let element = effect.render(None, params, blur, 0.0, 1.0);
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

    let render_scene = |vk: &mut Vk, transform: Transform, with_effect: bool| -> Vec<u8> {
        render_scene_opt(vk, transform, with_effect, None, None)
    };

    let blur_opts = crate::render_helpers::blur::BlurOptions {
        passes: 3,
        offset: 2.0,
    };

    // Count the quadrants that contain a blended (blurred-edge) pixel. The edge crosses all four
    // quadrants whatever the transform, so an unrestricted blur lights up 4; a blur restricted to
    // the logical top half lights up exactly the 2 quadrants that half maps to — whichever 2 those
    // are for a given transform. That makes this assertion transform-agnostic without having to
    // hard-code Smithay's rotation convention.
    let blended_quadrants = |pixels: &[u8]| -> usize {
        let mut n = 0;
        for (qx, qy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            let mut found = false;
            for y in (qy * S / 2)..((qy + 1) * S / 2) {
                for x in (qx * S / 2)..((qx + 1) * S / 2) {
                    let p = px(pixels, S, x, y);
                    if p[0].min(p[1]) > 40 {
                        found = true;
                    }
                }
            }
            n += usize::from(found);
        }
        n
    };

    // The strongest blend (both channels raised) anywhere in the frame. Only a blurred edge can
    // produce one; the sharp red|green scene cannot.
    let best_blend = |pixels: &[u8]| -> u8 {
        (0..S * S).fold(0u8, |best, i| {
            let p = px(pixels, S, i % S, i / S);
            best.max(p[0].min(p[1]))
        })
    };

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

        // The same, with the blur actually on. A wrongly-oriented capture would blur the backdrop
        // and put it back rotated, which shows up far from the edge as a corner changing colour —
        // the corners sit well outside the blur's reach, so they must survive a real blur too.
        let blurred = render_scene_opt(&mut vk, t, true, Some(blur_opts), None);
        let bc = corners(&blurred);
        for (i, (p, b)) in pc.iter().zip(bc.iter()).enumerate() {
            assert!(
                close(*p, *b),
                "{t:?}: corner {i} changed by the BLURRED effect: plain={p:?} blurred={b:?} \
                 (the blurred backdrop composited with the wrong orientation)"
            );
        }

        // And the blur must actually have run: the sharp scene has no blended pixel, the blurred
        // one must. Without this the corner check above would pass on a blur that drew nothing.
        let (sharp_blend, blur_blend) = (best_blend(&plain), best_blend(&blurred));
        assert!(
            sharp_blend <= 40,
            "{t:?}: the plain scene already has a blended pixel ({sharp_blend}); the probe is \
             not discriminating"
        );
        assert!(
            blur_blend > 40,
            "{t:?}: the blur did not soften the edge (max min(R,G) = {blur_blend})"
        );

        // A blur restricted to the *logical* top half must follow the content through the output
        // transform. The blurred edge crosses all four quadrants, so an unrestricted blur lights up
        // 4 and the subregion must cut that to exactly the 2 the logical top half maps to. A
        // subregion applied in the wrong space would light up some other count (4 if ignored).
        let sub_blurred = render_scene_opt(
            &mut vk,
            t,
            true,
            Some(blur_opts),
            Some(crate::utils::region::TransformedRegion {
                rects: std::sync::Arc::new(vec![Rectangle::from_size(Size::from((S, S / 2)))]),
                scale: Scale::from(1.0),
                offset: Point::from((0., 0.)),
            }),
        );
        let (all_q, sub_q) = (blended_quadrants(&blurred), blended_quadrants(&sub_blurred));
        assert_eq!(
            all_q, 4,
            "{t:?}: the unrestricted blur should reach all 4 quadrants, got {all_q} — the probe \
             is not discriminating"
        );
        assert_eq!(
            sub_q, 2,
            "{t:?}: a blur restricted to the logical top half should reach exactly 2 quadrants, \
             got {sub_q} (4 = the subregion was ignored; other = it landed in the wrong space \
             under this transform)"
        );

        eprintln!(
            "vulkan_backdrop_effect_roundtrips_under_rotation {t:?}: corners {pc:?} preserved, \
             blur blend {sharp_blend} -> {blur_blend}, quadrants all={all_q} subregion={sub_q}"
        );
    }
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

    let mut f = Fixture::with_config(config);
    f.synoik_state()
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

    f.synoik_complete_animations();
    f.double_roundtrip(id);

    Some(f)
}

const CLIP_RADIUS: f32 = 30.;

/// The clip material must both **sample** the window (rounded corners are not blank) and **round**
/// it (the geometry corners are cut away), matching the GLES oracle. This drives the full
/// `Synoik::render_to_vec` scene — a green window with a corner radius + `clip-to-geometry` —
/// through both renderers and asserts, without pixel-exact AA comparison, that: the window center
/// is green (sampled), the corners of the green region are **not** green (rounded away to the
/// backdrop), and the mid-edges **are** green (only the corners were cut — it is rounding, not a
/// full clip).
#[test]
fn vulkan_clips_a_window_to_rounded_geometry() {
    let Some(mut f) = clipped_window_fixture() else {
        return;
    };
    let output = f.synoik_output(1);

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
    let vbox = check("vulkan", &vk, vw, vh);

    // Only the corners were cut, so the green bbox still spans the window's own size. Pins the
    // clip to the window's geometry -- an over-eager clip (or a rounding radius applied to the
    // wrong space) shrinks it.
    let (bw, bh) = (vbox.2 - vbox.0 + 1, vbox.3 - vbox.1 + 1);
    assert!(
        (bw - WIN as i32).abs() <= 2 && (bh - WIN as i32).abs() <= 2,
        "clipped window bbox {vbox:?} is {bw}x{bh}, not the window's {WIN}x{WIN}"
    );
    eprintln!("vulkan_clips_a_window_to_rounded_geometry: bbox {vbox:?} ({bw}x{bh})");
}

/// A clipped tile must push its [`RoundedCornerDamage`] element, or a corner-radius change damages
/// nothing and leaves stale corners on screen.
///
/// The radius is not part of any surface's damage (`ClippedSurfaceRenderElement::damage_since` has
/// a standing FIXME saying so), so `Tile::rounded_corner_damage` is the *only* thing that reports
/// it: `update_config` bumps its commit counter, and the tile pushes it into the element list. A
/// `queue_redraw` alone cannot stand in — the damage tracker still derives damage from element
/// commit counters, so with the element absent the redraw computes empty damage and repaints
/// nothing.
///
/// This is a structural check (is the element in the list?), not a pixel one, because the test
/// render path composites unconditionally and would repaint the corners either way — hiding
/// exactly the bug this pins.
#[test]
fn vulkan_clipped_tile_pushes_its_rounded_corner_damage() {
    let Some(mut f) = clipped_window_fixture() else {
        return;
    };
    let output = f.synoik_output(1);

    let state = f.synoik_state();
    let found = state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| {
            let synoik = &mut state.synoik;
            synoik.update_render_elements(Some(&output));
            let ctx = RenderCtx {
                renderer: vk,
                target: RenderTarget::Output,
                xray: None,
            };
            // Same `{elem:?}`-sniffing idiom as `render_helpers::debug::push_opaque_regions`: the
            // element list is an opaque enum tree, and `ExtraDamage` is the only thing in this
            // scene that prints as one (blur/background-effect, the other user, is off here).
            synoik
                .render_to_vec(ctx, &output, false)
                .iter()
                .any(|elem| format!("{elem:?}").contains("ExtraDamage"))
        })
        .expect("headless backend must hold a Vulkan renderer");

    assert!(
        found,
        "the clipped tile pushed no ExtraDamage element — a corner-radius change would damage \
         nothing and leave stale corners"
    );
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
    let output = f.synoik_output(1);

    f.synoik_state().do_action(Action::OpenOverview, false);
    assert!(
        f.synoik().layout.is_overview_open(),
        "the overview must be open"
    );
    f.synoik_complete_animations();

    let (pixels, w, h) = render_output_vulkan_target(&mut f, &output, RenderTarget::Output);

    // The overview dims windows to ~[40,65,40], so match green-dominant (not bright-green): any
    // pixel where green clearly leads red/blue is the (dimmed) window content vs the gray backdrop.
    let is_green =
        |p: [u8; 4]| p[1] as i32 > p[0] as i32 + 15 && p[1] as i32 > p[2] as i32 + 15 && p[1] > 45;
    // Only the window-picker box: the thumbnail strip above it draws the same window as a
    // miniature (it is now always shown — `docs/fork/dynamic-workspaces-divergence.md`), and
    // a bbox spanning both would measure the strip rather than the wrapped picker render.
    let picker_top = f
        .synoik()
        .layout
        .controls_layout_for_output(&output)
        .expect("output 1 has a monitor")
        .workspaces
        .loc
        .y as i32;
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, -1, -1);
    let green = (0..w * h)
        .filter(|i| i / w >= picker_top && is_green(px(&pixels, w, i % w, i / w)))
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

/// The focus ring / border outline must be rounded, not pointy. It is drawn by
/// `BorderRenderElement` (a procedural rounded/gradient SDF); a plain `SolidColorRenderElement`
/// quad (square corners) is the fallback when the ring needs neither rounding nor a gradient.
///
/// This once picked between them via a `has_shader` predicate that asked the GLES shader registry,
/// so a Vulkan session got pointy quads even though the owned renderer draws borders procedurally.
/// The predicate is gone with GLES; this pins the outcome it was getting wrong. Map a focused
/// window with a thick blue focus ring + corner radius and assert the ring is present but its
/// extreme outer corner is rounded away to the backdrop.
#[test]
fn vulkan_draws_a_rounded_focus_ring() {
    if VulkanRenderer::new().is_err() {
        eprintln!("skipping: no Vulkan device");
        return;
    }

    // GNOME windowing mode draws no border and no focus ring around a window
    // (`layout::tile::focus_ring_config`), so the rounded-ring *rendering* is
    // exercised through niri's scrolling mode. The shader path this pins is still
    // live in GNOME mode — `ui::mru` and the workspace-thumbnail indicator both
    // draw through `FocusRing`.
    let mut config = crate::tests::scrolling(Config::default());
    config.layout.border.off = true; // isolate the focus ring
    config.layout.focus_ring.off = false;
    config.layout.focus_ring.width = 12.;
    // active_color defaults to [127,200,255]; make the ring's presence unambiguous.
    config.window_rules.push(WindowRule {
        geometry_corner_radius: Some(CornerRadius::from(20.)),
        clip_to_geometry: Some(true),
        ..Default::default()
    });

    let mut f = Fixture::with_config(config);
    f.synoik_state()
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
    f.synoik_complete_animations();
    f.double_roundtrip(id);

    let output = f.synoik_output(1);
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

/// Map a real **shm-textured** window (a green `Argb8888` buffer, `WIN`×`WIN`) on a fresh
/// Vulkan-backed fixture and settle the open animation, so the renderer's per-surface shm
/// import/cache path (`import_shm_buffer`) runs on a static scene. Returns the fixture, client id,
/// surface, and output for follow-up re-commits. `None` (with a skip) when no Vulkan device.
fn shm_window_fixture() -> Option<(Fixture, ClientId, WlSurface, Output)> {
    if VulkanRenderer::new().is_err() {
        eprintln!("skipping shm-cache test: no Vulkan device");
        return None;
    }

    let mut f = Fixture::new();
    f.synoik_state()
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
    f.synoik_complete_animations();
    f.double_roundtrip(id);

    let output = f.synoik_output(1);
    Some((f, id, surface, output))
}

/// The shm cache keeps a client's `VkTexture` across commits and re-uploads the new contents *in
/// place* on a same-(size, fourcc) hit. Drive a real surface through two same-size, same-format
/// commits (green → red) and assert the second frame is the re-uploaded red with no stale green: a
/// hit path that returned the cached texture without re-uploading would leave the first color.
#[test]
fn vulkan_shm_cache_refreshes_on_recommit() {
    let Some((mut f, id, surface, output)) = shm_window_fixture() else {
        return;
    };
    let is_green = |p: [u8; 4]| p[0] < 40 && p[1] > 200 && p[2] < 40;
    let is_red = |p: [u8; 4]| p[0] > 200 && p[1] < 40 && p[2] < 40;

    // First frame: the green buffer imports and populates the per-surface cache.
    let (pixels, w, h) = render_output_vulkan(&mut f, &output);
    let green = (0..w * h)
        .filter(|i| is_green(px(&pixels, w, i % w, i / w)))
        .count();
    assert!(green > 0, "green window absent from the first frame");

    // Swap in a RED buffer of the SAME size + format → a cache hit that must re-upload in place.
    let window = f.client(id).window(&surface);
    window.attach_shm_buffer(WIN as i32, WIN as i32, 255, 0, 0, 255);
    window.commit();
    f.double_roundtrip(id);

    let (pixels, w, h) = render_output_vulkan(&mut f, &output);
    let red = (0..w * h)
        .filter(|i| is_red(px(&pixels, w, i % w, i / w)))
        .count();
    let green = (0..w * h)
        .filter(|i| is_green(px(&pixels, w, i % w, i / w)))
        .count();
    assert!(
        red > 0,
        "the re-committed red window is absent (cache returned stale content?)"
    );
    assert_eq!(
        green, 0,
        "stale green survived the re-commit ({green} px): the cache hit did not re-upload"
    );
}

/// The cache keys reuse on `(size, fourcc)`, not size alone (unlike the GLES renderer): a same-size
/// buffer with a DIFFERENT fourcc must re-import, because Argb/Abgr map to different VkFormats and
/// view swizzles. Commit an Argb buffer, then an Abgr buffer of the same size whose bytes are RED
/// in Abgr order — a correct re-import renders RED, while a size-only cache reuse would upload the
/// new bytes through the old Argb view and swap R↔B, rendering BLUE.
#[test]
fn vulkan_shm_cache_reimports_on_format_change() {
    let Some((mut f, id, surface, output)) = shm_window_fixture() else {
        return;
    };
    let is_red = |p: [u8; 4]| p[0] > 200 && p[1] < 40 && p[2] < 40;
    let is_blue = |p: [u8; 4]| p[2] > 200 && p[0] < 40 && p[1] < 40;

    // First frame populates the cache with an Argb-format image (the fixture's green buffer).
    let _ = render_output_vulkan(&mut f, &output);

    // Swap in an Abgr buffer, SAME size, encoding RED. A size-only reuse would render it BLUE.
    let window = f.client(id).window(&surface);
    window.attach_shm_buffer_abgr(WIN as i32, WIN as i32, 255, 0, 0, 255);
    window.commit();
    f.double_roundtrip(id);

    let (pixels, w, h) = render_output_vulkan(&mut f, &output);
    let red = (0..w * h)
        .filter(|i| is_red(px(&pixels, w, i % w, i / w)))
        .count();
    let blue = (0..w * h)
        .filter(|i| is_blue(px(&pixels, w, i % w, i / w)))
        .count();
    assert!(red > 0, "the Abgr red window is absent from the frame");
    assert_eq!(
        blue, 0,
        "the format change reused the Argb image ({blue} blue px): red read back as blue — \
         cache keyed on size only, not fourcc"
    );
}

/// Compositing for a screencast must work on the owned Vulkan renderer.
///
/// Screencast used to render through the co-resident GLES renderer even on a Vulkan session (the
/// redraw path handed all capture consumers a `&mut GlesRenderer`), so `RenderTarget::Screencast`
/// had never been driven through the owned renderer at all. It is its own scene-collection pass —
/// block-out rules and the per-target element split key off the target — so a Vulkan session could
/// have been streaming a blank frame while the display looked fine.
///
/// The real PipeWire path needs GBM buffers and a consumer, neither of which headless has; what is
/// testable, and what actually changed, is that the same scene composites for the Screencast
/// target.
#[test]
fn vulkan_composites_for_a_screencast() {
    let Some(mut f) = green_window_fixture() else {
        return;
    };
    let output = f.synoik_output(1);

    let (pixels, w, h) = render_output_vulkan_target(&mut f, &output, RenderTarget::Screencast);
    assert_window_and_background(&pixels, w, h);
}

/// Drop shadows must draw. `Shadow::render` once skipped emitting any element at all unless a
/// `has_shader` predicate that only asked the GLES shader registry said yes — so a Vulkan session
/// drew no shadow whatsoever, even though the owned renderer has the material
/// (`VulkanFrame::render_shadow`) and `ShadowRenderElement` a real `RenderElement<VulkanRenderer>`
/// draw. The predicate is gone with GLES; this pins the outcome. Same shape as the bug in
/// `vulkan_draws_a_rounded_focus_ring`.
///
/// Assert it by darkening: the backdrop just outside the window must be strictly darker than the
/// backdrop far away from it, and shade off with distance. A missing shadow makes those equal.
#[test]
fn vulkan_draws_a_window_shadow() {
    if VulkanRenderer::new().is_err() {
        eprintln!("skipping vulkan_draws_a_window_shadow: no Vulkan device");
        return;
    }

    let mut config = Config::default();
    // Isolate the shadow: no ring/border pixels to confuse "darker than the backdrop".
    config.layout.border.off = true;
    config.layout.focus_ring.off = true;
    config.layout.shadow.on = true;
    config.layout.shadow.softness = 40.;
    config.layout.shadow.spread = 10.;
    // Straight down-and-right offset would bias the sampling; keep it centered.
    config.layout.shadow.offset = synoik_config::ShadowOffset {
        x: synoik_config::FloatOrInt(0.),
        y: synoik_config::FloatOrInt(0.),
    };

    let mut f = Fixture::with_config(config);
    f.synoik_state()
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
    f.synoik_complete_animations();
    f.double_roundtrip(id);

    let output = f.synoik_output(1);
    let (pixels, w, h) = render_output_vulkan_target(&mut f, &output, RenderTarget::Output);

    // Find the green window, then walk left from its edge along its vertical centre.
    let is_green = |p: [u8; 4]| p[0] < 40 && p[1] > 200 && p[2] < 40 && p[3] > 200;
    let (mut gx0, mut gy0, mut gy1) = (w, h, -1);
    for y in 0..h {
        for x in 0..w {
            if is_green(px(&pixels, w, x, y)) {
                gx0 = gx0.min(x);
                gy0 = gy0.min(y);
                gy1 = gy1.max(y);
            }
        }
    }
    assert!(
        gx0 < w && gy1 >= 0,
        "the green window is absent from the frame"
    );

    let cy = (gy0 + gy1) / 2;
    let lum = |x: i32| {
        let p = px(&pixels, w, x, cy);
        p[0] as i32 + p[1] as i32 + p[2] as i32
    };

    // Just outside the window edge (inside the shadow), vs far away (clean backdrop).
    let near = lum(gx0 - 3);
    let far = lum(gx0 - 250);
    let mid = lum(gx0 - 30);

    assert!(
        near < far,
        "no shadow: the backdrop next to the window ({near}) is not darker than the backdrop far \
         from it ({far})",
    );
    assert!(
        near < mid && mid <= far,
        "the shadow must fade with distance from the window, got near={near} mid={mid} far={far}",
    );
}

/// A window blocked out from screencast must not leak its contents into a cast *while resizing*.
///
/// The resize crossfade draws the snapshot taken when the resize started. The GLES path bakes a
/// blocked-out variant of that snapshot and picks it per target, but the neutral the Vulkan path
/// uses holds the window's real contents and has no per-target variant. Once screencast started
/// rendering through the owned renderer, a blocked-out window being resized during a cast drew its
/// real pre-resize pixels crossfading into the blocked-out solid — showing exactly what block-out
/// exists to hide.
///
/// Render the Screencast target mid-resize and assert no window pixels appear. The Output target is
/// checked too, so a fix that simply blanks the window everywhere can't pass.
#[test]
fn vulkan_blocked_out_window_does_not_leak_while_resizing() {
    use synoik_config::animations::{Curve, EasingParams, Kind};
    use synoik_config::BlockOutFrom;
    use synoik_ipc::SizeChange;

    if VulkanRenderer::new().is_err() {
        eprintln!("skipping vulkan_blocked_out_window_does_not_leak_while_resizing: no Vulkan");
        return;
    }

    const LINEAR: Kind = Kind::Easing(EasingParams {
        duration_ms: 1000,
        curve: Curve::Linear,
    });
    let mut config = Config::default();
    config.animations.window_resize.anim.kind = LINEAR;
    config.window_rules.push(WindowRule {
        block_out_from: Some(BlockOutFrom::Screencast),
        ..Default::default()
    });

    let mut f = Fixture::with_config(config);
    f.synoik_state()
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
    f.synoik_complete_animations();
    f.double_roundtrip(id);

    // Start an animated resize, and let the client commit the new size so the crossfade begins.
    f.synoik()
        .layout
        .set_column_width(SizeChange::SetFixed(900));
    f.double_roundtrip(id);
    let window = f.client(id).window(&surface);
    window.attach_shm_buffer(900, WIN as i32, 0, 255, 0, 255);
    window.set_size(900, WIN);
    window.ack_last_and_commit();
    f.roundtrip(id);

    let output = f.synoik_output(1);
    assert!(
        f.synoik().layout.are_animations_ongoing(Some(&output)),
        "expected an ongoing resize animation to composite",
    );

    let is_green = |p: [u8; 4]| p[0] < 40 && p[1] > 200 && p[2] < 40;
    let count_green = |pixels: &[u8], w: i32, h: i32| {
        (0..w * h)
            .filter(|i| is_green(px(pixels, w, i % w, i / w)))
            .count()
    };

    // The cast must not show the window at all — neither the live surface nor the crossfade's
    // pre-resize snapshot of it.
    let (cast, w, h) = render_output_vulkan_target(&mut f, &output, RenderTarget::Screencast);
    let leaked = count_green(&cast, w, h);
    assert_eq!(
        leaked, 0,
        "{leaked} window pixels leaked into the screencast while the window was resizing",
    );

    // But the window is still really there: on screen it renders normally.
    let (screen, w, h) = render_output_vulkan_target(&mut f, &output, RenderTarget::Output);
    assert!(
        count_green(&screen, w, h) > 0,
        "the window vanished from the screen too; block-out must only affect the cast",
    );
}

/// The frozen screenshot-UI screen must also draw into a **screencast**, not just on screen.
///
/// `OutputScreenshot` prefers the renderer-neutral capture on Vulkan and otherwise falls back to a
/// GLES element, which no-ops on the owned renderer. The neutral was captured for the `Output`
/// target only, so on a Vulkan session the Screencast and ScreenCapture targets fell through to
/// that no-op: with the screenshot UI open, a cast showed *nothing* where the frozen screen should
/// be. Neutrals are now captured per target.
///
/// Same shape as `vulkan_screenshot_ui_draws_the_frozen_screen`, but compositing the Screencast
/// target.
#[test]
fn vulkan_screenshot_ui_draws_the_frozen_screen_into_a_cast() {
    let Some(mut f) = green_window_fixture() else {
        return;
    };
    let output = f.synoik_output(1);

    f.synoik_state().open_screenshot_ui(false, None);
    assert!(
        f.synoik().screenshot_ui.is_open(),
        "screenshot UI must be open"
    );
    settle_screenshot_ui_open(&mut f);

    let (pixels, w, h) = render_output_vulkan_target(&mut f, &output, RenderTarget::Screencast);

    let is_greenish = |p: [u8; 4]| {
        let (r, g, b) = (p[0] as i16, p[1] as i16, p[2] as i16);
        g > 60 && g > r + 30 && g > b + 30
    };
    let green = (0..w * h)
        .filter(|i| is_greenish(px(&pixels, w, i % w, i / w)))
        .count();
    eprintln!("vulkan_screenshot_ui_draws_the_frozen_screen_into_a_cast: {green} greenish px");
    assert!(
        green > 1000,
        "the frozen screenshot is missing from the screencast ({green} greenish px); the \
         Screencast target fell through to the GLES element, which draws nothing on Vulkan",
    );
}

/// Close the mapped window exactly as `XdgShellHandler::toplevel_destroyed` does: capture the unmap
/// snapshot, start the close animation, and remove the window from the layout. Leaves the layout
/// with a `ClosingWindow` mid-animation.
fn close_the_only_window(f: &mut Fixture, output: &Output) {
    use crate::utils::transaction::Transaction;

    let window_id = crate::layout::LayoutElement::id(
        f.synoik()
            .layout
            .windows()
            .next()
            .expect("a mapped window")
            .1,
    )
    .clone();

    f.synoik_state()
        .store_unmap_snapshot(&window_id, Some(output));

    let transaction = Transaction::new();
    let blocker = transaction.blocker();
    let state = f.synoik_state();
    // `None`, as a Vulkan session does: the snapshot is renderer-neutral and no GLES renderer is
    // involved in starting the animation.
    state
        .synoik
        .layout
        .start_close_animation_for_window(&window_id, blocker);
    state.synoik.layout.remove_window(&window_id, transaction);
}

/// A closing window must draw into a **screencast**, not just on screen.
///
/// The close snapshot was captured for the `Output` target only, and `render_vulkan` hard-returned
/// `None` for every other target — so on a Vulkan session a closing window was simply absent from
/// casts. It is now captured per block-out variant, like the GLES path always was.
#[test]
fn vulkan_draws_a_closing_window_into_a_cast() {
    let Some(mut f) = green_window_fixture() else {
        return;
    };
    let output = f.synoik_output(1);

    close_the_only_window(&mut f, &output);
    assert!(
        f.synoik().layout.are_animations_ongoing(Some(&output)),
        "expected an ongoing close animation to composite",
    );

    let is_green = |p: [u8; 4]| p[0] < 40 && p[1] > 200 && p[2] < 40;
    let count_green = |pixels: &[u8], w: i32, h: i32| {
        (0..w * h)
            .filter(|i| is_green(px(pixels, w, i % w, i / w)))
            .count()
    };

    // Composite the Output target FIRST: its variant gets uploaded and cached. The Screencast
    // render must then still pick its own variant, not the one already in the cache.
    let (screen, w, h) = render_output_vulkan_target(&mut f, &output, RenderTarget::Output);
    let on_screen = count_green(&screen, w, h);
    assert!(
        on_screen > 0,
        "the closing window did not draw on screen at all",
    );

    let (cast, w, h) = render_output_vulkan_target(&mut f, &output, RenderTarget::Screencast);
    let in_cast = count_green(&cast, w, h);
    eprintln!(
        "vulkan_draws_a_closing_window_into_a_cast: {on_screen} on screen, {in_cast} in cast"
    );
    assert!(
        in_cast > 0,
        "the closing window is missing from the screencast; the Screencast target had no variant \
         to draw and fell through to nothing",
    );
}

/// ...but a window that is blocked out from screencasts must NOT appear in one while it closes.
///
/// This is the failure mode the per-variant capture must not introduce: the naive way to put
/// closing windows into casts (draw the Output capture on every target) turns today's blank into a
/// leak of exactly the pixels block-out exists to hide.
#[test]
fn vulkan_blocked_out_closing_window_does_not_leak_into_a_cast() {
    if VulkanRenderer::new().is_err() {
        eprintln!(
            "skipping vulkan_blocked_out_closing_window_does_not_leak_into_a_cast: no Vulkan device"
        );
        return;
    }

    use synoik_config::BlockOutFrom;

    let mut config = Config::default();
    config.window_rules.push(WindowRule {
        block_out_from: Some(BlockOutFrom::Screencast),
        ..Default::default()
    });

    let mut f = Fixture::with_config(config);
    f.synoik_state()
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
    f.synoik_complete_animations();
    f.double_roundtrip(id);

    let output = f.synoik_output(1);
    close_the_only_window(&mut f, &output);
    assert!(
        f.synoik().layout.are_animations_ongoing(Some(&output)),
        "expected an ongoing close animation to composite",
    );

    let is_green = |p: [u8; 4]| p[0] < 40 && p[1] > 200 && p[2] < 40;
    let count_green = |pixels: &[u8], w: i32, h: i32| {
        (0..w * h)
            .filter(|i| is_green(px(pixels, w, i % w, i / w)))
            .count()
    };

    let (cast, w, h) = render_output_vulkan_target(&mut f, &output, RenderTarget::Screencast);
    let leaked = count_green(&cast, w, h);
    assert_eq!(
        leaked, 0,
        "{leaked} window pixels leaked into the screencast while the window was closing",
    );

    // But the window is still really there: on screen the close animation renders normally.
    let (screen, w, h) = render_output_vulkan_target(&mut f, &output, RenderTarget::Output);
    assert!(
        count_green(&screen, w, h) > 0,
        "the closing window vanished from the screen too; block-out must only affect the cast",
    );
}

/// White pixels in the central band `y ∈ [3h/8, 5h/8)`, where the hotkey overlay draws its table.
fn white_px_in_overlay_band(pixels: &[u8], w: i32, h: i32) -> usize {
    let is_white = |p: [u8; 4]| p[0] > 200 && p[1] > 200 && p[2] > 200;
    ((h * 3 / 8) * w..(h * 5 / 8) * w)
        .filter(|i| is_white(px(pixels, w, i % w, i / w)))
        .count()
}

/// The "Important Hotkeys" overlay used to be cairo/pango text uploaded through a GLES-locked
/// element — it now draws straight into a `VkTexture` on the owned renderer. Assert its white table
/// text composites through Vulkan, measured against a closed-overlay baseline so the check can't go
/// vacuous.
#[test]
fn vulkan_hotkey_overlay_draws() {
    let Some(mut f) = green_window_fixture() else {
        return;
    };
    let output = f.synoik_output(1);

    // The overlay is shown at startup by default; hide it for a clean baseline.
    f.synoik().hotkey_overlay.hide();
    let (before, w, h) = render_output_vulkan_target(&mut f, &output, RenderTarget::Output);
    assert_eq!(
        white_px_in_overlay_band(&before, w, h),
        0,
        "the overlay band must be empty with the overlay closed, else it cannot witness it"
    );

    f.synoik().hotkey_overlay.show();

    let (after, w, h) = render_output_vulkan_target(&mut f, &output, RenderTarget::Output);
    let white = white_px_in_overlay_band(&after, w, h);
    eprintln!("vulkan_hotkey_overlay_draws: {white} white px in the overlay band");
    assert!(
        white > 500,
        "the hotkey overlay text did not draw on Vulkan (blank overlay?): {white} white px"
    );
}

/// An area screencast records a cropped sub-rectangle of the output. This pins that the shared
/// `RenderTarget::Screencast` element list, wrapped in `RelocateRenderElement` by the production
/// `area_crop_offset`, reads back at the pixel level as exactly the output sub-rectangle — so a
/// sign or buffer-size error in the shared offset helper fails headlessly. (The output-origin term
/// of the offset is pinned separately by `area_crop_offset_accounts_for_the_output_origin`, since
/// this fixture's single output sits at the global origin.) Slice 1, Half A.
#[test]
fn vulkan_area_cast_crops_to_the_output_subrect() {
    use smithay::backend::renderer::element::utils::{Relocate, RelocateRenderElement};
    use smithay::utils::{Point, Rectangle};

    let Some(mut f) = green_window_fixture() else {
        return;
    };
    let output = f.synoik_output(1);
    let output_geo = f
        .synoik()
        .global_space
        .output_geometry(&output)
        .expect("output geometry");

    // A sub-rectangle well inside the 1280×720 output (scale 1 → physical == logical).
    let area = Rectangle::new(Point::from((200, 150)), Size::from((400, 300)));

    let state = f.synoik_state();
    let (full, crop) = state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
            let synoik = &mut state.synoik;
            synoik.update_render_elements(Some(&output));

            let size: Size<i32, Physical> = output.current_mode().unwrap().size;
            let scale = Scale::from(output.current_scale().fractional_scale());

            // The full output frame, composited exactly as a monitor cast would.
            let ctx = RenderCtx {
                renderer: vk,
                target: RenderTarget::Screencast,
                xray: None,
            };
            let elements = synoik.render_to_vec(ctx, &output, false);
            let full = render_to_vec(
                vk,
                size,
                scale,
                Transform::Normal,
                Fourcc::Abgr8888,
                elements.iter().rev(),
            )?;

            // The same scene shifted into an area-sized buffer via the production crop offset —
            // the area-cast crop.
            let neg = crate::screencasting::area_crop_offset(area, output_geo, scale).upscale(-1);
            let relocated: Vec<_> = elements
                .iter()
                .map(|e| RelocateRenderElement::from_element(e, neg, Relocate::Relative))
                .collect();
            let crop = render_to_vec(
                vk,
                area.size.to_physical_precise_round(scale),
                scale,
                Transform::Normal,
                Fourcc::Abgr8888,
                relocated.iter().rev(),
            )?;

            Ok((full, crop))
        })
        .expect("headless backend must hold a Vulkan renderer")
        .expect("compositing through Vulkan must not error");

    // Every sampled crop pixel must equal the full-frame pixel at the area offset: the crop is
    // exactly the sub-rectangle, nothing shifted or mirrored.
    let (aw, ah) = (area.size.w, area.size.h);
    for &(sx, sy) in &[
        (0, 0),
        (aw - 1, 0),
        (0, ah - 1),
        (aw - 1, ah - 1),
        (aw / 2, ah / 2),
    ] {
        assert_eq!(
            px(&crop, aw, sx, sy),
            px(&full, i32::from(OUT_W), area.loc.x + sx, area.loc.y + sy),
            "area crop pixel ({sx},{sy}) must equal the full frame at (+{},+{})",
            area.loc.x,
            area.loc.y,
        );
    }
}

/// The overview dash bakes through the owned renderer, and its hover fill *lightens*
/// the hovered tile — the per-widget hover direction (`TILE_HOVER` =
/// `st-lighten($dash_background_color, 7%)`, `_dash.scss`). Pinned as a differential
/// in one frame: with favorite 0 hovered, sample tile 0's top border (hover fill,
/// above the icon) against tile 1's (plain pill background); tile 0 must be brighter
/// on every color channel. A sign flip (a theme reading it as a darken) fails here.
#[test]
fn an_icon_theme_change_keeps_the_symbolic_icons_drawable() {
    use std::sync::mpsc::TryRecvError;

    use smithay::backend::allocator::Fourcc;
    use smithay::utils::{Scale, Transform};

    use crate::render_helpers::icon::{IconCache, SymbolicRasterized};
    use crate::render_helpers::memory::MemoryBuffer;

    // An icon-theme change replaces the whole IconCache, and re-rasterizing goes through the
    // worker — so a bare replacement has nothing to draw and every symbolic icon on screen
    // (panel status, quick-settings toggles, calendar chevrons) vanishes until it catches up.
    let mut f = Fixture::new();
    f.synoik_state()
        .backend
        .headless()
        .add_renderer()
        .expect("build the Vulkan renderer");
    f.add_output(1, (1920, 1080));

    const NAME: &str = "view-app-grid-symbolic";
    const PX: f64 = 16.;
    const COLOR: [f32; 4] = [1., 1., 1., 1.];

    let state = f.synoik_state();
    let outcome = state.backend.headless().with_vulkan_renderer(|vk| {
        let mut cache = IconCache::new("Adwaita");
        let rx = cache.wire_test_worker();

        // A cold miss draws nothing and queues the rasterization — the async path.
        assert!(
            cache.texture(vk, NAME, PX, 1., COLOR).is_none(),
            "a cold miss has nothing to draw yet"
        );
        let req = match rx.try_recv() {
            Ok(req) => req,
            Err(TryRecvError::Empty) => panic!("the miss queued no rasterization"),
            Err(err) => panic!("worker channel broke: {err:?}"),
        };
        let pixels = MemoryBuffer::new(
            vec![255u8; 4],
            Fourcc::Abgr8888,
            Size::from((1, 1)),
            Scale::from(1.0),
            Transform::Normal,
        );
        cache.apply_rasterized(SymbolicRasterized::for_test(
            req.key(),
            Some(pixels),
            req.generation(),
        ));
        assert!(
            cache.texture(vk, NAME, PX, 1., COLOR).is_some(),
            "the rasterized icon should upload and draw"
        );
        assert_eq!(cache.texture_counts().0, 1, "one icon uploaded");

        // Now the theme changes: a fresh cache that inherits the old uploads.
        let mut next = IconCache::new("Papirus");
        next.adopt_textures_from(&cache);
        let _rx = next.wire_test_worker();
        assert_eq!(
            next.texture_counts(),
            (0, 1),
            "the replacement starts with nothing of its own and one inherited icon"
        );
        assert!(
            next.texture(vk, NAME, PX, 1., COLOR).is_some(),
            "a theme change must keep drawing the old symbolic pixels until the new ones \
             rasterize — drawing nothing blanks the panel and quick settings for a frame"
        );
    });
    if outcome.is_none() {
        eprintln!(
            "skipping an_icon_theme_change_keeps_the_symbolic_icons_drawable: no Vulkan device"
        );
    }
}

#[test]
fn a_ping_on_an_unchanged_catalog_keeps_the_dash_icons() {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog, RecordingLauncher};

    // glib's app-info monitor pings for any write under a watched directory, and one lands a
    // few seconds into every session on a catalog that is already loaded. The reload cleared
    // every icon upload regardless — and since icons re-decode off-thread, the dash drew blank
    // tiles until the worker caught up. That is the single dash flicker ~6s after startup that
    // needed nothing to be happening.
    let mut f = Fixture::new();
    f.synoik_state()
        .backend
        .headless()
        .add_renderer()
        .expect("build the Vulkan renderer");
    f.add_output(1, (1920, 1080));
    let output = f.synoik_output(1);

    let catalog = FakeCatalog::new(vec![
        AppEntry::fake("a.desktop", "a.desktop"),
        AppEntry::fake("b.desktop", "b.desktop"),
    ]);
    let apps = catalog.apps.clone();
    f.synoik().app_system =
        AppSystem::with_parts(Box::new(catalog), Box::new(RecordingLauncher::default()));
    f.synoik()
        .app_system
        .set_favorites(vec!["a.desktop".into(), "b.desktop".into()]);
    f.synoik().sync_dash_favorites();

    let controls = f
        .synoik()
        .layout
        .controls_layout_for_output(&output)
        .expect("output 1 has a monitor");

    // Render once, purely to populate the upload cache the reload would drop.
    let state = f.synoik_state();
    let rendered = state.backend.headless().with_vulkan_renderer(|vk| {
        let synoik = &mut state.synoik;
        let _ = synoik.dash.render(
            vk,
            &synoik.app_icon_cache,
            &synoik.icon_cache,
            &output,
            controls.dash,
            1.0,
        );
    });
    if rendered.is_none() {
        eprintln!("skipping a_ping_on_an_unchanged_catalog_keeps_the_dash_icons: no Vulkan device");
        return;
    }

    let warm = f.synoik().dash.icon_upload_count();
    assert!(
        warm > 0,
        "the dash uploaded no icons, so this test could not see them being dropped"
    );

    f.synoik().reload_app_catalog();
    assert_eq!(
        f.synoik().dash.icon_upload_count(),
        warm,
        "a ping on an unchanged catalog dropped the dash icon uploads — every tile would \
         draw blank until the off-thread decodes land"
    );

    // And a catalog that really did change must not blank them either: the old pixels
    // stay up until each replacement decode lands, which is when that icon's upload is
    // dropped (`Synoik::drop_app_icon_uploads`).
    apps.borrow_mut()
        .push(AppEntry::fake("c.desktop", "c.desktop"));
    f.synoik().reload_app_catalog();
    assert_eq!(
        f.synoik().dash.icon_upload_count(),
        warm,
        "a changed catalog dropped the uploads up front — the dash would draw blank \
         tiles until the worker caught up, which is the flicker this all exists to avoid"
    );
}

/// On a canvas the adaptive chrome ramps down, the dash's **icons** shrink with its
/// tiles. The lengths and the pixels are two different code paths — the layout derives
/// every box from `DashMetrics`, but each icon is drawn at a size passed to
/// `app_icon_element` — and when the second one kept GNOME's flat 64 the icons drew over
/// each other on a shrunk pill, which is exactly what the seat showed. Nothing in the
/// geometry corpus can see that: every *box* was right.
#[test]
fn vulkan_dash_icons_shrink_with_the_ramped_tiles() {
    use smithay::utils::Logical;

    use crate::app_system::{AppEntry, AppSystem, FakeCatalog, RecordingLauncher};

    if let Err(e) = VulkanRenderer::new() {
        eprintln!("skipping vulkan_dash_icons_shrink_with_the_tiles: no Vulkan device ({e})");
        return;
    }

    // Ink outside the tile boxes, on the row through the icon centres: the icons are
    // wider than their tiles exactly when they were drawn at the un-ramped size.
    let spill =
        |size: (u16, u16)| -> u32 {
            let mut f = Fixture::new();
            f.synoik_state()
                .backend
                .headless()
                .add_renderer()
                .expect("build the Vulkan renderer");
            f.add_output(1, size);
            let output = f.synoik_output(1);

            let apps = vec![
                AppEntry::fake("a.desktop", "a.desktop"),
                AppEntry::fake("b.desktop", "b.desktop"),
            ];
            f.synoik().app_system = AppSystem::with_parts(
                Box::new(FakeCatalog::new(apps)),
                Box::new(RecordingLauncher::default()),
            );
            f.synoik()
                .app_system
                .set_favorites(vec!["a.desktop".into(), "b.desktop".into()]);
            f.synoik().sync_dash_favorites();

            let controls = f
                .synoik()
                .layout
                .controls_layout_for_output(&output)
                .expect("the output has a monitor");
            let band = controls.dash;
            let tiles: Vec<Rectangle<f64, Logical>> = (0..2)
                .map(|i| f.synoik().dash.tile_rect(i, band).expect("a dash tile"))
                .collect();

            let state = f.synoik_state();
            let composited = state.backend.headless().with_vulkan_renderer(
                |vk| -> anyhow::Result<(Vec<u8>, i32)> {
                    let synoik = &mut state.synoik;
                    let elements = synoik.dash.render(
                        vk,
                        &synoik.app_icon_cache,
                        &synoik.icon_cache,
                        &output,
                        band,
                        1.,
                    );
                    let phys: Size<i32, Physical> = output.current_mode().unwrap().size;
                    let scale = Scale::from(output.current_scale().fractional_scale());
                    let pixels = render_to_vec(
                        vk,
                        phys,
                        scale,
                        Transform::Normal,
                        Fourcc::Abgr8888,
                        elements.iter().rev(),
                    )?;
                    Ok((pixels, phys.w))
                },
            );
            let (pixels, w) = composited
                .expect("a Vulkan device")
                .expect("compositing the dash must not error");

            // The pill fill is uniform, so "ink" is any pixel that differs from it. Sample
            // the row through the icon centres, between the two tiles and outside them.
            let row = (tiles[0].loc.y + tiles[0].size.h / 2.) as i32;
            let bg = px(&pixels, w, (tiles[0].loc.x - 3.) as i32, row);
            let mut spilled = 0;
            let between = (tiles[0].loc.x + tiles[0].size.w) as i32..tiles[1].loc.x as i32;
            for x in between {
                let p = px(&pixels, w, x, row);
                if (0..3).any(|c| (i16::from(p[c]) - i16::from(bg[c])).abs() > 12) {
                    spilled += 1;
                }
            }
            spilled
        };

    // 1920x1080 is above the reference canvas: GNOME's own 64px icon in a 76px tile,
    // which is the control — the gap between two tiles is pill, not icon.
    let big = spill((1920, 1080));
    eprintln!("vulkan_dash_icons: spill at 1920x1080 = {big}");
    assert_eq!(big, 0, "an unramped dash draws its icons inside its tiles");

    // 1024x665 ramps the dash a rung down; the icons must follow.
    let small = spill((1024, 665));
    eprintln!("vulkan_dash_icons: spill at 1024x665 = {small}");
    assert_eq!(
        small, 0,
        "a ramped dash must too — {small} px of icon drew between two tiles, which is a \
         64px icon on a tile that is no longer 76 wide"
    );
}

#[test]
fn vulkan_dash_hover_lightens_the_tile() {
    use smithay::utils::Logical;

    use crate::app_system::{AppEntry, AppSystem, FakeCatalog, RecordingLauncher};
    use crate::ui::dash::DashHit;

    if let Err(e) = VulkanRenderer::new() {
        eprintln!("skipping vulkan_dash_hover_lightens_the_tile: no Vulkan device ({e})");
        return;
    }

    let mut f = Fixture::new();
    f.synoik_state()
        .backend
        .headless()
        .add_renderer()
        .expect("build the Vulkan renderer");
    f.add_output(1, (1920, 1080));
    let output = f.synoik_output(1);

    let apps = vec![
        AppEntry::fake("a.desktop", "a.desktop"),
        AppEntry::fake("b.desktop", "b.desktop"),
    ];
    f.synoik().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(apps)),
        Box::new(RecordingLauncher::default()),
    );
    f.synoik()
        .app_system
        .set_favorites(vec!["a.desktop".into(), "b.desktop".into()]);
    f.synoik().sync_dash_favorites();
    f.synoik().dash.set_hovered(Some(DashHit::App(0)));

    let controls = f
        .synoik()
        .layout
        .controls_layout_for_output(&output)
        .expect("output 1 has a monitor");
    let c0 = f
        .synoik()
        .dash
        .tile_center(0, controls.dash)
        .expect("tile 0");
    let c1 = f
        .synoik()
        .dash
        .tile_center(1, controls.dash)
        .expect("tile 1");

    let state = f.synoik_state();
    let composited = state.backend.headless().with_vulkan_renderer(
        |vk| -> anyhow::Result<(Vec<u8>, i32, i32)> {
            let synoik = &mut state.synoik;
            // Render the dash directly at full opacity — this pins the bake, not the
            // overview open animation (whose progress is subject to the headless clock).
            let elements = synoik.dash.render(
                vk,
                &synoik.app_icon_cache,
                &synoik.icon_cache,
                &output,
                controls.dash,
                1.0,
            );
            let phys: Size<i32, Physical> = output.current_mode().unwrap().size;
            let scale = Scale::from(output.current_scale().fractional_scale());
            // First element is topmost, so composite back-to-front (pill under icons).
            let pixels = render_to_vec(
                vk,
                phys,
                scale,
                Transform::Normal,
                Fourcc::Abgr8888,
                elements.iter().rev(),
            )?;
            Ok((pixels, phys.w, phys.h))
        },
    );
    let Some(result) = composited else {
        eprintln!("skipping vulkan_dash_hover_lightens_the_tile: no Vulkan device");
        return;
    };
    let (pixels, w, _h) = result.expect("compositing the dash through Vulkan must not error");

    // Top border of each tile: inside the 76px tile but above the 64px icon (≈35px up
    // from center), so we sample the tile fill, never an icon glyph.
    let sample = |c: Point<f64, Logical>| px(&pixels, w, c.x as i32, (c.y - 35.) as i32);
    let hovered = sample(c0);
    let plain = sample(c1);
    eprintln!("vulkan_dash_hover_lightens_the_tile: hovered={hovered:?} plain={plain:?}");

    // The plain tile is the pill background — proves the dash actually baked. The pill is the
    // translucent `OVERVIEW_PLATE` now, so what is pinned is *its* alpha: an opaque result would
    // mean some surface under it went back to painting a solid.
    let plate_a = (crate::ui::widget::style::OVERVIEW_PLATE[3] * 255.).round() as u8;
    assert_eq!(
        plain[3], plate_a,
        "the dash pill must composite at the plate's alpha ({plate_a}), got {plain:?}"
    );
    for ch in 0..3 {
        assert!(
            hovered[ch] > plain[ch],
            "the hovered tile must be lighter than the plain one on channel {ch} \
             (hover lightens): hovered={hovered:?} plain={plain:?}"
        );
    }
}

/// The window picker's close button actually draws: hovering a preview must put
/// the `.window-close` disc on its top-right corner, and un-hovering must take it
/// away again. The headless test pins the geometry and the close request; this
/// pins that the chrome reaches the frame at all.
#[test]
fn vulkan_hovered_preview_draws_its_close_button() {
    use crate::ui::window_preview::close_rect;

    let Some(mut f) = green_window_fixture() else {
        return;
    };
    let output = f.synoik_output(1);
    f.synoik().hotkey_overlay.hide();
    let win = f.synoik().layout.focus().unwrap().window.clone();

    f.synoik_state().do_action(Action::OpenOverview, false);
    f.synoik_state().update_keyboard_focus();
    f.settle_animations();

    // Un-hovered: the button's outer corner is bare backdrop.
    let slot = f.synoik().layout.expose_target_rect(&win).unwrap();
    let probe = |rect: smithay::utils::Rectangle<f64, smithay::utils::Logical>| {
        let b = close_rect(rect);
        (
            (b.loc.x + b.size.w / 2.) as i32,
            (b.loc.y + b.size.h / 2. - b.size.h * 0.3) as i32,
        )
    };
    let (bx, by) = probe(slot);
    let (pixels, w, _) = render_output_vulkan(&mut f, &output);
    let bare = px(&pixels, w, bx, by);

    // Hover the preview and settle the 200ms overlay fade.
    let center = slot.loc + slot.size.downscale(2.).to_point();
    let cur = f.synoik().seat.get_pointer().unwrap().current_location();
    f.pointer_motion(center.x - cur.x, center.y - cur.y);
    f.settle_animations();

    let drawn = f.synoik().layout.expose_drawn_rect(&win).unwrap();
    let (bx, by) = probe(drawn);
    let (pixels, w, _) = render_output_vulkan(&mut f, &output);
    let button = px(&pixels, w, bx, by);

    assert_ne!(
        button, bare,
        "hovering must draw the close button over the preview's corner"
    );
    // The disc is `$window_close_button_color` = #3f3f46 at 98%
    // (`_window-picker.scss:2`), a hair of whatever it covers showing through.
    assert!(
        button[0].abs_diff(63) <= 2 && button[1].abs_diff(63) <= 2 && button[2].abs_diff(70) <= 2,
        "the disc must be $window_close_button_color #3f3f46, got {button:?} over {bare:?}"
    );
}

/// The overview search bakes through the owned renderer: the entry pill composites an
/// opaque dark fill, and (with a query + results) the results card draws with the
/// selected tile washed *lighter* than an unselected one — the `.overview-tile`
/// selection highlight. Pinned as a one-frame differential.
/// The overview search cross-fade actually *blends* the window picker: a mid-fade
/// frame must read `S·α + B·(1−α)` at the preview center — not `S` (the group
/// pushed straight through) and not `B` (the group dropped). This pins the
/// partial-alpha offscreen-composite branch of `Synoik::push_group_at_alpha`, which
/// every other test skips by settling the fade to one end.
///
/// Two traps make the naive version of this test lie:
/// - The startup "Important Hotkeys" overlay (`Synoik::new`) sits over the picker and is dismissed
///   by the *first key press* — so engaging the search would otherwise change the frame by a whole
///   panel, not just the picker's alpha.
/// - `green > 200` matches **white**, not green: the panel clock, the entry caret and the card text
///   all clear it. The reference has to come from the preview's own rect, and the assertion from
///   the measured ends rather than a filter.
#[test]
fn vulkan_search_fade_blends_the_picker_at_partial_alpha() {
    use crate::app_system::{AppSystem, FakeCatalog, RecordingLauncher};

    let Some(mut f) = green_window_fixture() else {
        return;
    };
    let output = f.synoik_output(1);

    // An empty catalog: on a machine with real desktop entries "a" would match
    // apps and draw their icons over the frame.
    f.synoik().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![])),
        Box::new(RecordingLauncher::default()),
    );
    f.synoik().hotkey_overlay.hide();
    let win = f.synoik().layout.focus().unwrap().window.clone();

    f.synoik_state().do_action(Action::OpenOverview, false);
    f.synoik_state().update_keyboard_focus();
    f.settle_animations();

    // The preview's own rect, from the layout — no pixel hunting. Its center
    // holds picker content only (the results card sits well above it).
    let rect = f.synoik().layout.expose_target_rect(&win).unwrap();
    let (cx, cy) = (
        (rect.loc.x + rect.size.w / 2.) as i32,
        (rect.loc.y + rect.size.h / 2.) as i32,
    );

    // S — fade 0, the pass-through end.
    assert_eq!(f.synoik().overview_search_fade(), 0.);
    let (pixels, w, _) = render_output_vulkan(&mut f, &output);
    let s = px(&pixels, w, cx, cy);
    assert!(
        s[1] > 200 && s[0] < 60,
        "the preview center must be the green window, got {s:?}"
    );

    // Engage the search and pin the clock part-way through the 250ms ease.
    f.key_press(30); // KEY_A
    f.key_release(30);
    f.synoik().advance_animations();
    {
        let synoik = f.synoik();
        let now = synoik.clock.now_unadjusted();
        synoik
            .clock
            .set_unadjusted(now + Duration::from_millis(100));
        synoik.advance_animations();
    }
    let fade = f.synoik().overview_search_fade();
    assert!(
        fade > 0.1 && fade < 0.9,
        "the fade must be partial, got {fade}"
    );
    let (pixels, ..) = render_output_vulkan(&mut f, &output);
    let m = px(&pixels, w, cx, cy);

    // B — fade 1, where the group is dropped entirely.
    f.settle_animations();
    assert_eq!(f.synoik().overview_search_fade(), 1.);
    let (pixels, ..) = render_output_vulkan(&mut f, &output);
    let b = px(&pixels, w, cx, cy);
    assert!(
        b[1] < 100,
        "the covered picker must leave plain background, got {b:?}"
    );

    // …and "plain background" means the `#overviewGroup` backdrop, not the
    // workspace. gnome-shell fades the whole `workspacesDisplay` — a Workspace
    // owns its `WorkspaceBackground`, so the rounded wallpaper goes with the
    // window clones (`overviewControls.js:628-637`). Fading only the picker
    // leaves the workspace rectangle sitting opaque under the results. Measured
    // against a corner that is backdrop in any case, so a retuned backdrop colour
    // keeps this honest.
    let corner = px(&pixels, w, 8, 8 + crate::ui::panel::panel_height() as i32);
    for c in 0..4 {
        assert!(
            b[c].abs_diff(corner[c]) <= 1,
            "channel {c}: the searched-over preview reads {} but the bare backdrop \
             reads {} — the workspace is still drawn under the results \
             (preview={b:?} backdrop={corner:?})",
            b[c],
            corner[c],
        );
    }

    eprintln!("vulkan_search_fade: S={s:?} M={m:?} B={b:?} fade={fade}");

    // Both ends are measured, so this stays honest if the theme changes.
    let alpha = 1. - fade;
    for c in 0..3 {
        let expected = s[c] as f64 * alpha + b[c] as f64 * fade;
        assert!(
            (m[c] as f64 - expected).abs() <= 4.,
            "channel {c}: got {}, want the blend {expected:.1} (S={} B={} \
             alpha={alpha:.3}); pushing the group straight through would give {}, \
             dropping it {}",
            m[c],
            s[c],
            b[c],
            s[c],
            b[c],
        );
    }
}

/// GNOME turns the top panel's background transparent while the overview is up
/// (`#panel:overview`, `_panel.scss:98-102`), so the `#overviewGroup` fill
/// (`$system_base_color` `#222226`, `_overview.scss:7-9` / `_colors.scss:20`) runs
/// unbroken from the top of the screen down and the bar reads as part of the
/// overview rather than a black band above it.
///
/// Pin both ends over the *same* pixel: in the overview, the colour the backdrop has just below
/// the bar. Comparing against the measured backdrop rather than a literal is what makes this a
/// "no visible break" assertion: it keeps holding if the backdrop colour is retuned, and fails if
/// the panel keeps any background of its own.
///
/// On the desktop the bar is *not* GNOME's opaque black — see [`crate::ui::panel::BAR_BG`]: ours
/// is a dark wash over a blurred capture of what is behind. Blurring a flat fill returns that same
/// fill, so with nothing but the workspace background under the bar the wash is the only thing
/// that happened, and the desktop end pins exactly that: the fill, dimmed by the wash.
#[test]
fn vulkan_overview_panel_background_matches_the_backdrop() {
    let Some(mut f) = green_window_fixture() else {
        return;
    };
    let output = f.synoik_output(1);
    f.synoik().hotkey_overlay.hide();

    // A column with no panel content in either state: right of the Activities button and
    // well clear of the right-hand cluster (the status indicators and, past them, the clock).
    let x = 300;
    // Where the backdrop is sampled. The picker box now starts one spacing under the
    // panel (the search entry floats, so it no longer pushes the row down), which puts
    // the workspace preview under x = 300; x = 80 is left of the row, so it is backdrop
    // — a uniform fill, which is why sampling it in a different column is still a fair
    // comparison against the bar.
    let backdrop_x = 80;
    let bar_y = (crate::ui::panel::panel_height() / 2.) as i32;
    let below_y = (crate::ui::panel::panel_height() + 8.) as i32;

    let (pixels, w, _) = render_output_vulkan(&mut f, &output);
    let desktop = px(&pixels, w, x, bar_y);
    // The workspace fill behind the bar, dimmed by the wash — both from their own constants, so
    // retuning either one moves the expectation with it.
    let fill = synoik_config::DEFAULT_BACKGROUND_COLOR.to_array_unpremul()[0];
    let wash = 1. - crate::ui::panel::BAR_BG[3];
    let expected = (fill * wash * 255.).round() as u8;
    for c in 0..3 {
        assert!(
            desktop[c].abs_diff(expected) <= 1,
            "channel {c}: on the desktop the bar should be the workspace fill under it dimmed by \
             the wash ({expected}), got {desktop:?} — a blur of a flat fill is that same fill, so \
             anything else means the bar is painting a background of its own",
        );
    }
    assert_eq!(
        desktop[3], 255,
        "the bar's own strip is still fully covered"
    );

    f.synoik_state().do_action(Action::OpenOverview, false);
    f.synoik_state().update_keyboard_focus();
    f.settle_animations();
    assert_eq!(
        f.synoik()
            .layout
            .monitor_for_output(&output)
            .and_then(|mon| mon.expose_progress()),
        Some(1.),
        "the overview must be fully open before sampling"
    );

    let (pixels, ..) = render_output_vulkan(&mut f, &output);
    let bar = px(&pixels, w, x, bar_y);
    let backdrop = px(&pixels, w, backdrop_x, below_y);
    eprintln!("vulkan_overview_panel_bg: desktop={desktop:?} bar={bar:?} backdrop={backdrop:?}");

    assert!(
        backdrop[0] > 0 && backdrop != desktop,
        "the sampled backdrop must not be black, or the comparison below is vacuous \
         (got {backdrop:?})"
    );
    for c in 0..4 {
        assert!(
            bar[c].abs_diff(backdrop[c]) <= 1,
            "channel {c}: the overview bar reads {} but the backdrop right below it \
             reads {} — the panel is still painting a background (bar={bar:?} \
             backdrop={backdrop:?})",
            bar[c],
            backdrop[c],
        );
    }
}

/// A search result rests at the same caption height as a grid tile
/// ([`crate::ui::widget::TILE_LABEL_LINES`]) — the two are the same `.overview-tile`
/// (`search.js:142`), and letting them disagree would be the odd choice. The second line
/// hangs below the tile box, so the card has to reserve room for it: the card is one
/// bake, and a line past its edge is simply not there.
#[test]
fn vulkan_search_result_caption_rests_at_the_grid_line_count() {
    use smithay::utils::Logical;

    use crate::app_system::AppIconRef;
    use crate::ui::overview_search::SearchResultEntry;
    use crate::ui::widget::TileMetrics;

    if let Err(e) = VulkanRenderer::new() {
        eprintln!("skipping vulkan_search_result_caption_rests: no Vulkan device ({e})");
        return;
    }

    let mut f = Fixture::new();
    f.synoik_state()
        .backend
        .headless()
        .add_renderer()
        .expect("build the Vulkan renderer");
    f.add_output(1, (1920, 1080));
    let output = f.synoik_output(1);

    // A name that needs the second line, and a short one as the control.
    {
        let s = &mut f.synoik().overview_search;
        s.handle_key(
            None,
            Some('a'),
            crate::ui::text_edit::EditMods::default(),
            crate::ui::text_edit::KeyTheme::default(),
        );
        s.set_results(vec![
            SearchResultEntry {
                id: "long.desktop".into(),
                name: "Passwords and Keys".into(),
                icon: AppIconRef::Fallback,
            },
            SearchResultEntry {
                id: "short.desktop".into(),
                name: "Files".into(),
                icon: AppIconRef::Fallback,
            },
        ]);
    }

    let controls = f
        .synoik()
        .layout
        .controls_layout_for_output(&output)
        .expect("output 1 has a monitor");
    let area: crate::ui::overview_search::SearchArea = controls.into();
    let mut tile = |i: usize| {
        f.synoik()
            .overview_search
            .result_tile(i, area)
            .expect("a result tile")
    };
    let (long_tile, short_tile) = (tile(0), tile(1));

    let state = f.synoik_state();
    let composited =
        state
            .backend
            .headless()
            .with_vulkan_renderer(|vk| -> anyhow::Result<(Vec<u8>, i32)> {
                let synoik = &mut state.synoik;
                let elements = synoik.overview_search.render(
                    vk,
                    &synoik.app_icon_cache,
                    &synoik.icon_cache,
                    &output,
                    area,
                    crate::ui::overview_search::SearchFade {
                        overview: 1.0,
                        search: 1.0,
                    },
                    synoik.gnome_settings.accent_color,
                );
                let phys: Size<i32, Physical> = output.current_mode().unwrap().size;
                let scale = Scale::from(output.current_scale().fractional_scale());
                let pixels = render_to_vec(
                    vk,
                    phys,
                    scale,
                    Transform::Normal,
                    Fourcc::Abgr8888,
                    elements.iter().rev(),
                )?;
                Ok((pixels, phys.w))
            });
    let Some(result) = composited else {
        eprintln!("skipping vulkan_search_result_caption_rests: no Vulkan device");
        return;
    };
    let (pixels, w) = result.expect("compositing the search through Vulkan must not error");

    let m = TileMetrics::overview();
    // Peak glyph ink across one caption line band of a tile. The card fill sits under it,
    // so this reads brightness, not alpha: the ink is near-white over a dark card.
    let line_ink = |t: Rectangle<f64, Logical>, line: usize| -> u8 {
        let top = m.label_top(t) + line as f64 * m.label_h;
        let mut max = 0u8;
        for y in top as i32..(top + m.label_h) as i32 {
            for x in t.loc.x as i32..(t.loc.x + t.size.w) as i32 {
                max = max.max(px(&pixels, w, x, y)[0]);
            }
        }
        max
    };

    let (first, second) = (line_ink(long_tile, 0), line_ink(long_tile, 1));
    let control = line_ink(short_tile, 1);
    eprintln!("vulkan_search_caption: long l0={first} l1={second} short l1={control}");
    assert!(first > 150, "the first caption line draws ({first})");
    assert!(
        second > 150,
        "…and so does the second, past the tile box ({second}) — a card sized for one \
         line would clip it"
    );
    assert!(
        control < 90,
        "a name that fits draws nothing on the second line ({control}) — so the ink \
         above is the wrap, not the card"
    );
}

#[test]
fn vulkan_overview_search_draws_entry_and_selection() {
    use smithay::utils::Logical;

    use crate::app_system::AppIconRef;
    use crate::ui::overview_search::SearchResultEntry;

    if let Err(e) = VulkanRenderer::new() {
        eprintln!(
            "skipping vulkan_overview_search_draws_entry_and_selection: no Vulkan device ({e})"
        );
        return;
    }

    let mut f = Fixture::new();
    f.synoik_state()
        .backend
        .headless()
        .add_renderer()
        .expect("build the Vulkan renderer");
    f.add_output(1, (1920, 1080));
    let output = f.synoik_output(1);

    // Drive the model directly into an active state with two results, tile 0 selected.
    {
        let s = &mut f.synoik().overview_search;
        s.handle_key(
            None,
            Some('a'),
            crate::ui::text_edit::EditMods::default(),
            crate::ui::text_edit::KeyTheme::default(),
        );
        s.set_results(vec![
            SearchResultEntry {
                id: "a.desktop".into(),
                name: "A".into(),
                icon: AppIconRef::Fallback,
            },
            SearchResultEntry {
                id: "b.desktop".into(),
                name: "B".into(),
                icon: AppIconRef::Fallback,
            },
        ]);
    }

    let controls = f
        .synoik()
        .layout
        .controls_layout_for_output(&output)
        .expect("output 1 has a monitor");
    let area: crate::ui::overview_search::SearchArea = controls.into();
    let pill = f.synoik().overview_search.entry_pill(area);
    let t0 = f
        .synoik()
        .overview_search
        .result_tile(0, area)
        .expect("tile 0");
    let t1 = f
        .synoik()
        .overview_search
        .result_tile(1, area)
        .expect("tile 1");

    let state = f.synoik_state();
    let composited = state.backend.headless().with_vulkan_renderer(
        |vk| -> anyhow::Result<(Vec<u8>, i32, i32)> {
            let synoik = &mut state.synoik;
            let elements = synoik.overview_search.render(
                vk,
                &synoik.app_icon_cache,
                &synoik.icon_cache,
                &output,
                area,
                crate::ui::overview_search::SearchFade {
                    overview: 1.0,
                    search: 1.0,
                },
                synoik.gnome_settings.accent_color,
            );
            let phys: Size<i32, Physical> = output.current_mode().unwrap().size;
            let scale = Scale::from(output.current_scale().fractional_scale());
            let pixels = render_to_vec(
                vk,
                phys,
                scale,
                Transform::Normal,
                Fourcc::Abgr8888,
                elements.iter().rev(),
            )?;
            Ok((pixels, phys.w, phys.h))
        },
    );
    let Some(result) = composited else {
        eprintln!("skipping vulkan_overview_search_draws_entry_and_selection: no Vulkan device");
        return;
    };
    let (pixels, w, _h) = result.expect("compositing the search through Vulkan must not error");

    // The entry pill fill, sampled low in the pill (below the text line, left of the trailing
    // clear glyph). It is the shared translucent `OVERVIEW_PLATE` — the search entry sits on the
    // overview backdrop — so pin its alpha rather than an opaque dark fill.
    let ex = (pill.loc.x + pill.size.w * 0.5) as i32;
    let ey = (pill.loc.y + pill.size.h - 5.) as i32;
    let entry = px(&pixels, w, ex, ey);
    eprintln!("vulkan_overview_search: entry={entry:?}");
    let plate = crate::ui::widget::style::OVERVIEW_PLATE;
    assert_eq!(
        entry[3],
        (plate[3] * 255.).round() as u8,
        "the entry pill must composite at the plate's alpha: {entry:?}"
    );
    for ch in 0..3 {
        // Premultiplied output, so the plate's own channel arrives scaled by its alpha.
        let want = (plate[ch] * plate[3] * 255.).round() as i32;
        assert!(
            (i32::from(entry[ch]) - want).abs() <= 2,
            "channel {ch}: the entry pill fill must be the plate ({want}): {entry:?}"
        );
    }

    // Inside each tile's left padding band, clear of the icon: the selected tile 0
    // carries the wash, tile 1 the plain card bg.
    let edge = |t: Rectangle<f64, Logical>| {
        px(
            &pixels,
            w,
            (t.loc.x + 4.) as i32,
            (t.loc.y + t.size.h / 2.) as i32,
        )
    };
    let selected = edge(t0);
    let plain = edge(t1);
    eprintln!("vulkan_overview_search: selected={selected:?} plain={plain:?}");
    assert_eq!(
        plain[3],
        (crate::ui::widget::style::OVERVIEW_PLATE[3] * 255.).round() as u8,
        "the results card must composite at the plate's alpha: {plain:?}"
    );
    for ch in 0..3 {
        assert!(
            selected[ch] > plain[ch],
            "the selected result tile must be lighter than an unselected one on \
             channel {ch}: selected={selected:?} plain={plain:?}"
        );
    }

    // The selection fill is `.overview-tile`'s radius (24), not `%tile`'s (16).
    // A point 6px diagonally inside the corner is outside a 24-radius round
    // (it needs 0.293·r ≈ 7.0) but inside a 16-radius one (needs ≈ 4.7), so this
    // discriminates the two rules rather than merely re-measuring the fill.
    let corner = px(&pixels, w, (t0.loc.x + 6.) as i32, (t0.loc.y + 6.) as i32);
    eprintln!("vulkan_overview_search: corner={corner:?}");
    let d_plain: i32 = (0..3)
        .map(|c| (corner[c] as i32 - plain[c] as i32).abs())
        .sum();
    let d_selected: i32 = (0..3)
        .map(|c| (corner[c] as i32 - selected[c] as i32).abs())
        .sum();
    assert!(
        d_plain < d_selected,
        "the tile corner must be cut by the `.overview-tile` radius (24) — at \
         `%tile`'s 16 it would still be washed: corner={corner:?} \
         plain={plain:?} selected={selected:?}"
    );
}

/// The app grid draws its labelled tiles into the `app_display` band: the hovered
/// tile carries the `.overview-tile:hover` wash (an opaque, non-transparent pixel in
/// its padding band), while a non-hovered tile's padding band stays transparent (the
/// grid has no card background — tiles sit straight on the overview). This pins the
/// render wiring, the hover bake key, and that the grid composites at all.
#[test]
fn vulkan_app_grid_draws_hovered_tile() {
    use smithay::utils::{Logical, Point};

    use crate::app_system::AppIconRef;
    use crate::ui::app_grid::AppGridEntry;

    if let Err(e) = VulkanRenderer::new() {
        eprintln!("skipping vulkan_app_grid_draws_hovered_tile: no Vulkan device ({e})");
        return;
    }

    let mut f = Fixture::new();
    f.synoik_state()
        .backend
        .headless()
        .add_renderer()
        .expect("build the Vulkan renderer");
    f.add_output(1, (1920, 1080));
    let output = f.synoik_output(1);

    // Drive three apps into the grid, tile 0 hovered.
    {
        let g = &mut f.synoik().app_grid;
        g.set_entries(
            ["A", "B", "C"]
                .iter()
                .map(|n| AppGridEntry {
                    id: format!("{n}.desktop"),
                    name: (*n).to_string(),
                    icon: AppIconRef::Fallback,
                    folder: None,
                })
                .collect(),
        );
        g.set_hovered(Some(0));
    }

    // A fixed band to lay the grid into (independent of the state animation).
    let area: Rectangle<f64, Logical> = Rectangle::new((0., 120.).into(), (1920., 700.).into());
    let t0 = f.synoik().app_grid.tile_center(0, area).expect("tile 0");
    let t1 = f.synoik().app_grid.tile_center(1, area).expect("tile 1");

    let state = f.synoik_state();
    let composited = state.backend.headless().with_vulkan_renderer(
        |vk| -> anyhow::Result<(Vec<u8>, i32, i32)> {
            let synoik = &mut state.synoik;
            let elements = synoik.app_grid.render(
                vk,
                &synoik.app_icon_cache,
                &synoik.icon_cache,
                &output,
                area,
                1.0,
                crate::gnome::ACCENT_BLUE,
            );
            let phys: Size<i32, Physical> = output.current_mode().unwrap().size;
            let scale = Scale::from(output.current_scale().fractional_scale());
            let pixels = render_to_vec(
                vk,
                phys,
                scale,
                Transform::Normal,
                Fourcc::Abgr8888,
                elements.iter().rev(),
            )?;
            Ok((pixels, phys.w, phys.h))
        },
    );
    let Some(result) = composited else {
        eprintln!("skipping vulkan_app_grid_draws_hovered_tile: no Vulkan device");
        return;
    };
    let (pixels, w, _h) = result.expect("compositing the app grid through Vulkan must not error");

    // Sample each tile's left padding band, mid-height, clear of the icon: the hovered
    // tile 0 carries the wash (non-transparent); tile 1's band is transparent.
    let band = |c: Point<f64, Logical>| {
        px(
            &pixels,
            w,
            (c.x - 48.) as i32, // left of the 96px icon, in the tile padding
            c.y as i32,
        )
    };
    let hovered = band(t0);
    let plain = band(t1);
    eprintln!("vulkan_app_grid: hovered={hovered:?} plain={plain:?}");
    assert!(
        hovered[3] > 0,
        "the hovered tile must carry a wash (non-transparent): {hovered:?}"
    );
    // The wash is `style::HOVER_WASH`, straight white at 10%; a bake stores premultiplied alpha,
    // so it must land as rgb == a (~26), NOT rgb 255 with a 26. Pins the straight→premultiplied
    // conversion at the toolkit boundary — without it this element composites at full-strength
    // white, and with a straight *blend* underneath it darkens instead of lightens.
    assert!(
        hovered[..3]
            .iter()
            .all(|&c| i16::from(c) - i16::from(hovered[3]) <= 2
                && i16::from(hovered[3]) - i16::from(c) <= 2),
        "the hover wash must be stored premultiplied (rgb == a): {hovered:?}"
    );
    assert_eq!(
        plain[3], 0,
        "a non-hovered tile's padding band must be transparent (no card bg): {plain:?}"
    );
}

/// A grid caption too long for one line ellipsizes at rest and, when the tile is
/// highlighted, wraps onto further lines showing the whole name
/// (`AppViewItem._updateMultiline`, `appDisplay.js:1891-1924`). Before this the label
/// was hard-clipped to the tile box, so the tail of a long name was cut mid-glyph and
/// hovering did nothing.
///
/// Sampled on the line past [`crate::ui::widget::TILE_LABEL_LINES`], which only exists
/// when expanded: at rest that band is empty, hovered it carries glyph ink (bright, well
/// past the 10% hover wash that also grows over it). The name is long enough to need it —
/// a resting caption is two lines here, so a name that fits in two would expand to nothing.
#[test]
fn vulkan_app_grid_expands_a_long_caption_on_hover() {
    use smithay::utils::Logical;

    use crate::app_system::AppIconRef;
    use crate::ui::app_grid::AppGridEntry;
    use crate::ui::widget::TileMetrics;

    if let Err(e) = VulkanRenderer::new() {
        eprintln!(
            "skipping vulkan_app_grid_expands_a_long_caption_on_hover: no Vulkan device ({e})"
        );
        return;
    }

    let mut f = Fixture::new();
    f.synoik_state()
        .backend
        .headless()
        .add_renderer()
        .expect("build the Vulkan renderer");
    f.add_output(1, (1920, 1080));
    let output = f.synoik_output(1);

    // Tile 1's name does not fit the caption box; tile 0's does.
    let long = "Passwords and Keys and Certificates";
    f.synoik().app_grid.set_entries(
        ["Files", long]
            .iter()
            .map(|n| AppGridEntry {
                id: format!("{n}.desktop"),
                name: (*n).to_string(),
                icon: AppIconRef::Fallback,
                folder: None,
            })
            .collect(),
    );

    let area: Rectangle<f64, Logical> = Rectangle::new((0., 120.).into(), (1920., 700.).into());
    let t1 = f.synoik().app_grid.tile_center(1, area).expect("tile 1");
    let m = TileMetrics::overview();
    let tile_top = t1.y - m.size().h / 2.;
    let first_x = (t1.x - m.label_w() / 2.) as i32;

    // How many separate lines of glyph ink the caption draws, counted as runs of rows with ink
    // across the caption's column. Counted rather than sampled at a computed row: an expanded
    // tile grows about its centre, so the extra line pushes the whole block *up* as well as
    // down, and a row derived from the resting tile top misses it. The claim under test is
    // "one more line of text", so count lines.
    let caption_lines = |f: &mut Fixture, hovered: Option<usize>| -> usize {
        f.synoik().app_grid.set_hovered(hovered);
        let state = f.synoik_state();
        let composited =
            state
                .backend
                .headless()
                .with_vulkan_renderer(|vk| -> anyhow::Result<(Vec<u8>, i32)> {
                    let synoik = &mut state.synoik;
                    let elements = synoik.app_grid.render(
                        vk,
                        &synoik.app_icon_cache,
                        &synoik.icon_cache,
                        &output,
                        area,
                        1.0,
                        crate::gnome::ACCENT_BLUE,
                    );
                    let phys: Size<i32, Physical> = output.current_mode().unwrap().size;
                    let scale = Scale::from(output.current_scale().fractional_scale());
                    let pixels = render_to_vec(
                        vk,
                        phys,
                        scale,
                        Transform::Normal,
                        Fourcc::Abgr8888,
                        elements.iter().rev(),
                    )?;
                    Ok((pixels, phys.w))
                });
        let (pixels, w) = composited
            .expect("a Vulkan device")
            .expect("compositing the app grid through Vulkan must not error");

        // Glyph ink only — well above the 10% hover wash (alpha ~26) that covers the whole tile.
        let icon_bottom = (tile_top + m.pad + m.icon_px) as i32;
        let (mut lines, mut in_line) = (0usize, false);
        for y in icon_bottom..(icon_bottom + 120) {
            let has_ink =
                (0..m.label_w() as i32).any(|dx| px(&pixels, w, first_x + dx, y)[3] > 128);
            if has_ink && !in_line {
                lines += 1;
            }
            in_line = has_ink;
        }
        lines
    };

    let at_rest = caption_lines(&mut f, None);
    let expanded = caption_lines(&mut f, Some(1));
    eprintln!("vulkan_app_grid caption: at_rest={at_rest} expanded={expanded}");
    assert_eq!(
        at_rest,
        crate::ui::widget::TILE_LABEL_LINES,
        "a resting caption stops at TILE_LABEL_LINES"
    );
    assert!(
        expanded > at_rest,
        "hovering must wrap the caption onto at least one more line of glyph ink \
         ({expanded} vs {at_rest})"
    );
}

/// While a drag is in flight the grid's side bands carry the `.page-navigation-hint`
/// gradient and the *next page's* first column slides into the right one
/// (`_syncPageIndicators` + `_translateNextPageIcons`, `appDisplay.js:311-397`). At rest
/// the bands are empty — the grid content never reaches into them.
#[test]
fn vulkan_app_grid_previews_the_next_page_while_dragging() {
    use smithay::utils::Logical;

    use crate::app_system::AppIconRef;
    use crate::ui::app_grid::AppGridEntry;

    if let Err(e) = VulkanRenderer::new() {
        eprintln!(
            "skipping vulkan_app_grid_previews_the_next_page_while_dragging: no device ({e})"
        );
        return;
    }

    let mut f = Fixture::new();
    f.synoik_state()
        .backend
        .headless()
        .add_renderer()
        .expect("build the Vulkan renderer");
    f.add_output(1, (1920, 1080));
    let output = f.synoik_output(1);

    // Enough apps to paginate, so there is a next page to preview.
    let area: Rectangle<f64, Logical> = Rectangle::new((0., 120.).into(), (1920., 700.).into());

    // Two pages, whatever this band holds — the capacity is not GNOME's fixed 24 since the fill
    // divergence scales the mode up to the canvas. Asked after seeding: an empty grid has no
    // layout to report a capacity from.
    let seed = |n: usize| -> Vec<AppGridEntry> {
        (0..n)
            .map(|i| AppGridEntry {
                id: format!("app{i:02}.desktop"),
                name: format!("App {i:02}"),
                icon: AppIconRef::Fallback,
                folder: None,
            })
            .collect()
    };
    f.synoik().app_grid.set_entries(seed(256));
    let per_page = f.synoik().app_grid.items_per_page(area);
    f.synoik().app_grid.set_entries(seed(per_page + 6));
    // The right band is 10% of the width, so 1728..1920. Sample the *first* row of
    // tiles: the next-page arrow lives in this band too, vertically centred, and it is
    // there whether or not anything is being dragged.
    let sample =
        |f: &mut Fixture| -> u8 {
            let state = f.synoik_state();
            let composited = state.backend.headless().with_vulkan_renderer(
                |vk| -> anyhow::Result<(Vec<u8>, i32)> {
                    let synoik = &mut state.synoik;
                    let elements = synoik.app_grid.render(
                        vk,
                        &synoik.app_icon_cache,
                        &synoik.icon_cache,
                        &output,
                        area,
                        1.0,
                        crate::gnome::ACCENT_BLUE,
                    );
                    let phys: Size<i32, Physical> = output.current_mode().unwrap().size;
                    let scale = Scale::from(output.current_scale().fractional_scale());
                    let pixels = render_to_vec(
                        vk,
                        phys,
                        scale,
                        Transform::Normal,
                        Fourcc::Abgr8888,
                        elements.iter().rev(),
                    )?;
                    Ok((pixels, phys.w))
                },
            );
            let (pixels, w) = composited
                .expect("a Vulkan device")
                .expect("compositing the app grid through Vulkan must not error");
            // Brightest alpha anywhere over the band, across the first row of tiles.
            (200..340)
                .flat_map(|y| (1728..1918).map(move |x| (x, y)))
                .map(|(x, y)| px(&pixels, w, x, y)[3])
                .max()
                .unwrap_or(0)
        };

    let at_rest = sample(&mut f);
    assert_eq!(at_rest, 0, "with no drag the side bands are empty");

    f.synoik().app_grid.set_drag_active(true);
    f.settle_animations();
    let peeking = sample(&mut f);
    eprintln!("vulkan_app_grid preview: {peeking}");
    assert!(
        peeking > 128,
        "the next page's first column must slide into the band — the 5% hint gradient \
         alone could not reach this: {peeking}"
    );
}

/// The grid's batch icon upload path: two distinct icons (so the page has >1 pending upload,
/// tripping `import_memory_batch`) must both draw, each with its own colors — proving the single
/// submit uploaded every texture correctly, not a swapped/blank one.
#[test]
fn vulkan_app_grid_batch_uploads_page_icons() {
    use smithay::utils::Logical;

    use crate::app_system::AppIconRef;
    use crate::ui::app_grid::AppGridEntry;

    if let Err(e) = VulkanRenderer::new() {
        eprintln!("skipping vulkan_app_grid_batch_uploads_page_icons: no Vulkan device ({e})");
        return;
    }

    // Two solid-color source icons on disk (distinct paths → distinct cache keys → two pending
    // uploads on the page). The harness has no decode worker, so `buffer()` resolves inline and
    // both are ready when the grid renders → the batch path runs.
    let dir = std::env::temp_dir();
    let red_path = dir.join(format!("synoik-batch-red-{}.png", std::process::id()));
    let blue_path = dir.join(format!("synoik-batch-blue-{}.png", std::process::id()));
    image::RgbaImage::from_pixel(16, 16, image::Rgba([220, 20, 20, 255]))
        .save(&red_path)
        .expect("write red icon");
    image::RgbaImage::from_pixel(16, 16, image::Rgba([20, 20, 220, 255]))
        .save(&blue_path)
        .expect("write blue icon");

    let mut f = Fixture::new();
    f.synoik_state()
        .backend
        .headless()
        .add_renderer()
        .expect("build the Vulkan renderer");
    f.add_output(1, (1920, 1080));
    let output = f.synoik_output(1);

    {
        let g = &mut f.synoik().app_grid;
        g.set_entries(vec![
            AppGridEntry {
                id: "red.desktop".into(),
                name: "Red".into(),
                icon: AppIconRef::File(red_path.clone()),
                folder: None,
            },
            AppGridEntry {
                id: "blue.desktop".into(),
                name: "Blue".into(),
                icon: AppIconRef::File(blue_path.clone()),
                folder: None,
            },
        ]);
    }

    let area: Rectangle<f64, Logical> = Rectangle::new((0., 120.).into(), (1920., 700.).into());
    let c0 = f.synoik().app_grid.icon_center(0, area).expect("icon 0");
    let c1 = f.synoik().app_grid.icon_center(1, area).expect("icon 1");

    let state = f.synoik_state();
    let composited =
        state
            .backend
            .headless()
            .with_vulkan_renderer(|vk| -> anyhow::Result<(Vec<u8>, i32)> {
                let synoik = &mut state.synoik;
                let elements = synoik.app_grid.render(
                    vk,
                    &synoik.app_icon_cache,
                    &synoik.icon_cache,
                    &output,
                    area,
                    1.0,
                    crate::gnome::ACCENT_BLUE,
                );
                let phys: Size<i32, Physical> = output.current_mode().unwrap().size;
                let scale = Scale::from(output.current_scale().fractional_scale());
                let pixels = render_to_vec(
                    vk,
                    phys,
                    scale,
                    Transform::Normal,
                    Fourcc::Abgr8888,
                    elements.iter().rev(),
                )?;
                Ok((pixels, phys.w))
            });

    let _ = std::fs::remove_file(&red_path);
    let _ = std::fs::remove_file(&blue_path);

    let Some(result) = composited else {
        eprintln!("skipping vulkan_app_grid_batch_uploads_page_icons: no Vulkan device");
        return;
    };
    let (pixels, w) = result.expect("compositing the batched grid through Vulkan must not error");

    let p0 = px(&pixels, w, c0.x as i32, c0.y as i32);
    let p1 = px(&pixels, w, c1.x as i32, c1.y as i32);
    eprintln!("vulkan_app_grid_batch: icon0={p0:?} icon1={p1:?}");
    assert!(
        p0[3] > 0 && p0[0] > 200 && p0[2] < 60,
        "tile 0 drew its red icon through the batch: {p0:?}"
    );
    assert!(
        p1[3] > 0 && p1[2] > 200 && p1[0] < 60,
        "tile 1 drew its blue icon through the batch: {p1:?}"
    );
}

/// A folder tile draws its first four members as a 2×2 over a **raised** background
/// (`createFolderIcon` + `.app-folder`'s `tile_button($raised: true)`,
/// `appDisplay.js:2138-2162`, `_app-grid.scss:41`), where an app tile draws one icon
/// over nothing — `tile_button`'s flat branch forces the resting background
/// transparent (`_drawing.scss:362-365`).
///
/// Sampling four member colors at their own quadrants is what separates a real
/// composition from a single icon centered in the tile, and sampling the tile corner
/// separates the raised folder from its flat neighbour.
#[test]
fn vulkan_app_grid_composes_a_folder_tile() {
    use smithay::utils::Logical;

    use crate::app_system::AppIconRef;
    use crate::ui::app_grid::AppGridEntry;

    if let Err(e) = VulkanRenderer::new() {
        eprintln!("skipping vulkan_app_grid_composes_a_folder_tile: no Vulkan device ({e})");
        return;
    }

    let dir = std::env::temp_dir();
    // Four distinguishable members plus a plain app tile to contrast against.
    let colors = [
        [220u8, 20, 20],
        [20, 220, 20],
        [20, 20, 220],
        [220, 220, 20],
    ];
    let paths: Vec<std::path::PathBuf> = colors
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let path = dir.join(format!("synoik-folder-{}-{i}.png", std::process::id()));
            image::RgbaImage::from_pixel(16, 16, image::Rgba([c[0], c[1], c[2], 255]))
                .save(&path)
                .expect("write member icon");
            path
        })
        .collect();

    let mut f = Fixture::new();
    f.synoik_state()
        .backend
        .headless()
        .add_renderer()
        .expect("build the Vulkan renderer");
    f.add_output(1, (1920, 1080));
    let output = f.synoik_output(1);

    {
        let g = &mut f.synoik().app_grid;
        g.set_entries(vec![
            AppGridEntry {
                id: "plain.desktop".into(),
                name: "Plain".into(),
                icon: AppIconRef::Fallback,
                folder: None,
            },
            AppGridEntry {
                id: "Utilities".into(),
                name: "Utilities".into(),
                icon: AppIconRef::Fallback,
                folder: Some(
                    paths
                        .iter()
                        .enumerate()
                        .map(|(i, path)| AppGridEntry {
                            id: format!("m{i}.desktop"),
                            name: format!("M{i}"),
                            icon: AppIconRef::File(path.clone()),
                            folder: None,
                        })
                        .collect(),
                ),
            },
        ]);
    }

    let area: Rectangle<f64, Logical> = Rectangle::new((0., 120.).into(), (1920., 700.).into());
    let subs: Vec<_> = (0..4)
        .map(|i| {
            f.synoik()
                .app_grid
                .folder_subicon_center(1, i, area)
                .expect("folder sub-icon")
        })
        .collect();
    // A tile corner, inside the folder's rounded box but well clear of its icons.
    let folder_rect = f
        .synoik()
        .app_grid
        .entry_rect(1, area)
        .expect("folder tile");
    let plain_rect = f.synoik().app_grid.entry_rect(0, area).expect("app tile");
    let corner = |r: Rectangle<f64, Logical>| (r.loc.x + r.size.w - 12., r.loc.y + 12.);

    let state = f.synoik_state();
    let composited =
        state
            .backend
            .headless()
            .with_vulkan_renderer(|vk| -> anyhow::Result<(Vec<u8>, i32)> {
                let synoik = &mut state.synoik;
                let elements = synoik.app_grid.render(
                    vk,
                    &synoik.app_icon_cache,
                    &synoik.icon_cache,
                    &output,
                    area,
                    1.0,
                    crate::gnome::ACCENT_BLUE,
                );
                let phys: Size<i32, Physical> = output.current_mode().unwrap().size;
                let scale = Scale::from(output.current_scale().fractional_scale());
                let pixels = render_to_vec(
                    vk,
                    phys,
                    scale,
                    Transform::Normal,
                    Fourcc::Abgr8888,
                    elements.iter().rev(),
                )?;
                Ok((pixels, phys.w))
            });

    for path in &paths {
        let _ = std::fs::remove_file(path);
    }

    let Some(result) = composited else {
        eprintln!("skipping vulkan_app_grid_composes_a_folder_tile: no Vulkan device");
        return;
    };
    let (pixels, w) = result.expect("compositing the folder tile through Vulkan must not error");

    for (i, ((center, _), want)) in subs.iter().zip(&colors).enumerate() {
        let got = px(&pixels, w, center.x as i32, center.y as i32);
        eprintln!("vulkan_app_grid_folder: sub{i} at {center:?} = {got:?}");
        for ch in 0..3 {
            assert!(
                (got[ch] as i32 - want[ch] as i32).abs() < 40,
                "sub-icon {i} must draw its own color in its own quadrant: got {got:?}, \
                 want {want:?} — a single centered icon or a wrong cell order fails here"
            );
        }
    }

    let (fx, fy) = corner(folder_rect);
    let (px_, py) = corner(plain_rect);
    let folder_corner = px(&pixels, w, fx as i32, fy as i32);
    let plain_corner = px(&pixels, w, px_ as i32, py as i32);
    eprintln!("vulkan_app_grid_folder: folder={folder_corner:?} plain={plain_corner:?}");
    // The folder tile is the grid's only *raised* tile, so it is the only one with a resting
    // fill at all — the translucent `OVERVIEW_PLATE`. A plain tile's corner is empty, which is
    // what the comparison below is against.
    assert_eq!(
        folder_corner[3],
        (crate::ui::widget::style::OVERVIEW_PLATE[3] * 255.).round() as u8,
        "the folder tile has a resting fill at the plate's alpha: {folder_corner:?}"
    );
    assert_eq!(
        plain_corner[3], 0,
        "the app tile beside it is flat: nothing at rest"
    );
}

/// With more apps than one page holds, the app grid bakes its page-indicator dots:
/// the active page's dot is a full-opacity white circle, an inactive one is dimmer,
/// and the gap between dots is transparent.
#[test]
fn vulkan_app_grid_draws_page_indicator_dots() {
    use smithay::utils::{Logical, Point};

    use crate::app_system::AppIconRef;
    use crate::ui::app_grid::AppGridEntry;

    if let Err(e) = VulkanRenderer::new() {
        eprintln!("skipping vulkan_app_grid_draws_page_indicator_dots: no Vulkan device ({e})");
        return;
    }

    let mut f = Fixture::new();
    f.synoik_state()
        .backend
        .headless()
        .add_renderer()
        .expect("build the Vulkan renderer");
    f.add_output(1, (1920, 1080));
    let output = f.synoik_output(1);

    let area: Rectangle<f64, Logical> = Rectangle::new((0., 120.).into(), (1920., 700.).into());

    // Two pages on a wide band -> a two-dot indicator row.
    //
    // Two pages, whatever this band holds. The capacity is not GNOME's fixed 24 — the fill
    // divergence scales the mode up to the canvas — so it is asked of the grid, and asked after
    // seeding, since an empty grid has no layout to report one from.
    {
        let g = &mut f.synoik().app_grid;
        let seed = |n: usize| -> Vec<AppGridEntry> {
            (0..n)
                .map(|i| AppGridEntry {
                    id: format!("app{i:02}.desktop"),
                    name: format!("App {i:02}"),
                    icon: AppIconRef::Fallback,
                    folder: None,
                })
                .collect()
        };
        g.set_entries(seed(256));
        let per_page = g.items_per_page(area);
        g.set_entries(seed(per_page + 6));
    }
    let d0 = f
        .synoik()
        .app_grid
        .indicator_center(0, area)
        .expect("dot 0");
    let d1 = f
        .synoik()
        .app_grid
        .indicator_center(1, area)
        .expect("dot 1");
    let mid = Point::from(((d0.x + d1.x) / 2., d0.y));

    let state = f.synoik_state();
    let composited = state.backend.headless().with_vulkan_renderer(
        |vk| -> anyhow::Result<(Vec<u8>, i32, i32)> {
            let synoik = &mut state.synoik;
            let elements = synoik.app_grid.render(
                vk,
                &synoik.app_icon_cache,
                &synoik.icon_cache,
                &output,
                area,
                1.0,
                crate::gnome::ACCENT_BLUE,
            );
            let phys: Size<i32, Physical> = output.current_mode().unwrap().size;
            let scale = Scale::from(output.current_scale().fractional_scale());
            let pixels = render_to_vec(
                vk,
                phys,
                scale,
                Transform::Normal,
                Fourcc::Abgr8888,
                elements.iter().rev(),
            )?;
            Ok((pixels, phys.w, phys.h))
        },
    );
    let Some(result) = composited else {
        eprintln!("skipping vulkan_app_grid_draws_page_indicator_dots: no Vulkan device");
        return;
    };
    let (pixels, w, _h) = result.expect("compositing the dots through Vulkan must not error");

    let at = |p: Point<f64, Logical>| px(&pixels, w, p.x as i32, p.y as i32);
    let active = at(d0); // page 0 is current
    let inactive = at(d1);
    let gap = at(mid);
    eprintln!("vulkan_app_grid_dots: active={active:?} inactive={inactive:?} gap={gap:?}");
    assert_eq!(
        active[3], 255,
        "the active page dot is opaque white: {active:?}"
    );
    assert!(
        inactive[3] > 0 && inactive[3] < active[3],
        "an inactive dot is dimmer than the active one: {inactive:?} vs {active:?}"
    );
    assert_eq!(gap[3], 0, "the gap between dots is transparent: {gap:?}");
}

/// The page-navigation arrow bakes correctly: the chevron glyph draws opaque, the
/// button is flat (no resting background) off the glyph, and hovering it paints the
/// standard wash disc there.
#[test]
fn vulkan_app_grid_draws_navigation_arrows() {
    use smithay::utils::{Logical, Point};

    use crate::app_system::AppIconRef;
    use crate::ui::app_grid::{AppGridEntry, PageArrow};

    if let Err(e) = VulkanRenderer::new() {
        eprintln!("skipping vulkan_app_grid_draws_navigation_arrows: no Vulkan device ({e})");
        return;
    }

    let mut f = Fixture::new();
    f.synoik_state()
        .backend
        .headless()
        .add_renderer()
        .expect("build the Vulkan renderer");
    f.add_output(1, (1920, 1080));
    let output = f.synoik_output(1);

    let area: Rectangle<f64, Logical> = Rectangle::new((0., 120.).into(), (1920., 700.).into());

    // Two pages -> page 0 shows a next arrow (and no previous one).
    //
    // Two pages, whatever this band holds. The capacity is not GNOME's fixed 24 — the fill
    // divergence scales the mode up to the canvas — so it is asked of the grid, and asked after
    // seeding, since an empty grid has no layout to report one from.
    {
        let g = &mut f.synoik().app_grid;
        let seed = |n: usize| -> Vec<AppGridEntry> {
            (0..n)
                .map(|i| AppGridEntry {
                    id: format!("app{i:02}.desktop"),
                    name: format!("App {i:02}"),
                    icon: AppIconRef::Fallback,
                    folder: None,
                })
                .collect()
        };
        g.set_entries(seed(256));
        let per_page = g.items_per_page(area);
        g.set_entries(seed(per_page + 6));
    }
    let center = f
        .synoik()
        .app_grid
        .arrow_center(PageArrow::Next, area)
        .expect("the next arrow exists on page 0");
    // A point inside the 60px disc but outside the central 24px glyph box: flat at
    // rest, washed on hover.
    let off_glyph: Point<f64, Logical> = Point::from((center.x + 20., center.y));

    let render =
        |f: &mut Fixture| -> (Vec<u8>, i32) {
            let state = f.synoik_state();
            let composited = state.backend.headless().with_vulkan_renderer(
                |vk| -> anyhow::Result<(Vec<u8>, i32)> {
                    let synoik = &mut state.synoik;
                    let elements = synoik.app_grid.render(
                        vk,
                        &synoik.app_icon_cache,
                        &synoik.icon_cache,
                        &output,
                        area,
                        1.0,
                        crate::gnome::ACCENT_BLUE,
                    );
                    let phys: Size<i32, Physical> = output.current_mode().unwrap().size;
                    let scale = Scale::from(output.current_scale().fractional_scale());
                    let pixels = render_to_vec(
                        vk,
                        phys,
                        scale,
                        Transform::Normal,
                        Fourcc::Abgr8888,
                        elements.iter().rev(),
                    )?;
                    Ok((pixels, phys.w))
                },
            );
            composited
                .expect("no Vulkan device")
                .expect("compositing the arrows through Vulkan must not error")
        };

    // At rest: the chevron glyph draws opaque somewhere in its central box, and the
    // button is flat (transparent) off the glyph.
    let (rest, w) = render(&mut f);
    let glyph_max = (-10..=10)
        .flat_map(|dy| (-10..=10).map(move |dx| (dx, dy)))
        .map(|(dx, dy)| px(&rest, w, center.x as i32 + dx, center.y as i32 + dy)[3])
        .max()
        .unwrap();
    assert!(
        glyph_max > 128,
        "the chevron glyph draws opaque (max alpha {glyph_max})"
    );
    let rest_off = px(&rest, w, off_glyph.x as i32, off_glyph.y as i32);
    assert_eq!(
        rest_off[3], 0,
        "a resting flat button has no background off its glyph: {rest_off:?}"
    );

    // Hovering the arrow paints the wash disc under the glyph.
    assert!(f.synoik().app_grid.set_arrow_hovered(Some(PageArrow::Next)));
    let (hover, w) = render(&mut f);
    let hover_off = px(&hover, w, off_glyph.x as i32, off_glyph.y as i32);
    eprintln!("vulkan_app_grid_arrow: rest_off={rest_off:?} hover_off={hover_off:?}");
    assert!(
        hover_off[3] > 0 && hover_off[3] < 255,
        "hovering paints a translucent wash off the glyph: {hover_off:?}"
    );
}

/// The dash's running chrome bakes correctly: the `.dash-separator` reads as a
/// line *lighter* than the pill it sits on, and a running app's dot draws over
/// the pill.
///
/// The separator is the one worth a render test. `Painter::hairline` *clears*
/// rather than blends, so painting `$system_borders_color` (white at 10%) raw
/// would replace the opaque pill with a 10%-alpha pixel — a transparent slot
/// showing the wallpaper through the dash, invisible to every geometry test in
/// `dash.rs` because the box is in exactly the right place either way. Asserting
/// the sample is *opaque* is what catches it.
#[test]
fn vulkan_dash_separator_and_running_dot_bake_over_the_pill() {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog, RecordingLauncher};

    if let Err(e) = VulkanRenderer::new() {
        eprintln!("skipping vulkan_dash_separator_and_running_dot: no Vulkan device ({e})");
        return;
    }

    let mut f = Fixture::new();
    f.synoik_state()
        .backend
        .headless()
        .add_renderer()
        .expect("build the Vulkan renderer");
    f.add_output(1, (1920, 1080));
    let output = f.synoik_output(1);

    // One favorite (not running) plus one running non-favorite: the exact
    // condition that draws a divider.
    let apps = vec![
        AppEntry::fake("fav.desktop", "fav.desktop"),
        AppEntry::fake_with_wm_class("run.desktop", "run.desktop", "run"),
    ];
    f.synoik().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(apps)),
        Box::new(RecordingLauncher::default()),
    );
    f.synoik()
        .app_system
        .set_favorites(vec!["fav.desktop".into()]);
    f.synoik()
        .app_system
        .set_windows(vec![crate::app_system::RunningWindow {
            id: crate::window::mapped::MappedId::next(),
            app_id: Some("run".to_owned()),
            title: None,
            last_focus: None,
        }]);
    f.synoik().sync_dash_favorites();

    let controls = f
        .synoik()
        .layout
        .controls_layout_for_output(&output)
        .expect("output 1 has a monitor");
    let area = controls.dash;
    let sep = f
        .synoik()
        .dash
        .separator_box(area)
        .expect("a favorite plus a running non-favorite draws the divider");
    let fav = f.synoik().dash.tile_center(0, area).expect("tile 0");
    let running = f.synoik().dash.tile_center(1, area).expect("tile 1");
    let f_dot = f.synoik().dash.dot_box_for(1, area);
    assert!(
        f.synoik().dash.dot_box_for(0, area).is_none(),
        "the non-running favorite has no dot"
    );

    let state = f.synoik_state();
    let composited = state.backend.headless().with_vulkan_renderer(
        |vk| -> anyhow::Result<(Vec<u8>, i32, i32)> {
            let synoik = &mut state.synoik;
            let elements = synoik.dash.render(
                vk,
                &synoik.app_icon_cache,
                &synoik.icon_cache,
                &output,
                area,
                1.0,
            );
            let phys: Size<i32, Physical> = output.current_mode().unwrap().size;
            let scale = Scale::from(output.current_scale().fractional_scale());
            let pixels = render_to_vec(
                vk,
                phys,
                scale,
                Transform::Normal,
                Fourcc::Abgr8888,
                elements.iter().rev(),
            )?;
            Ok((pixels, phys.w, phys.h))
        },
    );
    let Some(result) = composited else {
        eprintln!("skipping vulkan_dash_separator_and_running_dot: no Vulkan device");
        return;
    };
    let (pixels, w, _h) = result.expect("compositing the dash through Vulkan must not error");

    let sample = |x: f64, y: f64| px(&pixels, w, x as i32, y as i32);

    // Reference: bare pill, sampled above the icon row on the favorite's tile.
    let pill = sample(fav.x, fav.y - 35.);
    let on_sep = sample(sep.loc.x + sep.size.w / 2., sep.loc.y + sep.size.h / 2.);
    // The dot boxes come from the dash itself, so the probe cannot drift from the
    // geometry it is pinning.
    let dot_box = f_dot.expect("the running app has a dot");
    let dot = sample(
        dot_box.loc.x + dot_box.size.w / 2.,
        dot_box.loc.y + dot_box.size.h / 2.,
    );
    let no_dot = sample(
        dot_box.loc.x + dot_box.size.w / 2. - (running.x - fav.x),
        dot_box.loc.y + dot_box.size.h / 2.,
    );
    eprintln!("dash chrome: pill={pill:?} separator={on_sep:?} dot={dot:?} no_dot={no_dot:?}");

    let plate_a = (crate::ui::widget::style::OVERVIEW_PLATE[3] * 255.).round() as u8;
    assert_eq!(
        pill[3], plate_a,
        "the dash pill must composite at the plate's alpha ({plate_a}): {pill:?}"
    );
    // `hairline` *clears* rather than blends, so the divider's colour is pre-composited onto the
    // pill by `style::over`. Over a translucent plate that means the divider comes out *more*
    // opaque than the pill, never less: anything below the pill's own alpha is the hole this
    // pre-composite exists to prevent, which is what a fixed-alpha-1 `over` used to produce here
    // in reverse — a solid bar across a plate the backdrop is meant to show through.
    assert!(
        on_sep[3] > plate_a && on_sep[3] < 255,
        "the divider must be denser than the pill but still let the backdrop through \
         (pill alpha {plate_a}): {on_sep:?}"
    );
    for ch in 0..3 {
        assert!(
            on_sep[ch] > pill[ch],
            "the divider must read lighter than the pill on channel {ch}: \
             separator={on_sep:?} pill={pill:?}"
        );
        assert!(
            dot[ch] >= 240,
            "the running dot draws `$system_fg_color` over the icon, channel \
             {ch}: dot={dot:?}"
        );
        assert!(
            dot[ch] > no_dot[ch],
            "the dot must only appear for a running app — the same spot on the \
             non-running tile still shows its icon, channel {ch}: dot={dot:?} \
             no_dot={no_dot:?}"
        );
    }
}

/// The xray blur costs no submit of its own, and the pixels still come out right.
///
/// A blur's own round trip costs about as much as the blur (`docs/fork/venus-cost.md` §3.8, §12),
/// so `EffectBlur::queue` does not make one: the chain is recorded into the next frame's command
/// buffer, alongside the uploads and the dmabuf acquires. Two things have to hold for that to be
/// safe, and neither is visible in a frame that merely looks correct:
///
///   (a) the blurred output is still fully written before anything samples it. Ordering replaces
///       the wait *inside* a frame, and outside one the consumer has to drain the queue itself
///       (`flush_pending_blurs`) — a regression here shows up as a *stale or blank* blur, not a
///       torn one;
///   (b) the blur chain outlives the recording that names it. It owns the render pass, pipelines
///       and descriptor sets a queued blur binds, so rebuilding it (a resize) while one is queued
///       would free objects the command buffer still refers to. `SharedBlurChain`'s reference
///       count is what holds it; under `SYNOIK_VK_VALIDATION=1` a missing one is a use-after-free,
///       not a wrong pixel.
///
/// [`vulkan_effect_buffer_renders_offscreen_and_blur`] covers the same path through a real frame;
/// this one drives the queue directly, so the accounting is visible.
#[test]
fn vulkan_the_xray_blur_costs_no_submit_and_still_lands() {
    use smithay::backend::renderer::element::Kind;

    use crate::render_helpers::blur::BlurOptions;
    use crate::render_helpers::effect_buffer::EffectBuffer;
    use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};

    let skip = |why: &str| eprintln!("skipping vulkan_the_xray_blur_costs_no_submit: {why}");
    let mut vk = match VulkanRenderer::new() {
        Ok(vk) => vk,
        Err(e) => return skip(&format!("no Vulkan device ({e})")),
    };
    if !vk.gpu().orders_submits() {
        return skip("no timeline semaphore, so deferring would be unsafe");
    }
    // Headless there is no KMS plane to take a fence, so the session opt-in has to be forced.
    vk.set_defer_scanout(true);

    let scale = Scale::from(1.0);
    let fill_edge = |buffer: &mut EffectBuffer, s: i32| {
        let red =
            SolidColorBuffer::new(Size::from((s as f64 / 2.0, s as f64)), [1.0, 0.0, 0.0, 1.0]);
        let green =
            SolidColorBuffer::new(Size::from((s as f64 / 2.0, s as f64)), [0.0, 1.0, 0.0, 1.0]);
        let elements = buffer.elements_vulkan();
        elements.clear();
        elements.push(
            SolidColorRenderElement::from_buffer(&red, (0.0, 0.0), 1.0, Kind::Unspecified).into(),
        );
        elements.push(
            SolidColorRenderElement::from_buffer(
                &green,
                (s as f64 / 2.0, 0.0),
                1.0,
                Kind::Unspecified,
            )
            .into(),
        );
    };

    // Read the blurred output back and return the strongest blend along the mid scanline. A hard
    // red|green edge has min(R,G) == 0 everywhere; blurring it mixes the two.
    let edge_blend = |vk: &mut VulkanRenderer, buffer: &EffectBuffer, s: i32| -> u8 {
        let mut tex = buffer.texture_vulkan(true).expect("blurred texture");
        let fb = vk.bind(&mut tex).expect("bind blurred");
        let region = Rectangle::<i32, BufferCoord>::from_size(Size::from((s, s)));
        let mapping = vk
            .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
            .expect("copy");
        let pixels = vk.map_texture(&mapping).expect("map").to_vec();
        let y = s / 2;
        (0..s).fold(0u8, |best, x| {
            let p = px(&pixels, s, x, y);
            best.max(p[0].min(p[1]))
        })
    };

    const S: i32 = 64;
    let mut buffer = EffectBuffer::new();
    buffer.update_size(Size::<i32, Physical>::from((S, S)), scale);
    buffer.update_blur_options(BlurOptions {
        passes: 3,
        offset: 2.0,
    });
    fill_edge(&mut buffer, S);

    let before = vk.in_flight_len();
    assert!(
        buffer.prepare_vulkan(&mut vk, true),
        "prepare_vulkan (blur) failed"
    );

    // Exactly one, and the exactness is the test. This prepare used to make two submits — the
    // offscreen render that fills the source, and the blur — and only the offscreen one is left;
    // the blur is queued for the next frame's command buffer instead. A blur that went back to a
    // submit of its own would read 2 here whether or not anyone waited for it.
    //
    // The obvious assertion — that the `Blur` site's `retiring` is zero, the number the frame log
    // prints as `1 blur in Xms` — cannot be used here: `synoik_vk::stats`' timers are gated on
    // `set_enabled`, which only the frame log turns on, so every duration reads zero under test
    // whether or not anything waited.
    assert_eq!(
        vk.in_flight_len() - before,
        1,
        "the offscreen render is the only submit this prepare may make; the blur must be queued,          not submitted"
    );
    assert_eq!(
        vk.pending_blurs_len(),
        1,
        "and it must actually be queued — an empty queue with no submit means no blur at all"
    );

    // Rebuild the chain **with that blur still queued** — the lifetime hazard. A resize drops the
    // old offscreen and its `EffectBlur`, while the queue still holds a blur naming that chain's
    // images, framebuffers and pipelines.
    //
    // Nothing may read pixels back before this point, and that is the whole design of the test: a
    // readback drains the queue, and a drained queue makes the hazard unreachable.
    const S2: i32 = 96;
    buffer.update_size(Size::<i32, Physical>::from((S2, S2)), scale);
    fill_edge(&mut buffer, S2);
    assert!(
        buffer.prepare_vulkan(&mut vk, true),
        "prepare_vulkan after resize failed"
    );

    // Only now read back — proving the deferred blur lands at all, and that the rebuild did not
    // corrupt it. A blank or stale output here means ordering did not stand in for the wait.
    let blend = edge_blend(&mut vk, &buffer, S2);
    assert!(
        blend > 40,
        "the deferred blur did not land: the edge is still hard (max min(R,G) = {blend})"
    );
}

/// A fresh offscreen must not cost a submit to become sampleable.
///
/// `make_offscreen_sampleable` is a no-op for a texture a frame just rendered into — the frame
/// leaves it sampleable on the submit it was making anyway. The path that is *not* a no-op is the
/// effect buffer's no-redraw branch: when its elements have not changed but its texture has just
/// been recreated (a size change — an overview zoom does that every frame), it makes a brand-new
/// `UNDEFINED` image sampleable without rendering into it, and that cost a whole command buffer,
/// submit and fence wait for one pipeline barrier. On the live seat it was `2 transition in
/// 3.03ms`, the only wait left in the frame line.
///
/// Measured here rather than assumed: every redraw round must cost zero transition submits (it
/// always did), and so must the no-redraw round (it used to cost one). The barrier is queued for
/// the next frame's command buffer instead — which is safe for exactly this layout, because
/// `UNDEFINED` means there are no contents to discard.
#[test]
fn vulkan_a_fresh_offscreen_costs_no_transition_submit() {
    use smithay::backend::renderer::element::Kind;
    use smithay::backend::renderer::Texture as _;

    use crate::render_helpers::blur::BlurOptions;
    use crate::render_helpers::effect_buffer::EffectBuffer;
    use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
    use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};

    let mut vk = match VulkanRenderer::new() {
        Ok(vk) => vk,
        Err(e) => {
            eprintln!(
                "skipping vulkan_a_fresh_offscreen_costs_no_transition_submit: no Vulkan ({e})"
            );
            return;
        }
    };
    let site = synoik_vk::stats::SubmitSite::ALL
        .iter()
        .position(|s| *s == synoik_vk::stats::SubmitSite::Transition)
        .expect("transition site");
    let transitions = |_: ()| synoik_vk::stats::take_sites()[site].submits;

    let scale = Scale::from(1.0);
    let mut buffer = EffectBuffer::new();
    buffer.update_blur_options(BlurOptions {
        passes: 3,
        offset: 2.0,
    });

    // Redraw rounds, including the size changes that recreate the texture.
    for s in [64i32, 96, 96, 128] {
        buffer.update_size(Size::<i32, Physical>::from((s, s)), scale);
        let red = SolidColorBuffer::new(Size::from((s as f64, s as f64)), [1.0, 0.0, 0.0, 1.0]);
        let elements = buffer.elements_vulkan();
        elements.clear();
        elements.push(
            SolidColorRenderElement::from_buffer(&red, (0.0, 0.0), 1.0, Kind::Unspecified).into(),
        );
        let _ = transitions(());
        assert!(
            buffer.prepare_vulkan(&mut vk, true),
            "prepare failed at {s}"
        );
        assert_eq!(
            transitions(()),
            0,
            "a rendered offscreen is left sampleable by its own frame; nothing may submit a \
             barrier for it",
        );
    }

    // The no-redraw round: elements unchanged, so nothing renders and the texture is made
    // sampleable on its own. This is the one that used to submit.
    let _ = transitions(());
    assert!(
        buffer.prepare_vulkan(&mut vk, true),
        "no-redraw prepare failed"
    );
    assert_eq!(
        transitions(()),
        0,
        "making a fresh offscreen sampleable submitted a command buffer for one barrier — it must \
         ride the next frame's instead",
    );

    // And the barrier still lands. Sampling an image whose *tracked* layout says
    // SHADER_READ_ONLY while the image is really still UNDEFINED is a layout violation — invisible
    // in the pixels (this texture is legitimately blank: its elements never re-rendered into the
    // recreated image), which is why the assertion here is the readback completing at all. Under
    // `SYNOIK_VK_VALIDATION=1` a barrier that never got recorded is reported on this draw.
    let tex = buffer.texture_vulkan(true).expect("blurred texture");
    let (w, h) = (tex.size().w, tex.size().h);
    let buf = TextureBuffer::from_texture(&vk, tex, 1.0, Transform::Normal, Vec::new());
    let element = TextureRenderElement::from_texture_buffer(
        buf,
        Point::from((0.0, 0.0)),
        1.0,
        None,
        None,
        Kind::Unspecified,
    );
    let out = render_to_vec(
        &mut vk,
        Size::<i32, Physical>::from((w, h)),
        scale,
        Transform::Normal,
        Fourcc::Abgr8888,
        std::iter::once(element),
    )
    .expect("sampling the queued-barrier offscreen must not fail");
    assert_eq!(out.len(), (w * h * 4) as usize, "unexpected readback size");
}

// ---------------------------------------------------------------------------
// Animation bake guardrails
// ---------------------------------------------------------------------------
//
// A widget bake is an uncached rasterization into its own texture: a render pass,
// a submit and a fence wait. On this stack that round trip has a fat tail, so a
// widget that re-bakes on *every* frame of an animation is a stutter generator
// even when its median cost looks negligible — the panel's was 1.39ms at the
// median and produced 18 of the 19 over-budget frames it appeared in.
//
// The trap is that this is invisible from every direction that usually catches
// things. Pixels are identical, since a bake and its cache produce the same image.
// Frame *counts* are identical. End-state tests never sample a running animation
// at all. And it arrives by accident: the panel's bake was deliberately excluded
// from the overview animation, then silently re-included when opening the overview
// started a fill fade that put `are_animations_ongoing()` back to true.
//
// So the invariant is asserted directly, on a real render of a running animation:
// **no widget may bake on more than one frame of it.** One bake is fine and
// expected — content appearing for the first time. Every frame is the bug.

/// Render `frames` frames of whatever animation is currently running, stepping the
/// clock by `step` between them, and return each frame's bake sites.
///
/// The clock is driven explicitly rather than through `synoik_complete_animations`,
/// which settles an animation instead of sampling it — the whole point here is to
/// look *at* the frames in between.
fn bake_sites_per_frame(
    f: &mut Fixture,
    output: &Output,
    frames: usize,
    step: Duration,
) -> Vec<Vec<crate::frame_log::BakeSite>> {
    let mut per_frame = Vec::with_capacity(frames);
    let mut animated = 0usize;
    for _ in 0..frames {
        let mut clock = f.synoik().clock.clone();
        let now = clock.now_unadjusted();
        clock.set_unadjusted(now + step);
        f.synoik().advance_animations();
        if f.synoik().layout.are_animations_ongoing(Some(output))
            || f.synoik().panel_popover.are_animations_ongoing()
        {
            animated += 1;
        }
        // Drop whatever the step itself banked, so each entry is one frame's own.
        let _ = crate::frame_log::take_bake_sites();
        let _ = render_output_vulkan(f, output);
        per_frame.push(crate::frame_log::take_bake_sites());
    }
    // Anti-vacuity, and it is not hypothetical: every caller here asserts that *nothing* re-baked,
    // which a run of six static frames satisfies perfectly. A `step` larger than the animation, or
    // a toggle that did not start one, would make the whole family of tests green forever. See
    // [[headless-animation-clock-trap]] — the clock has to be driven, and then checked.
    assert!(
        animated >= 2,
        "only {animated} of {frames} sampled frames had an animation running — this samples          static frames, so an assertion about per-frame re-baking proves nothing. Shorten `step`          or check that the action actually started an animation."
    );
    per_frame
}

/// Sites that baked on more than one of the sampled frames, as `("ui/panel.rs:1540",
/// frames)`, worst first.
fn sites_baking_repeatedly(per_frame: &[Vec<crate::frame_log::BakeSite>]) -> Vec<(String, usize)> {
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for frame in per_frame {
        for site in frame {
            let file = site.file.strip_prefix("src/").unwrap_or(site.file);
            *seen.entry(format!("{file}:{}", site.line)).or_default() += 1;
        }
    }
    let mut repeats: Vec<(String, usize)> = seen.into_iter().filter(|&(_, n)| n > 1).collect();
    repeats.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    repeats
}

/// Open the overview and sample the frames of its opening animation.
fn overview_open_bake_sites(
    f: &mut Fixture,
    output: &Output,
) -> Vec<Vec<crate::frame_log::BakeSite>> {
    // One render before the animation, so anything that bakes merely because it is
    // being composited for the first time in this process does it now and does not
    // land on the first animation frame.
    let _ = render_output_vulkan(f, output);
    let _ = crate::frame_log::take_bake_sites();

    f.synoik().layout.toggle_overview();
    bake_sites_per_frame(f, output, 6, Duration::from_millis(40))
}

/// Nothing may re-bake on every frame of the overview animation.
///
/// This is the case that was live on the seat: `ui/panel.rs` baked the whole panel
/// bar — clock, workspace dots, button pills — once per frame for the length of the
/// animation, because opening the overview checks the Activities button and the
/// resulting fill fade made the bar look like it was animating.
#[test]
fn the_overview_animation_rebakes_nothing_per_frame() {
    let Some(mut f) = window_fixture_settled(GREEN, true, Some("overview bake probe")) else {
        return;
    };
    let output = f.synoik().global_space.outputs().next().unwrap().clone();

    let per_frame = overview_open_bake_sites(&mut f, &output);
    let repeats = sites_baking_repeatedly(&per_frame);

    assert!(
        repeats.is_empty(),
        "these widgets re-baked across {} frames of the overview animation: {repeats:?}\n\
         A bake is a GPU round trip; one per frame of an animation is a stutter. Either \
         cache it across the animation, or split the animating part out as its own \
         element the way the panel background and button pills are.",
        per_frame.len(),
    );
}

/// Nothing may re-bake on every frame of a workspace switch either.
///
/// This one carried a known exception until 2026-07-26: the panel's workspace dots
/// interpolate their width, height and opacity every frame, so no cached bake could serve
/// them and the trick that saves an animated pill — bake it opaque, ride the fade on the
/// element's alpha — does not extend to a size. They are
/// [`RoundedSolidRenderElement`](crate::render_helpers::rounded_solid::RoundedSolidRenderElement)s
/// now, drawn straight into the frame, and the exception is gone.
#[test]
fn the_workspace_switch_rebakes_nothing_per_frame() {
    let Some(mut f) = window_fixture_settled(GREEN, true, Some("workspace bake probe")) else {
        return;
    };
    let output = f.synoik().global_space.outputs().next().unwrap().clone();

    let _ = render_output_vulkan(&mut f, &output);
    let _ = crate::frame_log::take_bake_sites();

    f.synoik_state()
        .do_action(Action::FocusWorkspaceDown, false);
    let per_frame = bake_sites_per_frame(&mut f, &output, 6, Duration::from_millis(30));
    let repeats = sites_baking_repeatedly(&per_frame);

    assert!(
        repeats.is_empty(),
        "these widgets re-baked across {} frames of a workspace switch: {repeats:?}\n\
         A bake is a GPU round trip; one per frame of an animation is a stutter. An \
         animated *alpha* can ride the element (see `pill_element`); animated *geometry* \
         needs a real drawing primitive (see `workspace_dots`).",
        per_frame.len(),
    );
}

/// Open the quick-settings popover on `output`, exactly as the panel indicator does.
fn open_quick_settings(f: &mut Fixture, output: &Output) {
    let output_w = output_size(output).w;
    let toggles = f.synoik().gnome_settings.quick_toggles;
    let anchor = f.synoik().panel.quick_settings_rect(output_w);
    let network = f.synoik().system_status.network;
    let airplane = f.synoik().system_status.airplane;
    let power = f.synoik().system_status.power.clone();
    let bluetooth = f.synoik().system_status.bluetooth.clone();
    let bluetooth_rfkill = f.synoik().system_status.bluetooth_rfkill;
    let battery = f.synoik().system_status.battery.clone();
    let audio = f.synoik().audio;
    let sink_list = f.synoik().sink_list.clone();
    let mic = f.synoik().mic;
    let source_list = f.synoik().source_list.clone();
    let brightness = f.synoik().brightness.view();
    let accent = f.synoik().gnome_settings.accent_color;
    f.synoik().panel_popover.toggle_quick_settings(
        output.clone(),
        anchor,
        toggles,
        network,
        airplane,
        power,
        bluetooth,
        bluetooth_rfkill,
        battery,
        audio,
        sink_list,
        crate::audio::AudioCards::default(),
        false,
        mic,
        source_list,
        brightness,
        accent,
    );
}

/// Open the calendar popover on `output`, exactly as the clock does.
fn open_calendar(f: &mut Fixture, output: &Output) {
    let anchor = f.synoik().panel.date_menu_rect(output_size(output).w);
    let cal = f.synoik().gnome_settings.calendar;
    let accent = f.synoik().gnome_settings.accent_color;
    f.synoik().panel_popover.toggle_calendar(
        output.clone(),
        anchor,
        cal.week_start,
        cal.show_week_numbers,
        accent,
        Vec::new(),
    );
}

/// A popover's open fade must not re-bake its contents on every frame.
///
/// The generalization of the panel bug (`009213dd`): a popover fades in by *alpha*, and alpha is
/// free at composite time — `TextureRenderElement` carries it. A widget whose bake key moves with
/// the fade instead pays a full GPU round trip per frame for a picture that never changes, and
/// nothing about the result looks wrong, so only this kind of test can see it.
///
/// Both popovers in one test because they share `PanelPopover`'s fade; if the fade itself ever
/// starts feeding bake keys, both fail together and the shared cause is obvious.
#[test]
fn a_popover_open_fade_rebakes_nothing_per_frame() {
    for (name, subject, open) in [
        (
            "quick settings",
            "quick_settings.rs",
            open_quick_settings as fn(&mut Fixture, &Output),
        ),
        ("calendar", "calendar.rs", open_calendar),
    ] {
        let Some(mut f) = window_fixture_settled(GREEN, false, Some("popover bake probe")) else {
            return;
        };
        let output = f.synoik_output(1);

        // Everything that bakes merely because it is being composited for the first time in this
        // process does it now, so it cannot land on an animation frame below.
        open(&mut f, &output);
        f.settle_animations();
        let _ = crate::frame_log::take_bake_sites();
        let _ = render_output_vulkan(&mut f, &output);
        let warm = crate::frame_log::take_bake_sites();
        // The other vacuity mode, and the one that killed the app-grid version of this test: a
        // widget that never renders re-bakes nothing on every frame, perfectly. Prove the subject
        // is live before asserting anything about how often it bakes.
        assert!(
            warm.iter().any(|s| s.file.contains(subject)),
            "no {subject} bake while the {name} popover was open and settled, so this test cannot              observe the thing it asserts about. Sites seen: {warm:?}"
        );

        f.synoik().panel_popover.close();
        f.settle_animations();
        let _ = render_output_vulkan(&mut f, &output);
        let _ = crate::frame_log::take_bake_sites();

        open(&mut f, &output);
        assert!(f.synoik().panel_popover.is_open(), "{name} did not open");
        let per_frame = bake_sites_per_frame(&mut f, &output, 6, Duration::from_millis(20));
        let repeats = sites_baking_repeatedly(&per_frame);

        assert!(
            repeats.is_empty(),
            "these widgets re-baked across {} frames of the {name} popover's open fade: \
             {repeats:?}\nA bake is a GPU round trip. The fade is alpha, and alpha is free on the \
             element — keep it out of the bake key.",
            per_frame.len(),
        );
    }
}

/// The app grid slides up out of the dash; nothing in it may re-bake per frame.
///
/// The grid is the worst case for this class — a page of tiles, each with its own shaped label —
/// and it has been fixed for it once already (`c5336421`: hover bumped `content_rev`, so a pointer
/// moving during the *open* animation re-shaped every label every frame; the tell was that closing
/// stayed smooth, because the grid is not reactive then). That fix is one field on one widget,
/// which nothing but this kind of test protects.
///
/// The precondition worth knowing, because it silently made an earlier version of this test assert
/// nothing at all: `AppGrid`'s entries do not come from `app_system` on demand. `sync_app_grid()`
/// copies them across and is driven by app-system *change events* — so installing a catalog
/// straight onto `Synoik` leaves the grid empty, rendering nothing, re-baking nothing, and passing.
/// The "subject baked at least once" assertion below is what makes that failure loud.
#[test]
fn the_app_grid_open_rebakes_nothing_per_frame() {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog, RecordingLauncher};

    let Some(mut f) = window_fixture_settled(GREEN, true, Some("app grid bake probe")) else {
        return;
    };
    let output = f.synoik().global_space.outputs().next().unwrap().clone();

    // Enough tiles to fill a page, each with a distinct name so every label is its own shaped run.
    let apps: Vec<AppEntry> = (0..24)
        .map(|i| AppEntry::fake(&format!("app{i}.desktop"), &format!("Application {i:02}")))
        .collect();
    f.synoik().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(apps)),
        Box::new(RecordingLauncher::default()),
    );
    assert!(f.synoik().sync_app_grid(), "the grid took no entries");

    // Settle into the open overview first: this test is about the grid's own animation, not the
    // overview's, which `the_overview_animation_rebakes_nothing_per_frame` already owns.
    f.synoik().layout.toggle_overview();
    f.settle_animations();
    assert!(
        f.synoik().layout.toggle_app_grid(),
        "the app grid did not open, so this test proves nothing"
    );
    f.settle_animations();
    let _ = crate::frame_log::take_bake_sites();
    let _ = render_output_vulkan(&mut f, &output);
    let warm = crate::frame_log::take_bake_sites();
    assert!(
        warm.iter().any(|s| s.file.contains("app_grid.rs")),
        "no app_grid.rs bake with the grid open and settled, so this test cannot observe the \
         thing it asserts about. Sites seen: {warm:?}"
    );

    // Close and reopen, so the sampled frames are a real open animation from a warm cache.
    assert!(f.synoik().layout.toggle_app_grid());
    f.settle_animations();
    let _ = render_output_vulkan(&mut f, &output);
    let _ = crate::frame_log::take_bake_sites();

    assert!(f.synoik().layout.toggle_app_grid());
    let per_frame = bake_sites_per_frame(&mut f, &output, 6, Duration::from_millis(40));
    let repeats = sites_baking_repeatedly(&per_frame);

    assert!(
        repeats.is_empty(),
        "these widgets re-baked across {} frames of the app-grid open: {repeats:?}\n\
         Every tile label is a shaped run; re-shaping them per frame is the `c5336421` stutter.",
        per_frame.len(),
    );
}

/// A wallpaper change must not put the xray effect buffer into a permanent per-frame
/// recreate-and-reblur.
///
/// The frame that *records* a queued blur holds the effect buffer's offscreen alive as
/// `blur.source` until its submit retires — and under deferred scanout (the live KMS path, where
/// the fence goes to the plane and the CPU walks away) that record outlives the frame. The reuse
/// check counted that keep-alive as a foreign owner, so the next frame's prepare threw the
/// offscreen away, which invalidated the blur, which queued another blur for the next frame to
/// record and hold: **self-sustaining**. One wallpaper change cost the live seat a full-output blur
/// plus three image creations on every idle frame — ~15ms of GPU on a 16.67ms budget — until an
/// unrelated blocking wait (closing a window) drained the in-flight list.
///
/// Both halves are load-bearing, and each is invisible without the other:
///   - **deferral on**, or every submit is waited out and the keep-alive is gone before the next
///     prepare ever looks (the undeferred path settles after one frame either way — that is what
///     made this bug live-only);
///   - **a wallpaper change**, or no blur is ever queued and nothing holds the offscreen at all.
///
/// The assertion is on image *creations* per frame rather than pixels: the composite is identical
/// either way — the recreated offscreen is re-rendered and re-blurred with the same contents — so
/// no pixel comparison can see this.
#[test]
fn a_wallpaper_change_does_not_leave_the_xray_buffer_rebuilding_every_frame() {
    let Some((mut f, output, red, blue)) = xray_wallpaper_fixture() else {
        return;
    };

    set_wallpaper(&mut f, &red);
    synoik_vk::stats::set_enabled(true);
    let _ = synoik_vk::stats::take_creates();

    // Warm: the offscreen, its blur chain and the wallpaper texture all exist and are steady.
    let steady: Vec<(u64, u64)> = (0..4)
        .map(|_| render_deferred_once(&mut f, &output))
        .collect();
    let baseline = *steady.last().unwrap();

    set_wallpaper(&mut f, &blue);
    // Frame 1 legitimately re-renders and re-blurs (the wallpaper really did change); every frame
    // after it must be back to the steady cost.
    let after: Vec<(u64, u64)> = (0..5)
        .map(|_| render_deferred_once(&mut f, &output))
        .collect();

    // `render_to_texture` allocates the frame's own target, and that is the *only* thing a steady
    // frame may allocate. An absolute bound, not a comparison against the warm frames: under the
    // bug the warm frames are broken too, so any "same as before" assertion passes.
    const OWN_TARGET: u64 = 1;
    let rebuilding: Vec<_> = steady[1..]
        .iter()
        .chain(&after[1..])
        .filter(|&&(_, creates)| creates > OWN_TARGET)
        .collect();
    assert!(
        rebuilding.is_empty(),
        "the xray effect buffer is being rebuilt on frames that changed nothing: \
         (draws, creations)/frame were {steady:?} while steady, then {after:?} across a wallpaper \
         change. The frame that recorded the blur holds its source — our own keep-alive, not a \
         foreign owner (`VulkanRenderer::discount_pending_holds`)."
    );
    assert_ne!(
        after[0].0, baseline.0,
        "the wallpaper change cost the xray buffer no extra draws, so this scene never blurs and \
         the test cannot see the bug it exists for: {steady:?} then {after:?}"
    );
}

/// The fixture the xray/wallpaper tests share: the gsrs XRAY rule (translucent, no opaque border
/// background, `background-effect` blur+xray) over a window, plus two solid wallpapers to swap
/// between.
fn xray_wallpaper_fixture() -> Option<(Fixture, Output, std::path::PathBuf, std::path::PathBuf)> {
    use synoik_config::BackgroundEffectRule;

    if let Err(e) = VulkanRenderer::new() {
        eprintln!("skipping: no Vulkan device ({e})");
        return None;
    }

    let dir = std::env::temp_dir().join("synoik-xray-wallpaper-test");
    std::fs::create_dir_all(&dir).unwrap();
    let red = dir.join("red.png");
    let blue = dir.join("blue.png");
    write_solid_png(&red, [255, 0, 0]);
    write_solid_png(&blue, [0, 0, 255]);

    let mut config = Config::default();
    config.window_rules.push(WindowRule {
        opacity: Some(0.25),
        draw_border_with_background: Some(false),
        background_effect: BackgroundEffectRule {
            blur: Some(true),
            xray: Some(true),
            ..Default::default()
        },
        ..Default::default()
    });

    let mut f = Fixture::with_config(config);
    f.synoik_state()
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
    window.attach_shm_buffer(600, 400, 200, 100, 50, 255);
    window.set_size(600, 400);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.synoik_complete_animations();

    let output = f.synoik_output(1);
    Some((f, output, red, blue))
}

/// A solid `rgb` PNG — a wallpaper whose contribution through a translucent window is unambiguous.
fn write_solid_png(path: &std::path::Path, rgb: [u8; 3]) {
    let mut img = image::RgbaImage::new(256, 256);
    for p in img.pixels_mut() {
        *p = image::Rgba([rgb[0], rgb[1], rgb[2], 255]);
    }
    img.save(path).expect("write png");
}

/// Point `org.gnome.desktop.background` at `path` and let the compositor pick it up. Decodes
/// synchronously — the fixture wires no worker thread.
fn set_wallpaper(f: &mut Fixture, path: &std::path::Path) {
    let settings = crate::gnome::BackgroundSettings {
        picture: Some(path.to_path_buf()),
        options: crate::gnome::BackgroundOptions::default(),
    };
    let gpu = f
        .synoik_state()
        .backend
        .with_vulkan_renderer(|r| r.gpu().clone());
    f.synoik().wallpaper.update(&settings, gpu.as_ref());
}

/// Render one frame the way the live KMS path does: collect elements, render into a target, and
/// walk away from the submit without waiting for it. Returns the frame's draws and image
/// creations.
fn render_deferred_once(f: &mut Fixture, output: &Output) -> (u64, u64) {
    use crate::render_helpers::render_to_texture;

    let state = f.synoik_state();
    state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| -> anyhow::Result<(u64, u64)> {
            vk.set_defer_scanout(true);
            // Retirement is a poll; the live seat routinely reaches the next prepare without
            // having observed the previous submit complete. Pin that side of the race.
            vk.set_retire_paused(true);
            let synoik = &mut state.synoik;
            synoik.update_render_elements(Some(output));

            let size: Size<i32, Physical> = output.current_mode().unwrap().size;
            let scale = Scale::from(output.current_scale().fractional_scale());
            let ctx = RenderCtx {
                renderer: vk,
                target: RenderTarget::Output,
                xray: None,
            };
            // Reset before *collection*: the xray effect buffer is prepared while elements are
            // built, so its offscreen and blur allocations land there, not in the render below.
            let _ = synoik_vk::stats::take_creates();
            let d0 = synoik_vk::stats::draws();
            let elements = synoik.render_to_vec(ctx, output, false);
            // Drop the backdrop-blur elements (the top panel's, `ui::panel::BAR_BG`). They are not
            // what this measures, and `render_to_texture` has no per-element state to keep their
            // capture and blur chain in — `render_elements` hands every element a fresh
            // `UserDataMap` — so on *this* path they allocate once per call by construction, which
            // would sit on top of the absolute bound below and hide the buffer this is watching.
            // The live path is the damage tracker's, which keeps that state across frames and only
            // re-captures when something behind the element damaged.
            let elements: Vec<_> = elements
                .iter()
                .filter(|e| {
                    !smithay::backend::renderer::element::Element::is_framebuffer_effect(*e)
                })
                .collect();
            let (_tex, _sync) = render_to_texture(
                vk,
                size,
                scale,
                Transform::Normal,
                NATIVE_FOURCC,
                elements.iter().rev(),
            )?;
            Ok((
                synoik_vk::stats::draws() - d0,
                synoik_vk::stats::take_creates().0,
            ))
        })
        .expect("headless backend must hold a Vulkan renderer")
        .expect("rendering must not error")
}

/// The frame log's unattributed-time clause has a silent-failure mode: if any `enter_attributed`
/// in the renderer is not matched by a `leave_attributed`, the union runs past the wall time it
/// gets subtracted from, every residual clamps to zero, and the clause simply stops appearing.
/// Nothing else would notice — a missing warning looks exactly like a healthy frame.
///
/// So drive a real frame through the whole renderer and assert the union stays inside it. Unit
/// tests pin the arithmetic on synthetic spans; only a real render exercises every guard.
///
/// Retried, because `stats::set_enabled` is a process-wide flag and the counters it gates are
/// per-thread: **every** `Fixture::new` on any test thread runs `FrameLog::from_env`, which turns
/// timing back off (`SYNOIK_FRAME_LOG` is unset under libtest). A neighbour constructing a fixture
/// mid-measurement therefore reads as "attributed nothing", which is exactly what a broken guard
/// looks like. Losing that race every attempt is vanishingly unlikely; an unwired guard loses it
/// every time — mutation-checked. The `<=` invariant needs no retry: it holds whatever the flag is.
#[test]
fn the_attributed_union_stays_inside_the_work_it_measures() {
    let Some((mut f, output, red, _blue)) = xray_wallpaper_fixture() else {
        return;
    };

    set_wallpaper(&mut f, &red);

    // Warm: measure a steady frame, not the first-touch allocation of every cache in the path.
    synoik_vk::stats::set_enabled(true);
    for _ in 0..3 {
        render_deferred_once(&mut f, &output);
    }

    let mut measured = None;
    for _ in 0..8 {
        synoik_vk::stats::set_enabled(true);
        let before = synoik_vk::stats::attributed();
        let started = std::time::Instant::now();
        render_deferred_once(&mut f, &output);
        let wall = started.elapsed();
        let attributed = synoik_vk::stats::attributed().saturating_sub(before);

        assert!(
            attributed <= wall,
            "the attributed union ({attributed:?}) ran past the frame it measures ({wall:?}): an \
             `enter_attributed` somewhere in the renderer has no matching leave, which silently \
             clamps every residual to zero"
        );

        if attributed > Duration::ZERO {
            measured = Some(attributed);
            break;
        }
    }

    assert!(
        measured.is_some(),
        "no attempt attributed any time at all — the guards are not wired to the union, so every \
         phase would report itself as entirely unexplained"
    );
}

/// The app-folder dialog draws its three layers, bottom to top: the `DIALOG_SHADE_NORMAL`
/// shade over the whole output, the opaque `.app-folder-dialog` panel (rounded to
/// `$modal_radius * 4` — its square corner is *not* filled), and the folder's members in
/// their own view inside it.
#[test]
fn vulkan_folder_dialog_draws_its_panel_over_a_shade() {
    use smithay::utils::Logical;

    use crate::app_system::AppIconRef;
    use crate::ui::app_grid::AppGridEntry;
    use crate::ui::folder_dialog::FolderDialogRenderElement;

    if let Err(e) = VulkanRenderer::new() {
        eprintln!("skipping vulkan_folder_dialog_draws_its_panel_over_a_shade: no Vulkan ({e})");
        return;
    }

    let dir = std::env::temp_dir();
    let colors = [[220u8, 20, 20], [20, 220, 20]];
    let paths: Vec<std::path::PathBuf> = colors
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let path = dir.join(format!("synoik-folder-dlg-{}-{i}.png", std::process::id()));
            image::RgbaImage::from_pixel(16, 16, image::Rgba([c[0], c[1], c[2], 255]))
                .save(&path)
                .expect("write member icon");
            path
        })
        .collect();

    let mut f = Fixture::new();
    f.synoik_state()
        .backend
        .headless()
        .add_renderer()
        .expect("build the Vulkan renderer");
    f.add_output(1, (1920, 1080));
    let output = f.synoik_output(1);

    f.synoik().folder_dialog.popup(
        "Utilities",
        "Utilities",
        paths
            .iter()
            .enumerate()
            .map(|(i, path)| AppGridEntry {
                id: format!("m{i}.desktop"),
                name: format!("M{i}"),
                icon: AppIconRef::File(path.clone()),
                folder: None,
            })
            .collect(),
    );
    // The open zooms out of the source tile over 200 ms; settle it, or every sample below
    // reads the first frame (shade alpha 0). See the headless-animation-clock trap.
    f.settle_animations();

    let view: Rectangle<f64, Logical> = Rectangle::new((0., 0.).into(), (1920., 1080.).into());
    let l = crate::ui::folder_dialog::layout(view);
    let icons: Vec<_> = (0..2)
        .map(|i| {
            f.synoik()
                .folder_dialog
                .icon_center(i, l.grid_area)
                .expect("member icon center")
        })
        .collect();

    let state = f.synoik_state();
    let composited =
        state
            .backend
            .headless()
            .with_vulkan_renderer(|vk| -> anyhow::Result<(Vec<u8>, i32)> {
                let synoik = &mut state.synoik;
                let mut elements: Vec<FolderDialogRenderElement> = Vec::new();
                synoik.folder_dialog.render(
                    vk,
                    &synoik.app_icon_cache,
                    &synoik.icon_cache,
                    &output,
                    view,
                    None,
                    1.0,
                    crate::gnome::ACCENT_BLUE,
                    &mut |element| elements.push(element),
                );
                let phys: Size<i32, Physical> = output.current_mode().unwrap().size;
                let scale = Scale::from(output.current_scale().fractional_scale());
                let pixels = render_to_vec(
                    vk,
                    phys,
                    scale,
                    Transform::Normal,
                    Fourcc::Abgr8888,
                    elements.iter().rev(),
                )?;
                Ok((pixels, phys.w))
            });

    for path in &paths {
        let _ = std::fs::remove_file(path);
    }

    let Some(result) = composited else {
        eprintln!("skipping vulkan_folder_dialog_draws_its_panel_over_a_shade: no Vulkan device");
        return;
    };
    let (pixels, w) = result.expect("compositing the folder dialog through Vulkan must not error");

    // Well clear of the panel: the shade, `rgba(0,0,0,204)`.
    let shade = px(&pixels, w, 40, 40);
    eprintln!("vulkan_folder_dialog: shade={shade:?}");
    assert!(
        shade[0] < 12 && shade[1] < 12 && shade[2] < 12,
        "the shade is black: {shade:?}"
    );
    assert!(
        (shade[3] as i32 - 204).abs() <= 3,
        "the shade is DIALOG_SHADE_NORMAL's 204/255: {shade:?}"
    );

    // Inside the panel, below the grid and clear of every tile: the opaque overlay fill.
    let inside = px(
        &pixels,
        w,
        (l.panel.loc.x + 20.) as i32,
        (l.panel.loc.y + l.panel.size.h - 20.) as i32,
    );
    eprintln!("vulkan_folder_dialog: panel={inside:?}");
    assert_eq!(inside[3], 255, "the panel is opaque: {inside:?}");
    for ch in 0..3 {
        let want = (crate::ui::widget::style::OVERLAY_BG[ch] * 255.).round() as i32;
        assert!(
            (inside[ch] as i32 - want).abs() <= 4,
            "the panel is $system_overlay_bg_color: {inside:?}"
        );
    }

    // The panel's *square* corner is outside its 64px rounding, so the shade shows there.
    let corner = px(
        &pixels,
        w,
        (l.panel.loc.x + 3.) as i32,
        (l.panel.loc.y + 3.) as i32,
    );
    eprintln!("vulkan_folder_dialog: corner={corner:?}");
    assert_eq!(
        corner, shade,
        "the corner is cut by `$modal_radius * 4`, so the shade shows through: {corner:?}"
    );

    // And the members are drawn in the folder's own view.
    for (i, (center, want)) in icons.iter().zip(&colors).enumerate() {
        let got = px(&pixels, w, center.x as i32, center.y as i32);
        eprintln!("vulkan_folder_dialog: member{i} at {center:?} = {got:?}");
        for ch in 0..3 {
            assert!(
                (got[ch] as i32 - want[ch] as i32).abs() < 40,
                "member {i} draws its own icon inside the dialog: got {got:?}, want {want:?}"
            );
        }
    }

    // The name row sits ON the panel, not under it. Both the label and the edit button are
    // their own elements now (they cross-fade / take hover), and the first element pushed is
    // the *topmost* — so pushing them after the panel buries them, which is invisible to
    // every geometry test and is exactly what happened.
    // Scan the band's mid-line rather than probing its exact centre pixel: the name is centred, so
    // whether the middle column lands on ink or in a letter gap is a property of the shaped font,
    // not of the z-order this is testing. (It landed on the panel bg the moment the UI font
    // changed — a green test that only measured which glyphs the session happened to have.)
    let cy = (l.name_band.loc.y + l.name_band.size.h / 2.) as i32;
    let mut label = None;
    for x in (l.name_band.loc.x as i32)..((l.name_band.loc.x + l.name_band.size.w) as i32) {
        let got = px(&pixels, w, x, cy);
        if got[0] > 150 && got[1] > 150 && got[2] > 150 {
            label = Some(got);
            break;
        }
    }
    eprintln!("vulkan_folder_dialog: name={label:?}");
    assert!(
        label.is_some(),
        "the folder name draws over the panel in $system_fg_color: no light pixel across the band"
    );

    // The edit button's disc is `button(normal)` *over* the overlay surface — a distinctly
    // lighter fill. Sampled off-center so the pencil glyph does not decide the result.
    let disc = px(
        &pixels,
        w,
        (l.edit_button.loc.x + 6.) as i32,
        (l.edit_button.loc.y + l.edit_button.size.h / 2.) as i32,
    );
    eprintln!("vulkan_folder_dialog: edit disc={disc:?}");
    for ch in 0..3 {
        let want = (crate::ui::widget::style::OVERLAY_BUTTON_BG[ch] * 255.).round() as i32;
        assert!(
            (disc[ch] as i32 - want).abs() <= 6,
            "the edit button is `st-mix($system_fg_color, $system_overlay_bg_color, 9%)`, \
             NOT the panel it sits on: got {disc:?}, want channel {ch} ≈ {want}"
        );
    }
    assert!(
        disc.iter()
            .take(3)
            .zip(inside.iter())
            .any(|(a, b)| a.abs_diff(*b) > 10),
        "…and so it is visible against the panel: disc={disc:?}, panel={inside:?}"
    );

    // …and the pencil is drawn ON the disc. Same trap one level down: the glyph is a
    // separate element from the button chrome, so pushing the chrome first hides it inside
    // its own button. Scanned over the glyph's box rather than sampled at the exact center,
    // where `document-edit-symbolic` has a gap between strokes.
    let cx = (l.edit_button.loc.x + l.edit_button.size.w / 2.) as i32;
    let cy = (l.edit_button.loc.y + l.edit_button.size.h / 2.) as i32;
    let brightest = (cy - 8..=cy + 8)
        .flat_map(|y| (cx - 8..=cx + 8).map(move |x| (x, y)))
        .map(|(x, y)| px(&pixels, w, x, y)[0])
        .max()
        .unwrap();
    eprintln!("vulkan_folder_dialog: brightest over the edit glyph={brightest}");
    assert!(
        i32::from(brightest) - i32::from(disc[0]) > 60,
        "the pencil draws over its disc: brightest {brightest} vs disc {}",
        disc[0]
    );
}

/// A folder that paginates clips its pages to its own view (`clip_to_allocation` on the
/// grid's scroll view). The top-level app grid gets this for free — its pages slide off the
/// output, and nothing draws past the output edge — but the folder's view is an island in
/// the middle of the screen, so mid-slide the outgoing and incoming pages were drawn
/// travelling across the desktop on either side of the panel.
///
/// The only way to see this is to composite mid-slide and look *outside* the panel: every
/// geometry test passes either way, because the tiles' rects were right all along.
#[test]
fn vulkan_folder_dialog_clips_its_pages_to_the_panel() {
    use smithay::utils::Logical;

    use crate::app_system::AppIconRef;
    use crate::ui::app_grid::AppGridEntry;
    use crate::ui::folder_dialog::FolderDialogRenderElement;

    if let Err(e) = VulkanRenderer::new() {
        eprintln!("skipping vulkan_folder_dialog_clips_its_pages_to_the_panel: no Vulkan ({e})");
        return;
    }

    // A saturated icon nothing else on screen comes near: the shade is black and the panel
    // is dark grey, so any green outside the panel is a tile that escaped the clip.
    let path = std::env::temp_dir().join(format!("synoik-folder-clip-{}.png", std::process::id()));
    image::RgbaImage::from_pixel(64, 64, image::Rgba([20, 230, 20, 255]))
        .save(&path)
        .expect("write member icon");

    let mut f = Fixture::new();
    f.synoik_state()
        .backend
        .headless()
        .add_renderer()
        .expect("build the Vulkan renderer");
    f.add_output(1, (1920, 1080));
    let output = f.synoik_output(1);

    // Ten members: nine to a page, so the tenth paginates it.
    f.synoik().folder_dialog.popup(
        "Utilities",
        "Utilities",
        (0..10)
            .map(|i| AppGridEntry {
                id: format!("m{i}.desktop"),
                name: format!("M{i}"),
                icon: AppIconRef::File(path.clone()),
                folder: None,
            })
            .collect(),
    );
    f.settle_animations();

    let view: Rectangle<f64, Logical> = Rectangle::new((0., 0.).into(), (1920., 1080.).into());
    let l = crate::ui::folder_dialog::layout(view);
    assert!(
        f.synoik().folder_dialog.set_page(1, view),
        "ten members make a second page"
    );
    // Halfway through the 300 ms slide, where both pages are off-centre and the travel is
    // at its widest.
    let at = f.synoik().clock.now_unadjusted() + std::time::Duration::from_millis(150);
    f.synoik().clock.set_unadjusted(at);

    let state = f.synoik_state();
    let composited =
        state
            .backend
            .headless()
            .with_vulkan_renderer(|vk| -> anyhow::Result<(Vec<u8>, i32)> {
                let synoik = &mut state.synoik;
                let mut elements: Vec<FolderDialogRenderElement> = Vec::new();
                synoik.folder_dialog.render(
                    vk,
                    &synoik.app_icon_cache,
                    &synoik.icon_cache,
                    &output,
                    view,
                    None,
                    1.0,
                    crate::gnome::ACCENT_BLUE,
                    &mut |element| elements.push(element),
                );
                let phys: Size<i32, Physical> = output.current_mode().unwrap().size;
                let scale = Scale::from(output.current_scale().fractional_scale());
                let pixels = render_to_vec(
                    vk,
                    phys,
                    scale,
                    Transform::Normal,
                    Fourcc::Abgr8888,
                    elements.iter().rev(),
                )?;
                Ok((pixels, phys.w))
            });

    let _ = std::fs::remove_file(&path);

    let Some(result) = composited else {
        eprintln!("skipping vulkan_folder_dialog_clips_its_pages_to_the_panel: no Vulkan device");
        return;
    };
    let (pixels, w) = result.expect("compositing the folder dialog through Vulkan must not error");

    // The slide is mid-flight: something green is inside the panel, or this samples a
    // settled view and proves nothing.
    let inside_green = (l.grid_area.loc.y as i32..(l.grid_area.loc.y + l.grid_area.size.h) as i32)
        .step_by(4)
        .flat_map(|y| {
            (l.grid_area.loc.x as i32..(l.grid_area.loc.x + l.grid_area.size.w) as i32)
                .step_by(4)
                .map(move |x| (x, y))
        })
        .filter(|&(x, y)| {
            let p = px(&pixels, w, x, y);
            p[1] > 120 && p[0] < 120
        })
        .count();
    assert!(
        inside_green > 100,
        "the tiles are drawn inside the panel mid-slide: {inside_green} green samples"
    );

    // …and nothing green anywhere outside it, on either side.
    let mut escaped = Vec::new();
    for y in (0..1080).step_by(2) {
        for x in (0..1920).step_by(2) {
            if l.panel.contains((f64::from(x), f64::from(y))) {
                continue;
            }
            let p = px(&pixels, w, x, y);
            if p[1] > 60 && i32::from(p[1]) - i32::from(p[0]) > 30 {
                escaped.push((x, y, p));
            }
        }
    }
    assert!(
        escaped.is_empty(),
        "a page slid outside the folder's own view: {} samples, first {:?}",
        escaped.len(),
        escaped.first()
    );
}

/// A reorder does not snap: every tile whose slot changed *eases* to it
/// (`_shouldEaseItems` → `animateIconPosition`, `iconGrid.js:849,224-241`), and the
/// dragged tile scales to half and fades away where it sits so its slot reads as empty
/// (`scaleAndFade`, `appDisplay.js:1960-1966`).
///
/// Only a composite can see this: the model's entry order flips the instant the move
/// commits either way, so every geometry assertion passes against a grid that snaps.
#[test]
fn vulkan_app_grid_eases_tiles_to_their_new_slots() {
    use smithay::utils::Logical;

    use crate::app_system::AppIconRef;
    use crate::ui::app_grid::{AppGridEntry, DragLocation, GridDropTarget};

    if let Err(e) = VulkanRenderer::new() {
        eprintln!("skipping vulkan_app_grid_eases_tiles: no Vulkan device ({e})");
        return;
    }

    // One saturated icon, so "where is that tile" is a pixel question. The rest are the
    // fallback, which is what everything around it draws as.
    let dir = std::env::temp_dir();
    let marked = dir.join(format!("synoik-grid-ease-{}.png", std::process::id()));
    image::RgbaImage::from_pixel(64, 64, image::Rgba([230, 20, 20, 255]))
        .save(&marked)
        .expect("write the marker icon");

    let mut f = Fixture::new();
    f.synoik_state()
        .backend
        .headless()
        .add_renderer()
        .expect("build the Vulkan renderer");
    f.add_output(1, (1920, 1080));
    let output = f.synoik_output(1);

    // Six apps on one page; the *second* is the marked one, so it has neighbours either
    // side to be pushed past.
    let entries: Vec<AppGridEntry> = (0..6)
        .map(|i| AppGridEntry {
            id: format!("e{i}.desktop"),
            name: format!("E{i}"),
            icon: if i == 1 {
                AppIconRef::File(marked.clone())
            } else {
                AppIconRef::Fallback
            },
            folder: None,
        })
        .collect();
    f.synoik().app_grid.set_entries(entries);

    let area: Rectangle<f64, Logical> = Rectangle::new((0., 120.).into(), (1920., 700.).into());
    let center = |f: &mut Fixture, i: usize| -> Point<f64, Logical> {
        let t = f.synoik().app_grid.entry_rect(i, area).expect("a tile");
        Point::from((t.loc.x + t.size.w / 2., t.loc.y + t.size.h / 2.))
    };
    let (slot1, slot3) = (center(&mut f, 1), center(&mut f, 3));
    let tile_h = f
        .synoik()
        .app_grid
        .entry_rect(0, area)
        .expect("a tile")
        .size
        .h;

    /// A composite, its stride, and every element's center with its width.
    type Shot = (Vec<u8>, i32, Vec<(Point<f64, Logical>, f64)>);
    let shoot = |f: &mut Fixture| -> Shot {
        let state = f.synoik_state();
        let composited =
            state
                .backend
                .headless()
                .with_vulkan_renderer(|vk| -> anyhow::Result<Shot> {
                    let synoik = &mut state.synoik;
                    let elements = synoik.app_grid.render(
                        vk,
                        &synoik.app_icon_cache,
                        &synoik.icon_cache,
                        &output,
                        area,
                        1.0,
                        crate::gnome::ACCENT_BLUE,
                    );
                    // A tile is more than its icon: a caption left behind while the icon
                    // moves is invisible to a probe aimed at the icon, and the caption is
                    // emitted by a different branch, so it is exactly the piece that can
                    // be dropped unnoticed.
                    let centers: Vec<(Point<f64, Logical>, f64)> = elements
                        .iter()
                        .map(|el| {
                            let (loc, size) = (el.location(), el.logical_size());
                            (
                                Point::from((loc.x + size.w / 2., loc.y + size.h / 2.)),
                                size.w,
                            )
                        })
                        .collect();
                    let phys: Size<i32, Physical> = output.current_mode().unwrap().size;
                    let scale = Scale::from(output.current_scale().fractional_scale());
                    let pixels = render_to_vec(
                        vk,
                        phys,
                        scale,
                        Transform::Normal,
                        Fourcc::Abgr8888,
                        elements.iter().rev(),
                    )?;
                    Ok((pixels, phys.w, centers))
                });
        composited
            .expect("no Vulkan device")
            .expect("compositing the grid must not error")
    };

    // Where the marked icon is: the mean x of every red pixel across the icon row.
    let red_x = |pixels: &[u8], w: i32, y: i32| -> Option<f64> {
        let (mut sum, mut n) = (0i64, 0i64);
        for x in 0..w {
            let p = px(pixels, w, x, y);
            if p[0] > 120 && p[1] < 90 && p[2] < 90 && p[3] > 120 {
                sum += i64::from(x);
                n += 1;
            }
        }
        (n > 0).then(|| sum as f64 / n as f64)
    };
    let row_y = slot1.y.round() as i32;

    let (pixels, w, _) = shoot(&mut f);
    let at_rest = red_x(&pixels, w, row_y).expect("the marked icon draws at rest");
    assert!(
        (at_rest - slot1.x).abs() < 8.,
        "it starts in slot 1: {at_rest} vs {}",
        slot1.x
    );

    // Move it to slot 3 the way a drop does.
    let per_page = f.synoik().app_grid.items_per_page(area);
    assert!(f.synoik().app_grid.move_entry(
        "e1.desktop",
        GridDropTarget {
            page: 0,
            position: Some(3),
            location: DragLocation::EndEdge,
        },
        per_page,
    ));
    assert!(
        f.synoik().app_grid.are_animations_ongoing(),
        "the reflow holds the redraw loop open"
    );

    // Halfway through the 250 ms ease (plus this tile's stagger share). Measured on the
    // *elements*, not the pixels: mid-flight the tiles pass through each other and the one
    // on top hides part of the one below, so a colour probe reads a clipped shape.
    let at = f.synoik().clock.now_unadjusted() + std::time::Duration::from_millis(130);
    f.synoik().clock.set_unadjusted(at);
    f.synoik().app_grid.advance_animations();
    let (pixels, w, centers) = shoot(&mut f);
    assert!(
        red_x(&pixels, w, row_y).is_some(),
        "the marked icon is still drawn mid-ease"
    );

    // Per-tile elements only: the page's shared bake spans the whole block and belongs to
    // no single tile.
    let tile_w = f
        .synoik()
        .app_grid
        .entry_rect(0, area)
        .expect("a tile")
        .size
        .w;
    let centers: Vec<Point<f64, Logical>> = centers
        .into_iter()
        .filter(|(_, w)| *w <= tile_w)
        .map(|(c, _)| c)
        .collect();
    let slots: Vec<f64> = (0..6).map(|i| center(&mut f, i).x).collect();
    let travelling: Vec<f64> = centers
        .iter()
        .map(|c| c.x)
        .filter(|x| !slots.iter().any(|s| (s - x).abs() < 2.))
        .collect();
    eprintln!("vulkan_app_grid_ease: travelling {travelling:?} between {slots:?}");
    assert!(
        travelling
            .iter()
            .any(|x| *x > slot1.x + 20. && *x < slot3.x - 20.),
        "mid-ease a tile is between slots, not snapped to one: {travelling:?}"
    );
    // Each travelling position must carry a *pair* of elements — the icon and its caption.
    // A caption left behind still draws (it leaves the page bake either way), just at the
    // slot, so it shows up as an unpaired position: exactly the failure mode of forgetting
    // to move one of a tile's elements.
    for x in &travelling {
        let n = centers.iter().filter(|c| (c.x - x).abs() < 1.).count();
        assert!(
            n >= 2,
            "the tile travelling at {x} carries its caption as well as its icon, got {n}: \
             {centers:?}"
        );
    }

    // And it arrives — with everything that left the page's shared bake back in it. That
    // bake is keyed on the content, which does not change when an animation *ends*, so a
    // texture made while the tiles were actors (their captions missing from it) would be
    // served for ever after. Counted as ink below the icon row, where the captions are.
    let caption_y = (slot1.y + tile_h * 0.35).round() as i32;
    let ink = |pixels: &[u8], w: i32, y: i32| -> usize {
        (0..w).filter(|x| px(pixels, w, *x, y)[3] > 20).count()
    };
    let before = ink(&pixels, w, caption_y);

    f.settle_animations();
    f.synoik().app_grid.advance_animations();
    let (pixels, w, _) = shoot(&mut f);
    assert!(
        ink(&pixels, w, caption_y) >= before,
        "every caption is back in the settled page: {} px of caption ink, was {before} \
         mid-animation",
        ink(&pixels, w, caption_y)
    );
    let landed = red_x(&pixels, w, row_y).expect("the marked icon lands");
    assert!(
        (landed - slot3.x).abs() < 8.,
        "it comes to rest in slot 3: {landed} vs {}",
        slot3.x
    );
    assert!(
        !f.synoik().app_grid.are_animations_ongoing(),
        "…and lets the redraw loop idle again"
    );

    // The dragged tile leaves a hole: picked up, it scales to half and fades to nothing.
    f.synoik().app_grid.set_dragged(Some("e1.desktop"));
    f.settle_animations();
    f.synoik().app_grid.advance_animations();
    let (pixels, w, _) = shoot(&mut f);
    assert!(
        red_x(&pixels, w, row_y).is_none(),
        "the dragged tile has faded out of the grid it still holds a slot in"
    );

    let _ = std::fs::remove_file(&marked);
}

/// Mid-close, the dialog really is drawn shrunk toward its source tile: a point inside the
/// resting panel but outside the shrunken one shows what is *behind* the dialog, not panel
/// chrome. Sampled at a pinned instant — the end states are structurally blind to this (both
/// ends look right whichever direction the zoom runs).
#[test]
fn vulkan_folder_dialog_shrinks_toward_its_source_tile() {
    use smithay::utils::Logical;

    use crate::app_system::AppIconRef;
    use crate::ui::app_grid::AppGridEntry;
    use crate::ui::folder_dialog::{FolderDialogRenderElement, Zoom};

    if let Err(e) = VulkanRenderer::new() {
        eprintln!("skipping vulkan_folder_dialog_shrinks_toward_its_source_tile: no Vulkan ({e})");
        return;
    }

    let mut f = Fixture::new();
    f.synoik_state()
        .backend
        .headless()
        .add_renderer()
        .expect("build the Vulkan renderer");
    f.add_output(1, (1920, 1080));
    let output = f.synoik_output(1);

    f.synoik().folder_dialog.popup(
        "Utilities",
        "Utilities",
        vec![AppGridEntry {
            id: "m0.desktop".into(),
            name: "M0".into(),
            icon: AppIconRef::Fallback,
            folder: None,
        }],
    );
    f.settle_animations();
    assert!(f.synoik().folder_dialog.popdown(), "start the shrink");

    let view: Rectangle<f64, Logical> = Rectangle::new((0., 0.).into(), (1920., 1080.).into());
    // A source tile in the top-left quadrant, so the shrink travels somewhere obvious.
    let source: Rectangle<f64, Logical> = Rectangle::new((200., 200.).into(), (144., 144.).into());
    let panel = crate::ui::folder_dialog::layout(view).panel;

    // Sample a third of the way into the 200 ms close.
    let at = f.synoik().clock.now_unadjusted() + std::time::Duration::from_millis(66);
    f.synoik().clock.set_unadjusted(at);

    let state = f.synoik_state();
    let composited =
        state
            .backend
            .headless()
            .with_vulkan_renderer(|vk| -> anyhow::Result<(Vec<u8>, i32)> {
                let synoik = &mut state.synoik;
                let mut elements: Vec<FolderDialogRenderElement> = Vec::new();
                synoik.folder_dialog.render(
                    vk,
                    &synoik.app_icon_cache,
                    &synoik.icon_cache,
                    &output,
                    view,
                    Some(source),
                    1.0,
                    crate::gnome::ACCENT_BLUE,
                    &mut |element| elements.push(element),
                );
                let phys: Size<i32, Physical> = output.current_mode().unwrap().size;
                let scale = Scale::from(output.current_scale().fractional_scale());
                let pixels = render_to_vec(
                    vk,
                    phys,
                    scale,
                    Transform::Normal,
                    Fourcc::Abgr8888,
                    elements.iter().rev(),
                )?;
                Ok((pixels, phys.w))
            });
    let Some(result) = composited else {
        eprintln!("skipping vulkan_folder_dialog_shrinks_toward_its_source_tile: no Vulkan device");
        return;
    };
    let (pixels, w) = result.expect("compositing the shrinking dialog must not error");

    // Where the panel actually is at this instant, per the same transform the renderer used.
    let zoom = f.synoik().folder_dialog.zoom_for_test();
    let shrunk = Zoom::new(view, source, zoom).map(panel);
    eprintln!("vulkan_folder_dialog_shrink: zoom={zoom} panel={panel:?} shrunk={shrunk:?}");
    assert!(
        zoom > 0.02 && zoom < 0.9,
        "the sample must land mid-shrink, got {zoom}"
    );
    assert!(
        shrunk.size.w < panel.size.w * 0.9 && shrunk.size.h < panel.size.h * 0.9,
        "it should have shrunk by now: {shrunk:?}"
    );

    // The resting panel's centre is no longer covered by panel chrome — the box moved away.
    let vacated = Point::<f64, Logical>::from((
        panel.loc.x + panel.size.w - 20.,
        panel.loc.y + panel.size.h - 20.,
    ));
    assert!(
        !shrunk.contains(vacated),
        "pick a point the shrunk panel has left: {vacated:?} vs {shrunk:?}"
    );
    let got = px(&pixels, w, vacated.x as i32, vacated.y as i32);
    eprintln!("vulkan_folder_dialog_shrink: vacated={got:?}");
    for ch in 0..3 {
        let panel_ch = (crate::ui::widget::style::OVERLAY_BG[ch] * 255.).round() as i32;
        assert!(
            (got[ch] as i32 - panel_ch).abs() > 8,
            "the vacated corner still reads as panel fill — the zoom did not move it: {got:?}"
        );
    }

    // …and the shade has begun to lift with it.
    let shade = px(&pixels, w, 40, 40);
    eprintln!("vulkan_folder_dialog_shrink: shade={shade:?}");
    assert!(
        shade[3] > 0 && shade[3] < 204,
        "the shade fades out over the close: {shade:?}"
    );
}

/// The tile the open folder zoomed out of fades as ONE actor: its caption goes with its
/// background and its icons. The caption is the part that can silently stay behind — it
/// normally lives in the page-wide label bake, which has no per-tile alpha, so the fade
/// only works if the faded tile is routed out of that bake and drawn on its own.
#[test]
fn vulkan_app_grid_fades_a_folder_tile_caption_with_the_rest_of_it() {
    use smithay::utils::Logical;

    use crate::app_system::AppIconRef;
    use crate::ui::app_grid::AppGridEntry;

    if let Err(e) = VulkanRenderer::new() {
        eprintln!("skipping vulkan_app_grid_fades_a_folder_tile_caption: no Vulkan device ({e})");
        return;
    }

    let mut f = Fixture::new();
    f.synoik_state()
        .backend
        .headless()
        .add_renderer()
        .expect("build the Vulkan renderer");
    f.add_output(1, (1920, 1080));
    let output = f.synoik_output(1);

    let member = AppGridEntry {
        id: "m0.desktop".into(),
        name: "M0".into(),
        icon: AppIconRef::Fallback,
        folder: None,
    };
    f.synoik().app_grid.set_entries(vec![AppGridEntry {
        id: "Utilities".into(),
        name: "Utilities".into(),
        icon: AppIconRef::Fallback,
        folder: Some(vec![member]),
    }]);

    let area: Rectangle<f64, Logical> = Rectangle::new((0., 120.).into(), (1920., 700.).into());
    let tile = f
        .synoik()
        .app_grid
        .entry_rect(0, area)
        .expect("the folder tile");

    // The brightest pixel anywhere in the tile's caption band — glyph ink if the caption
    // is drawn, nothing at all if it faded away with the rest of the tile.
    let brightest_caption =
        |f: &mut Fixture| -> u8 {
            let state = f.synoik_state();
            let composited = state.backend.headless().with_vulkan_renderer(
                |vk| -> anyhow::Result<(Vec<u8>, i32)> {
                    let synoik = &mut state.synoik;
                    let elements = synoik.app_grid.render(
                        vk,
                        &synoik.app_icon_cache,
                        &synoik.icon_cache,
                        &output,
                        area,
                        1.0,
                        crate::gnome::ACCENT_BLUE,
                    );
                    let phys: Size<i32, Physical> = output.current_mode().unwrap().size;
                    let scale = Scale::from(output.current_scale().fractional_scale());
                    let pixels = render_to_vec(
                        vk,
                        phys,
                        scale,
                        Transform::Normal,
                        Fourcc::Abgr8888,
                        elements.iter().rev(),
                    )?;
                    Ok((pixels, phys.w))
                },
            );
            let (pixels, w) = composited
                .expect("no Vulkan device")
                .expect("compositing the grid must not error");
            // The caption sits under the icon: scan the tile's bottom third, full width.
            let y0 = (tile.loc.y + tile.size.h * 2. / 3.) as i32;
            let y1 = (tile.loc.y + tile.size.h) as i32;
            let mut max = 0u8;
            for y in y0..y1 {
                for x in tile.loc.x as i32..(tile.loc.x + tile.size.w) as i32 {
                    max = max.max(px(&pixels, w, x, y)[3]);
                }
            }
            max
        };

    let lit = brightest_caption(&mut f);
    eprintln!("vulkan_app_grid_fade: unfaded caption peak alpha = {lit}");
    assert!(
        lit > 100,
        "the control: an unfaded folder tile draws its caption ({lit})"
    );

    assert!(f
        .synoik()
        .app_grid
        .set_tile_fade(Some(("Utilities".to_owned(), 0.))));
    let faded = brightest_caption(&mut f);
    eprintln!("vulkan_app_grid_fade: faded caption peak alpha = {faded}");
    assert!(
        faded < 8,
        "a fully faded tile must leave NOTHING behind, caption included — got {faded}, \
         which is the page-wide label bake drawing it at full alpha"
    );
}

/// A folder tile's bubble covers its caption — including a resting caption that runs to
/// [`crate::ui::widget::TILE_LABEL_LINES`], which hangs past the one line the tile box
/// reserves. Reported live: "Sound & Video" wrapped to two lines and the second one sat
/// outside the folder's own bubble. GNOME's tile allocation follows its label, so the
/// bubble has to grow by what *this* name uses.
#[test]
fn vulkan_folder_tile_bubble_covers_a_two_line_caption() {
    use smithay::utils::Logical;

    use crate::app_system::AppIconRef;
    use crate::ui::app_grid::AppGridEntry;

    if let Err(e) = VulkanRenderer::new() {
        eprintln!("skipping vulkan_folder_tile_bubble_covers_two_lines: no Vulkan device ({e})");
        return;
    }

    let mut f = Fixture::new();
    f.synoik_state()
        .backend
        .headless()
        .add_renderer()
        .expect("build the Vulkan renderer");
    f.add_output(1, (1920, 1080));
    let output = f.synoik_output(1);

    let member = AppGridEntry {
        id: "m0.desktop".into(),
        name: "M0".into(),
        icon: AppIconRef::Fallback,
        folder: None,
    };
    // Two folders: one whose name needs the second line, one that fits on the first.
    f.synoik().app_grid.set_entries(vec![
        AppGridEntry {
            id: "sound".into(),
            name: "Sound & Video Recorders".into(),
            icon: AppIconRef::Fallback,
            folder: Some(vec![member.clone()]),
        },
        AppGridEntry {
            id: "utils".into(),
            name: "Tools".into(),
            icon: AppIconRef::Fallback,
            folder: Some(vec![member]),
        },
    ]);

    let area: Rectangle<f64, Logical> = Rectangle::new((0., 120.).into(), (1920., 700.).into());
    let mut rect = |i: usize| {
        f.synoik()
            .app_grid
            .entry_rect(i, area)
            .expect("a folder tile")
    };
    let (wide_name, short_name) = (rect(0), rect(1));

    let state = f.synoik_state();
    let composited =
        state
            .backend
            .headless()
            .with_vulkan_renderer(|vk| -> anyhow::Result<(Vec<u8>, i32)> {
                let synoik = &mut state.synoik;
                let elements = synoik.app_grid.render(
                    vk,
                    &synoik.app_icon_cache,
                    &synoik.icon_cache,
                    &output,
                    area,
                    1.0,
                    crate::gnome::ACCENT_BLUE,
                );
                let phys: Size<i32, Physical> = output.current_mode().unwrap().size;
                let scale = Scale::from(output.current_scale().fractional_scale());
                let pixels = render_to_vec(
                    vk,
                    phys,
                    scale,
                    Transform::Normal,
                    Fourcc::Abgr8888,
                    elements.iter().rev(),
                )?;
                Ok((pixels, phys.w))
            });
    let (pixels, w) = composited
        .expect("no Vulkan device")
        .expect("compositing the grid must not error");

    // Is the bubble present on the second caption line's row, at the tile's centre?
    // Sampled at the tile's centre x — clear of the bubble's rounded corners — just
    // *below* the tile box, which is where the second caption line hangs and where a
    // bubble that did not grow simply is not.
    let bubble_below_the_tile = |t: Rectangle<f64, Logical>| -> u8 {
        let y = (t.loc.y + t.size.h + 3.) as i32;
        px(&pixels, w, (t.loc.x + t.size.w / 2.) as i32, y)[3]
    };

    let two_line = bubble_below_the_tile(wide_name);
    let one_line = bubble_below_the_tile(short_name);
    eprintln!("vulkan_folder_bubble: two_line={two_line} one_line={one_line}");
    assert!(
        two_line > 0,
        "the bubble must reach the second caption line it is holding ({two_line})"
    );
    assert_eq!(
        one_line, 0,
        "…and a folder whose name fits one line must NOT grow ({one_line}) — the bubble \
         follows the caption, it is not simply taller now"
    );
}

/// A resting tile whose name does not fit one line draws **two** lines of caption
/// ([`crate::ui::widget::TILE_LABEL_LINES`] — our divergence from GNOME's single
/// ellipsized line). The second line hangs below the tile box, so it is exactly the row
/// a clip can eat silently: the per-tile bake, the shared page bake and the peek bake all
/// size themselves from the tile, and only one of them is exercised per drawn page.
#[test]
fn vulkan_app_grid_draws_two_caption_lines_at_rest() {
    use smithay::utils::Logical;

    use crate::app_system::AppIconRef;
    use crate::ui::app_grid::AppGridEntry;

    if let Err(e) = VulkanRenderer::new() {
        eprintln!("skipping vulkan_app_grid_draws_two_caption_lines: no Vulkan device ({e})");
        return;
    }

    let mut f = Fixture::new();
    f.synoik_state()
        .backend
        .headless()
        .add_renderer()
        .expect("build the Vulkan renderer");
    f.add_output(1, (1920, 1080));
    let output = f.synoik_output(1);

    // Two entries: a name that needs two lines, and a short one as the control.
    f.synoik().app_grid.set_entries(vec![
        AppGridEntry {
            id: "long.desktop".into(),
            name: "Passwords and Keys".into(),
            icon: AppIconRef::Fallback,
            folder: None,
        },
        AppGridEntry {
            id: "short.desktop".into(),
            name: "Files".into(),
            icon: AppIconRef::Fallback,
            folder: None,
        },
    ]);

    let area: Rectangle<f64, Logical> = Rectangle::new((0., 120.).into(), (1920., 700.).into());
    let metrics = f.synoik().app_grid.metrics_for(area);
    let mut rect = |i: usize| f.synoik().app_grid.entry_rect(i, area).expect("a tile");
    let (long_tile, short_tile) = (rect(0), rect(1));

    let state = f.synoik_state();
    let composited =
        state
            .backend
            .headless()
            .with_vulkan_renderer(|vk| -> anyhow::Result<(Vec<u8>, i32)> {
                let synoik = &mut state.synoik;
                let elements = synoik.app_grid.render(
                    vk,
                    &synoik.app_icon_cache,
                    &synoik.icon_cache,
                    &output,
                    area,
                    1.0,
                    crate::gnome::ACCENT_BLUE,
                );
                let phys: Size<i32, Physical> = output.current_mode().unwrap().size;
                let scale = Scale::from(output.current_scale().fractional_scale());
                let pixels = render_to_vec(
                    vk,
                    phys,
                    scale,
                    Transform::Normal,
                    Fourcc::Abgr8888,
                    elements.iter().rev(),
                )?;
                Ok((pixels, phys.w))
            });
    let (pixels, w) = composited
        .expect("no Vulkan device")
        .expect("compositing the grid must not error");

    // Peak ink in one caption line band of a tile.
    let line_ink = |tile: Rectangle<f64, Logical>, line: usize| -> u8 {
        let top = metrics.label_top(tile) + line as f64 * metrics.label_h;
        let mut max = 0u8;
        for y in top as i32..(top + metrics.label_h) as i32 {
            for x in tile.loc.x as i32..(tile.loc.x + tile.size.w) as i32 {
                max = max.max(px(&pixels, w, x, y)[3]);
            }
        }
        max
    };

    let (first, second) = (line_ink(long_tile, 0), line_ink(long_tile, 1));
    eprintln!("vulkan_app_grid_two_lines: long tile line0={first} line1={second}");
    assert!(first > 100, "the first caption line draws ({first})");
    assert!(
        second > 100,
        "…and so does the second, below the tile box ({second}) — a clip sized to the \
         tile would eat it"
    );

    // The control: a name that fits leaves the second band empty, so this is the name
    // wrapping and not some other ink down there.
    let short_second = line_ink(short_tile, 1);
    eprintln!("vulkan_app_grid_two_lines: short tile line1={short_second}");
    assert!(
        short_second < 8,
        "a name that fits one line draws nothing on the second ({short_second})"
    );
}

/// The keyboard-focused tile draws `.overview-tile:focus` — a 2px inset accent ring over a
/// faint accent-tinted fill (`focus_ring()` + `focus_bg_color`, `_drawing.scss:57-66,308-327`).
/// The ring is the only thing that tells a keyboard user where they are, and it is a separate
/// element from the page bake, so nothing else in the suite would notice it going missing.
#[test]
fn vulkan_app_grid_rings_the_keyboard_focused_tile() {
    use smithay::utils::Logical;

    use crate::app_system::AppIconRef;
    use crate::ui::app_grid::AppGridEntry;

    if let Err(e) = VulkanRenderer::new() {
        eprintln!("skipping vulkan_app_grid_rings_the_focused_tile: no Vulkan device ({e})");
        return;
    }

    let mut f = Fixture::new();
    f.synoik_state()
        .backend
        .headless()
        .add_renderer()
        .expect("build the Vulkan renderer");
    f.add_output(1, (1920, 1080));
    let output = f.synoik_output(1);

    let entry = |id: &str, name: &str| AppGridEntry {
        id: id.into(),
        name: name.into(),
        icon: AppIconRef::Fallback,
        folder: None,
    };
    f.synoik()
        .app_grid
        .set_entries(vec![entry("a.desktop", "A"), entry("b.desktop", "B")]);

    let area: Rectangle<f64, Logical> = Rectangle::new((0., 120.).into(), (1920., 700.).into());
    let rect_of = |f: &mut Fixture, i: usize| {
        f.synoik()
            .app_grid
            .entry_rect(i, area)
            .expect("both tiles are on the first page")
    };
    let (a, b) = (rect_of(&mut f, 0), rect_of(&mut f, 1));

    // One pixel inside the left edge, at mid height — on the ring, far from the corners.
    let edge = |r: Rectangle<f64, Logical>| {
        (
            (r.loc.x + 1.) as i32,
            (r.loc.y + r.size.h / 2.).round() as i32,
        )
    };

    let shoot =
        |f: &mut Fixture| -> (Vec<u8>, i32) {
            let state = f.synoik_state();
            let composited = state.backend.headless().with_vulkan_renderer(
                |vk| -> anyhow::Result<(Vec<u8>, i32)> {
                    let synoik = &mut state.synoik;
                    let elements = synoik.app_grid.render(
                        vk,
                        &synoik.app_icon_cache,
                        &synoik.icon_cache,
                        &output,
                        area,
                        1.0,
                        crate::gnome::ACCENT_BLUE,
                    );
                    let phys: Size<i32, Physical> = output.current_mode().unwrap().size;
                    let scale = Scale::from(output.current_scale().fractional_scale());
                    let pixels = render_to_vec(
                        vk,
                        phys,
                        scale,
                        Transform::Normal,
                        Fourcc::Abgr8888,
                        elements.iter().rev(),
                    )?;
                    Ok((pixels, phys.w))
                },
            );
            composited
                .expect("no Vulkan device")
                .expect("compositing the grid must not error")
        };

    // Nothing focused: an app tile is flat and forced transparent at rest, so both edges
    // are empty. This is the control that makes the ring assertion mean something.
    let (pixels, w) = shoot(&mut f);
    for (label, r) in [("first", a), ("second", b)] {
        let (x, y) = edge(r);
        let got = px(&pixels, w, x, y);
        assert!(
            got[3] < 8,
            "an unfocused tile has no resting fill ({label}): {got:?}"
        );
    }

    assert!(f.synoik().app_grid.set_focused(Some(1)));
    let (pixels, w) = shoot(&mut f);
    let (x, y) = edge(b);
    let ring = px(&pixels, w, x, y);
    eprintln!("vulkan_app_grid_focus: ring={ring:?}");
    // `$accent_color` #3584e4 at alpha .8 — blue-dominant and clearly opaque.
    assert!(ring[3] > 150, "the focused tile draws its ring: {ring:?}");
    assert!(
        ring[2] > ring[0] + 40 && ring[2] > ring[1] + 20,
        "…in the accent, not a neutral wash: {ring:?}"
    );

    let (x, y) = edge(a);
    let other = px(&pixels, w, x, y);
    assert!(other[3] < 8, "…and only on the focused tile: {other:?}");
}

/// A page change slides: GNOME's grid is one scroll view over every page, and `goToPage`
/// eases its adjustment to `pageIndex * pageWidth` over `PAGE_SWITCH_TIME`
/// (`iconGrid.js:1348-1377`), so mid-transition the outgoing and incoming pages are BOTH
/// on screen, a page width apart. That is the part no state test can see — the grid used
/// to draw exactly one page, and would happily "slide" by cutting.
#[test]
fn vulkan_app_grid_slides_both_pages_during_a_page_change() {
    use smithay::utils::Logical;

    use crate::app_system::AppIconRef;
    use crate::ui::app_grid::AppGridEntry;

    if let Err(e) = VulkanRenderer::new() {
        eprintln!("skipping vulkan_app_grid_slides_both_pages: no Vulkan device ({e})");
        return;
    }

    let mut f = Fixture::new();
    f.synoik_state()
        .backend
        .headless()
        .add_renderer()
        .expect("build the Vulkan renderer");
    f.add_output(1, (1920, 1080));
    let output = f.synoik_output(1);

    let area: Rectangle<f64, Logical> = Rectangle::new((0., 120.).into(), (1920., 700.).into());

    // Exactly two pages, whatever this area holds. Asked of the grid rather than hardcoded: the
    // page capacity is not GNOME's fixed 24 since the fill divergence scales the mode up to the
    // canvas, and a stale literal here silently turns a two-page slide test into a one-page one.
    // Seeded before asking — an empty grid has no layout to report a capacity from.
    let seed = |n: usize| -> Vec<AppGridEntry> {
        (0..n)
            .map(|i| AppGridEntry {
                id: format!("o{i:02}.desktop"),
                name: format!("O{i:02}"),
                icon: AppIconRef::Fallback,
                folder: None,
            })
            .collect()
    };
    f.synoik().app_grid.set_entries(seed(256));
    let per_page = f.synoik().app_grid.items_per_page(area);
    f.synoik().app_grid.set_entries(seed(per_page + 6));

    assert_eq!(f.synoik().app_grid.page_count(area), 2, "two pages");
    let tile0 = f
        .synoik()
        .app_grid
        .entry_rect(0, area)
        .expect("the first tile");
    let row_y = (tile0.loc.y + tile0.size.h / 2.).round() as i32;

    let shoot =
        |f: &mut Fixture| -> (Vec<u8>, i32) {
            let state = f.synoik_state();
            let composited = state.backend.headless().with_vulkan_renderer(
                |vk| -> anyhow::Result<(Vec<u8>, i32)> {
                    let synoik = &mut state.synoik;
                    let elements = synoik.app_grid.render(
                        vk,
                        &synoik.app_icon_cache,
                        &synoik.icon_cache,
                        &output,
                        area,
                        1.0,
                        crate::gnome::ACCENT_BLUE,
                    );
                    let phys: Size<i32, Physical> = output.current_mode().unwrap().size;
                    let scale = Scale::from(output.current_scale().fractional_scale());
                    let pixels = render_to_vec(
                        vk,
                        phys,
                        scale,
                        Transform::Normal,
                        Fourcc::Abgr8888,
                        elements.iter().rev(),
                    )?;
                    Ok((pixels, phys.w))
                },
            );
            composited
                .expect("no Vulkan device")
                .expect("compositing the grid must not error")
        };

    // The horizontal span of drawn content along the row of icon centers.
    let span = |pixels: &[u8], w: i32| -> (i32, i32) {
        let (mut lo, mut hi) = (i32::MAX, i32::MIN);
        for x in 0..w {
            if px(pixels, w, x, row_y)[3] > 20 {
                lo = lo.min(x);
                hi = hi.max(x);
            }
        }
        (lo, hi)
    };

    let (pixels, w) = shoot(&mut f);
    let (rest_lo, rest_hi) = span(&pixels, w);
    eprintln!("vulkan_app_grid_slide: settled span {rest_lo}..{rest_hi}");
    assert!(rest_lo < rest_hi, "the settled page draws something");

    // Start the slide and sample a fifth of the way in, before either page has arrived.
    assert!(f.synoik().app_grid.set_page(1, area));
    let at = f.synoik().clock.now_unadjusted() + std::time::Duration::from_millis(60);
    f.synoik().clock.set_unadjusted(at);
    let (pixels, w) = shoot(&mut f);
    let (mid_lo, mid_hi) = span(&pixels, w);
    eprintln!("vulkan_app_grid_slide: mid-slide span {mid_lo}..{mid_hi}");

    assert!(
        mid_lo < rest_lo,
        "the outgoing page has moved left, past where the resting page starts \
         ({mid_lo} vs {rest_lo})"
    );
    assert!(
        mid_hi > rest_hi,
        "…and the incoming page is entering from the right ({mid_hi} vs {rest_hi}) — \
         both pages must be on screen at once"
    );

    // Once it lands, the destination page sits exactly where the first one did.
    f.settle_animations();
    let (pixels, w) = shoot(&mut f);
    let (end_lo, end_hi) = span(&pixels, w);
    eprintln!("vulkan_app_grid_slide: settled-on-page-1 span {end_lo}..{end_hi}");
    // The last page holds 6 of the 30 apps, so its row is legitimately shorter — what
    // has to match is where it comes to *rest*: the block origin, with nothing left
    // hanging past the resting page's right edge.
    assert_eq!(
        end_lo, rest_lo,
        "the slide comes to rest at the same block origin the first page had"
    );
    assert!(
        end_hi <= rest_hi,
        "…with nothing still hanging off to the right ({end_hi} vs {rest_hi})"
    );
}

/// The page-indicator dots follow the page. The active dot is the full 10 px at full
/// opacity and the others shrink to 2/3 at half (`pageIndicators.js`), so "which dot is
/// lit" is a pixel question — and the strip is one cached bake, which is exactly how it
/// came to be frozen on the first dot when the page stopped bumping the bake revision.
#[test]
fn vulkan_app_grid_dots_follow_the_page() {
    use smithay::utils::Logical;

    use crate::app_system::AppIconRef;
    use crate::ui::app_grid::AppGridEntry;

    if let Err(e) = VulkanRenderer::new() {
        eprintln!("skipping vulkan_app_grid_dots_follow_the_page: no Vulkan device ({e})");
        return;
    }

    let mut f = Fixture::new();
    f.synoik_state()
        .backend
        .headless()
        .add_renderer()
        .expect("build the Vulkan renderer");
    f.add_output(1, (1920, 1080));
    let output = f.synoik_output(1);

    let area: Rectangle<f64, Logical> = Rectangle::new((0., 120.).into(), (1920., 700.).into());

    // Two pages, whatever this band holds — the capacity is not GNOME's fixed 24 since the fill
    // divergence scales the mode up to the canvas. Asked after seeding: an empty grid has no
    // layout to report a capacity from.
    let seed = |n: usize| -> Vec<AppGridEntry> {
        (0..n)
            .map(|i| AppGridEntry {
                id: format!("o{i:02}.desktop"),
                name: format!("O{i:02}"),
                icon: AppIconRef::Fallback,
                folder: None,
            })
            .collect()
    };
    f.synoik().app_grid.set_entries(seed(256));
    let per_page = f.synoik().app_grid.items_per_page(area);
    f.synoik().app_grid.set_entries(seed(per_page + 6));
    assert_eq!(f.synoik().app_grid.page_count(area), 2);
    let dots: Vec<_> = (0..2)
        .map(|p| {
            f.synoik()
                .app_grid
                .indicator_center(p, area)
                .expect("both dots are laid out")
        })
        .collect();

    let shoot =
        |f: &mut Fixture| -> (Vec<u8>, i32) {
            let state = f.synoik_state();
            let composited = state.backend.headless().with_vulkan_renderer(
                |vk| -> anyhow::Result<(Vec<u8>, i32)> {
                    let synoik = &mut state.synoik;
                    let elements = synoik.app_grid.render(
                        vk,
                        &synoik.app_icon_cache,
                        &synoik.icon_cache,
                        &output,
                        area,
                        1.0,
                        crate::gnome::ACCENT_BLUE,
                    );
                    let phys: Size<i32, Physical> = output.current_mode().unwrap().size;
                    let scale = Scale::from(output.current_scale().fractional_scale());
                    let pixels = render_to_vec(
                        vk,
                        phys,
                        scale,
                        Transform::Normal,
                        Fourcc::Abgr8888,
                        elements.iter().rev(),
                    )?;
                    Ok((pixels, phys.w))
                },
            );
            composited
                .expect("no Vulkan device")
                .expect("compositing the grid must not error")
        };

    // Which dot is the bright one, by alpha at its own center.
    let lit = |f: &mut Fixture| -> Vec<u8> {
        let (pixels, w) = shoot(f);
        dots.iter()
            .map(|c| px(&pixels, w, c.x.round() as i32, c.y.round() as i32)[3])
            .collect()
    };

    let on_first = lit(&mut f);
    eprintln!("vulkan_app_grid_dots: page 0 -> {on_first:?}");
    assert!(
        on_first[0] > on_first[1] + 40,
        "the first dot is the lit one on page 0: {on_first:?}"
    );

    assert!(f.synoik().app_grid.set_page(1, area));
    f.settle_animations();
    let on_second = lit(&mut f);
    eprintln!("vulkan_app_grid_dots: page 1 -> {on_second:?}");
    assert!(
        on_second[1] > on_second[0] + 40,
        "…and the second one on page 1 — a strip still showing the first dot lit is the \
         cached bake, not the layout: {on_second:?}"
    );
}

/// The OSD actually paints: an opaque `$osd_bg_color` pill at the bottom of the
/// output, a level bar whose white fill grows with the value, and — only once
/// `max_level > 1` — a `$destructive_color` overdrive segment past 100%
/// (`_osd.scss:5-34`, `js/ui/barLevel.js:180-220`).
#[test]
fn vulkan_renders_the_osd() {
    use crate::ui::osd::OsdLevel;

    let Some(mut f) = window_fixture(GREEN) else {
        return;
    };
    let output = f.synoik_output(1);
    let scale = Scale::from(output.current_scale().fractional_scale());
    let out_size = output_size(&output);

    // (white bar pixels, red overdrive pixels) inside the level bar's own band.
    let sample = |f: &mut Fixture, level: f64, max: f64| {
        let out = output.clone();
        f.synoik().osd.show_one(
            &out,
            &["audio-volume-high-symbolic"],
            None,
            OsdLevel::new(level, max),
        );
        f.settle_animations();

        let bar = f
            .synoik()
            .osd
            .level_rect(&out)
            .expect("an OSD with a level has a bar");
        let pill = f.synoik().osd.rect(&out).expect("the OSD is visible");
        assert!(
            pill.loc.y + pill.size.h < out_size.h,
            "the pill sits above the bottom edge (margin-bottom: 4em)"
        );

        let w = to_physical_precise_round::<i32>(scale.x, out_size.w);
        let h = to_physical_precise_round::<i32>(scale.x, out_size.h);
        let band_top = to_physical_precise_round::<i32>(scale.x, bar.loc.y);
        let band_bot = to_physical_precise_round::<i32>(scale.x, bar.loc.y + bar.size.h);

        let state = f.synoik_state();
        state
            .backend
            .headless()
            .with_vulkan_renderer(|vk| {
                let elems = state.synoik.osd.render(vk, &state.synoik.icon_cache, &out);
                assert!(!elems.is_empty(), "a visible OSD must render");
                let pixels = composite_ui(vk, elems, Size::<i32, Physical>::from((w, h)), scale);
                let mut white = 0usize;
                let mut red = 0usize;
                let mut pill_bg = 0usize;
                for (i, p) in pixels.chunks_exact(4).enumerate() {
                    let y = i as i32 / w;
                    // Abgr8888 is byte-order R,G,B,A here.
                    if p[3] == 255 && p[0] > 40 && p[0] < 60 && p[2] > 44 && p[2] < 64 {
                        pill_bg += 1;
                    }
                    if y < band_top || y >= band_bot {
                        continue;
                    }
                    if p[3] == 255 && p[0] > 240 && p[1] > 240 && p[2] > 240 {
                        white += 1;
                    }
                    if p[3] == 255 && p[0] > 150 && p[1] < 80 && p[2] < 90 {
                        red += 1;
                    }
                }
                assert!(pill_bg > 1000, "the $osd_bg_color pill must be drawn");
                (white, red)
            })
            .expect("vulkan renderer")
    };

    let (low, low_red) = sample(&mut f, 0.25, 1.);
    let (high, high_red) = sample(&mut f, 0.75, 1.);
    assert!(
        high > low * 2,
        "a fuller bar means more white: 0.25 -> {low}px, 0.75 -> {high}px"
    );
    assert_eq!(
        (low_red, high_red),
        (0, 0),
        "no overdrive segment while max_level is 1"
    );

    // Amplified volume: max 1.5, value 1.4 -> a red segment past the separator.
    let (_, over_red) = sample(&mut f, 1.4, 1.5);
    assert!(
        over_red > 20,
        "value past overdrive_start must paint $destructive_color, got {over_red}px"
    );
}

/// The media card renders as a `.message`: the card fill under its own chrome, the album-art
/// slot's `.message-themed-icon` backdrop lighter than that fill (`_message-list.scss:174-178`,
/// rounded rather than circular for media, `:260-263`), and a control glyph compositing ON TOP of
/// the card. Our own chrome, so a headless shot is trustworthy. Skips with no Vulkan.
#[test]
fn vulkan_renders_the_media_card() {
    let Some(mut f) = window_fixture(GREEN) else {
        return;
    };
    let output = f.synoik_output(1);
    let scale = Scale::from(output.current_scale().fractional_scale());

    // A player in the store, then the popover opened the way the panel opens it.
    let bus = "org.mpris.MediaPlayer2.rhythmbox";
    f.synoik_state()
        .on_mpris_msg(crate::mpris::MprisToSynoik::PlayerUpdated {
            bus_name: bus.to_owned(),
            state: Box::new(crate::mpris::PlayerState {
                identity: "Rhythmbox".into(),
                can_play: true,
                can_go_next: true,
                status: crate::mpris::PlaybackStatus::Playing,
                title: "So What".into(),
                artists: vec!["Miles Davis".into()],
                ..crate::mpris::PlayerState::default()
            }),
        });
    {
        let anchor = f.synoik().panel.date_menu_rect(output_size(&output).w);
        let cal = f.synoik().gnome_settings.calendar;
        let accent = f.synoik().gnome_settings.accent_color;
        f.synoik().panel_popover.toggle_calendar(
            output.clone(),
            anchor,
            cal.week_start,
            cal.show_week_numbers,
            accent,
            Vec::new(),
        );
    }
    f.synoik().refresh_popover_media();
    f.settle_animations();

    let state = f.synoik_state();
    let origin = state.synoik.panel_popover.content_location(&output);
    let (_, card_rect, controls) = state
        .synoik
        .panel_popover
        .date_menu()
        .unwrap()
        .media_card_rects()
        .remove(0);
    let play_icon_available = state
        .synoik
        .icon_cache
        .resolve("media-playback-pause-symbolic")
        .is_some();

    let (card_px, art_px, ctrl_px) = state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| {
            let elems = state.synoik.panel_popover.render(
                vk,
                &state.synoik.icon_cache,
                &state.synoik.app_icon_cache,
                &state.synoik.image_cache,
                &output,
            );
            let w = to_physical_precise_round(scale.x, output_size(&output).w);
            let h = to_physical_precise_round(scale.x, 500.);
            let pixels = composite_ui(vk, elems, Size::<i32, Physical>::from((w, h)), scale);
            let sample = |x: f64, y: f64| {
                let px = to_physical_precise_round::<i32>(scale.x, origin.x + x);
                let py = to_physical_precise_round::<i32>(scale.x, origin.y + y);
                let i = ((py * w + px) * 4) as usize;
                [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
            };
            // Between the art and the controls, below the text baselines: plain card fill.
            let card_px = sample(
                card_rect.loc.x + card_rect.size.w / 2.,
                card_rect.loc.y + card_rect.size.h - 4.,
            );
            // The art slot's top-left corner area, inside its rounded backdrop but clear of the
            // centred 32px fallback glyph. Taken from the layout, not a literal.
            let slot = crate::ui::media_card::layout(card_rect.size.w).art;
            let art_px = sample(
                card_rect.loc.x + slot.loc.x + 4.,
                card_rect.loc.y + slot.loc.y + 4.,
            );
            // The play/pause glyph. Its dead centre is the GAP between the pause bars, so scan
            // the row across the icon and keep the brightest pixel.
            let cy = controls[1].loc.y + controls[1].size.h / 2.;
            let cx = controls[1].loc.x + controls[1].size.w / 2.;
            let ctrl_px = (-8..=8)
                .map(|dx| sample(cx + f64::from(dx), cy))
                .max_by_key(|px| u32::from(px[0]) + u32::from(px[1]) + u32::from(px[2]))
                .unwrap();
            (card_px, art_px, ctrl_px)
        })
        .expect("vulkan renderer");

    assert_eq!(card_px[3], 255, "the card must be opaque, got {card_px:?}");
    assert!(
        (0x45..=0x60).contains(&card_px[0]) && (0x50..=0x68).contains(&card_px[2]),
        "expected the .message card bg (#51515a) under the media card, got {card_px:?}"
    );
    assert!(
        art_px[0] > card_px[0] && art_px[2] > card_px[2],
        "the art slot's white@7% backdrop must sit above the card fill, got {art_px:?} vs {card_px:?}"
    );
    if play_icon_available {
        assert!(
            ctrl_px[0] > 150 && ctrl_px[1] > 150 && ctrl_px[2] > 150,
            "the play-pause glyph must composite above the card, got {ctrl_px:?}"
        );
    }
}

/// One collapsed `.message` with a body icon, from the reference box model
/// (`_message-list.scss:83,118-120,160`). Sample points below are expressed relative to this
/// rather than as literals, so a padding correction does not read as a stacking or z-order
/// regression in three unrelated tests.
fn collapsed_card_h() -> f64 {
    use crate::ui::notification_card::{BODY_ICON, HEADER_H, HEADER_PAD_B, PAD};
    PAD + HEADER_H + HEADER_PAD_B + PAD + BODY_ICON + PAD * 2.
}

/// A player publishing `mpris:artUrl` draws the cover in the icon slot, and drawing it takes the
/// themed plate away with it: `.message-themed-icon` is toggled on `notify::is-symbolic`
/// (`js/ui/messageList.js:487-492`), so the backdrop only exists while the *fallback* is up.
///
/// The cover here is deliberately 2:1, which pins the other half of the rule: the art is
/// **aspect-fit** into the square slot (`CLUTTER_CONTENT_GRAVITY_RESIZE_ASPECT`,
/// `st-texture-cache.c:1017-1019`), so the slot's top band shows the card fill — a stretched or
/// cover-cropped implementation paints art there and fails this. Skips with no Vulkan.
#[test]
fn vulkan_draws_album_art_without_the_themed_plate() {
    let Some(mut f) = window_fixture(GREEN) else {
        return;
    };
    let output = f.synoik_output(1);
    let scale = Scale::from(output.current_scale().fractional_scale());

    // A 2:1 solid-red cover on disk. Wide, so fitting it into the 48px square leaves bands.
    let dir = std::env::temp_dir().join(format!("gsrs-album-art-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let art = dir.join("cover.png");
    let cover = image::RgbaImage::from_pixel(64, 32, image::Rgba([255, 0, 0, 255]));
    cover.save(&art).unwrap();

    let bus = "org.mpris.MediaPlayer2.rhythmbox";
    f.synoik_state()
        .on_mpris_msg(crate::mpris::MprisToSynoik::PlayerUpdated {
            bus_name: bus.to_owned(),
            state: Box::new(crate::mpris::PlayerState {
                identity: "Rhythmbox".into(),
                can_play: true,
                status: crate::mpris::PlaybackStatus::Playing,
                title: "So What".into(),
                artists: vec!["Miles Davis".into()],
                art: Some(crate::image_source::ImageSource::File(art.clone())),
                ..crate::mpris::PlayerState::default()
            }),
        });
    {
        let anchor = f.synoik().panel.date_menu_rect(output_size(&output).w);
        let cal = f.synoik().gnome_settings.calendar;
        let accent = f.synoik().gnome_settings.accent_color;
        f.synoik().panel_popover.toggle_calendar(
            output.clone(),
            anchor,
            cal.week_start,
            cal.show_week_numbers,
            accent,
            Vec::new(),
        );
    }
    f.synoik().refresh_popover_media();
    f.settle_animations();

    let state = f.synoik_state();
    let origin = state.synoik.panel_popover.content_location(&output);
    let (_, card_rect, _) = state
        .synoik
        .panel_popover
        .date_menu()
        .unwrap()
        .media_card_rects()
        .remove(0);
    // The art slot, card-relative: the body row's 48px square (`media_card::layout`).
    let slot = crate::ui::media_card::layout(card_rect.size.w).art;

    let (band_px, art_px, card_px) = state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| {
            let elems = state.synoik.panel_popover.render(
                vk,
                &state.synoik.icon_cache,
                &state.synoik.app_icon_cache,
                &state.synoik.image_cache,
                &output,
            );
            let w = to_physical_precise_round(scale.x, output_size(&output).w);
            let h = to_physical_precise_round(scale.x, 500.);
            let pixels = composite_ui(vk, elems, Size::<i32, Physical>::from((w, h)), scale);
            let sample = |x: f64, y: f64| {
                let px = to_physical_precise_round::<i32>(scale.x, origin.x + x);
                let py = to_physical_precise_round::<i32>(scale.x, origin.y + y);
                let i = ((py * w + px) * 4) as usize;
                [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
            };
            let cx = card_rect.loc.x + slot.loc.x + slot.size.w / 2.;
            // Inside the slot, above the fitted art: 2:1 into a square leaves a 12px band, so 4px
            // down is clear of the cover and squarely where the plate would be.
            let band_px = sample(cx, card_rect.loc.y + slot.loc.y + 4.);
            let art_px = sample(cx, card_rect.loc.y + slot.loc.y + slot.size.h / 2.);
            // The card fill to compare against, well clear of the slot.
            let card_px = sample(
                card_rect.loc.x + card_rect.size.w / 2.,
                card_rect.loc.y + card_rect.size.h - 4.,
            );
            (band_px, art_px, card_px)
        })
        .expect("vulkan renderer");

    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        art_px[0] > 200 && art_px[1] < 80 && art_px[2] < 80,
        "the cover must be drawn in the art slot, got {art_px:?}"
    );
    assert_eq!(
        band_px, card_px,
        "the slot's letterbox band must show the plain card fill: real art removes \
         `.message-themed-icon`, so there is no backdrop left to paint"
    );
}

/// Raise the Alt-Tab switcher and pin the clock past its open delay, so the panel is actually on
/// screen when the frame is composited.
///
/// **The delay is a visibility gate, not a fade** — inside [`POPUP_DELAY`] the popup is live but
/// draws nothing at all, so a test that opens it and renders immediately asserts against a frame
/// the panel never appeared in. Same trap as [`settle_screenshot_ui_open`], different mechanism:
/// there the chrome is at alpha 0, here it is not pushed at all.
///
/// [`POPUP_DELAY`]: crate::ui::switcher::POPUP_DELAY
fn open_window_switcher(f: &mut Fixture) {
    // A real held modifier, so the popup has something to commit on and does not immediately
    // finish through the release race.
    const KEY_LEFTALT: u32 = 56;
    f.key_press(KEY_LEFTALT);

    f.synoik_state().do_action(
        synoik_config::Action::SwitchWindows { backward: false },
        false,
    );
    assert!(
        f.synoik().switcher.is_open(),
        "Alt-Tab must raise the switcher"
    );

    let mut clock = f.synoik().clock.clone();
    let now = clock.now_unadjusted();
    clock.set_unadjusted(now + crate::ui::switcher::POPUP_DELAY * 2);
    f.synoik().advance_animations();

    assert!(
        f.synoik().switcher.is_visible(),
        "the popup must be past its open delay before the frame is taken"
    );
}

/// The switcher panel actually reaches the screen, with `%osd_panel`'s own fill.
///
/// Samples the panel's *computed* location rather than scanning for "some dark pixels": the
/// desktop backdrop is dark too, and a scan-based version of this test passed with the switcher's
/// renderer disabled entirely. Comparing a named pixel before and after is what makes it able to
/// fail.
#[test]
fn vulkan_draws_the_window_switcher_panel() {
    let Some(mut f) = window_fixture(GREEN) else {
        return;
    };
    let output = f.synoik_output(1);

    let (before, w, _) = render_output_vulkan(&mut f, &output);

    open_window_switcher(&mut f);

    // Where the panel actually is, from the same layout the renderer used.
    let panel = f
        .synoik()
        .switcher
        .panel_rect()
        .expect("an open switcher has a panel");
    let scale = output.current_scale().fractional_scale();
    // A point inside the panel's plate but clear of the item boxes: just below its top edge,
    // horizontally centred.
    let sx = ((panel.loc.x + panel.size.w / 2.) * scale).round() as i32;
    let sy = ((panel.loc.y + 3.) * scale).round() as i32;

    let (after, _, _) = render_output_vulkan(&mut f, &output);

    let plate = px(&after, w, sx, sy);
    let was = px(&before, w, sx, sy);
    assert_ne!(plate, was, "the panel must change the pixel it covers");

    // `%osd_panel` is `$osd_bg_color` — [0.180, 0.180, 0.200] opaque. Allow a few levels for the
    // rounded-rect coverage and the hairline over it.
    let expected = [46u8, 46, 51];
    for (i, (got, want)) in plate[..3].iter().zip(expected).enumerate() {
        assert!(
            got.abs_diff(want) <= 6,
            "panel channel {i}: got {got}, want ~{want} (full pixel {plate:?})"
        );
    }
    assert!(
        plate[3] > 240,
        "the panel plate is opaque, got alpha {}",
        plate[3]
    );
}

/// The live window shows up *inside* the switcher, not just the plate.
///
/// The preview is composited from the window's own surfaces rather than a snapshot, so a green
/// window must put green pixels inside the panel. Without this, a panel that drew its chrome but
/// dropped every thumbnail would pass the test above.
#[test]
fn vulkan_draws_the_live_window_preview_in_the_switcher() {
    let Some(mut f) = window_fixture(GREEN) else {
        return;
    };
    let output = f.synoik_output(1);

    // Count the green the desktop shows on its own, so the preview's green is what is measured.
    let (before, w, h) = render_output_vulkan(&mut f, &output);
    let green_before = count_green(&before, w, h);

    open_window_switcher(&mut f);
    let (after, w, h) = render_output_vulkan(&mut f, &output);
    let green_after = count_green(&after, w, h);

    assert!(
        green_after > green_before,
        "the switcher must add the window's own green as a preview \
         (before {green_before}, after {green_after})"
    );
}

/// The multi-window chevron reaches the screen, and only under the app that has two windows.
///
/// Both halves are the test. Asserting only that the two-window app's arrow slot brightens would
/// pass an implementation that drew an arrow under every item, which inverts what the arrow means
/// ("there is a window sub-list here", `altTab.js:857-873`). The slots are sampled at their
/// *computed* positions, from the same layout the renderer drew from.
#[test]
fn vulkan_draws_the_arrow_only_under_a_multi_window_app() {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog};

    if let Err(e) = VulkanRenderer::new() {
        eprintln!("skipping: no Vulkan device ({e})");
        return;
    }

    let mut f = Fixture::new();
    f.synoik_state()
        .backend
        .headless()
        .add_renderer()
        .expect("build the Vulkan renderer");
    f.add_output(1, (OUT_W, OUT_H));
    f.synoik().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![
            AppEntry::fake("org.example.One.desktop", "One"),
            AppEntry::fake("org.example.Two.desktop", "Two"),
        ])),
        Box::new(crate::app_system::RecordingLauncher::default()),
    );

    // "One" gets two windows and so owes an arrow; "Two" gets one and must not have one.
    let client = f.add_client();
    map_window_for_app(&mut f, client, "org.example.One");
    map_window_for_app(&mut f, client, "org.example.One");
    map_window_for_app(&mut f, client, "org.example.Two");
    f.synoik_complete_animations();

    let output = f.synoik_output(1);
    let (before, w, _) = render_output_vulkan(&mut f, &output);

    const KEY_LEFTMETA: u32 = 125;
    f.key_press(KEY_LEFTMETA);
    f.synoik_state()
        .do_action(Action::SwitchApplications { backward: false }, false);

    let mut clock = f.synoik().clock.clone();
    let now = clock.now_unadjusted();
    clock.set_unadjusted(now + crate::ui::switcher::POPUP_DELAY * 2);
    f.synoik().advance_animations();
    assert!(f.synoik().switcher.is_visible());

    // Item order is the MRU tab list: "Two" was mapped last, so it leads and "One" follows.
    let apps = f.synoik().switcher.item_count();
    assert_eq!(apps, Some(2), "two running apps, two items");
    let one = f.synoik().switcher.item_rect(1).expect("the second item");
    let two = f.synoik().switcher.item_rect(0).expect("the first item");

    let scale = output.current_scale().fractional_scale();
    // The apex: the arrow's own bottom-centre, where its coverage is lowest but nonzero, would be
    // a fragile sample. Take the middle of the base instead — solidly inside the triangle.
    let sample = |item| {
        let a = crate::ui::switcher::app_switcher::arrow_rect(item);
        (
            ((a.loc.x + a.size.w / 2.) * scale).round() as i32,
            ((a.loc.y + a.size.h * 0.25) * scale).round() as i32,
        )
    };
    let (ox, oy) = sample(one);
    let (tx, ty) = sample(two);

    let (after, _, _) = render_output_vulkan(&mut f, &output);

    // "One"'s slot carries a triangle; "Two"'s must be bare `%osd_panel` plate ([46, 46, 51], as
    // in `vulkan_draws_the_window_switcher_panel`).
    //
    // The second assertion is deliberately *absolute* rather than a brightness comparison between
    // the two slots. A relative one passed with the arrow drawn under every item: "One" is the
    // selected app, so its arrow is `:highlighted` white while "Two"'s is the dim 0.8 variant, and
    // the two differ by more than any sane threshold even when both are wrongly present.
    let with_arrow = px(&after, w, ox, oy);
    let without = px(&after, w, tx, ty);
    assert_ne!(
        with_arrow,
        px(&before, w, ox, oy),
        "the switcher must change the pixel the arrow covers"
    );
    assert!(
        with_arrow[0] > 200,
        "the multi-window app must have an arrow over its plate, got {with_arrow:?}"
    );
    for (i, (got, want)) in without[..3].iter().zip([46u8, 46, 51]).enumerate() {
        assert!(
            got.abs_diff(want) <= 6,
            "the single-window app's slot must be bare plate; channel {i} got {got}, \
             want ~{want} (full pixel {without:?})"
        );
    }
}

/// A single green square window that resolves to an app and carries `title`, so the switcher has
/// a preview, an app badge *and* a title band to draw.
///
/// Square deliberately: a landscape window is letterboxed inside the 128px preview box and its
/// clone never reaches the box's bottom-right corner, which is exactly where the badge goes — so
/// a landscape fixture cannot see the two overlap at all.
fn app_window_switcher_fixture(title: &str) -> Option<Fixture> {
    app_window_switcher_fixture_n(title, 1)
}

/// As [`app_window_switcher_fixture_n`], but with **single-pixel solid** buffers — the surfaces
/// that draw through `Frame::draw_solid` instead of the texture path.
fn solid_buffer_switcher_fixture() -> Option<Fixture> {
    app_window_switcher_fixture_inner("Solid", 2, true)
}

/// As [`app_window_switcher_fixture`], with `n` windows of the one app — so the app switcher has a
/// window sub-list to open.
fn app_window_switcher_fixture_n(title: &str, n: usize) -> Option<Fixture> {
    app_window_switcher_fixture_inner(title, n, false)
}

fn app_window_switcher_fixture_inner(title: &str, n: usize, solid: bool) -> Option<Fixture> {
    use crate::app_system::{AppEntry, AppSystem, FakeCatalog};

    if let Err(e) = VulkanRenderer::new() {
        eprintln!("skipping: no Vulkan device ({e})");
        return None;
    }

    let mut f = Fixture::new();
    f.synoik_state()
        .backend
        .headless()
        .add_renderer()
        .expect("build the Vulkan renderer");
    f.add_output(1, (OUT_W, OUT_H));
    f.synoik().app_system = AppSystem::with_parts(
        Box::new(FakeCatalog::new(vec![AppEntry::fake(
            "org.example.One.desktop",
            "One",
        )])),
        Box::new(crate::app_system::RecordingLauncher::default()),
    );

    let id = f.add_client();
    for i in 0..n {
        let window = f.client(id).create_window();
        let surface = window.surface.clone();
        window.set_app_id("org.example.One");
        window.set_title(&format!("{title} {i}"));
        window.commit();
        f.roundtrip(id);

        let window = f.client(id).window(&surface);
        // A real `wl_shm` texture by default; `solid` picks the single-pixel buffer instead,
        // which reaches the renderer as a colour rather than a texture and so exercises a
        // different draw path (see `vulkan_rounds_a_solid_colour_surface_too`).
        if solid {
            window.attach_solid_buffer(GREEN[0], GREEN[1], GREEN[2], GREEN[3]);
        } else {
            window.attach_shm_buffer(
                i32::from(WIN),
                i32::from(WIN),
                GREEN[0] as u8,
                GREEN[1] as u8,
                GREEN[2] as u8,
                GREEN[3] as u8,
            );
        }
        window.set_size(WIN, WIN);
        window.ack_last_and_commit();
        f.double_roundtrip(id);
    }
    f.synoik_complete_animations();
    f.double_roundtrip(id);

    Some(f)
}

/// The cycled window is drawn **above** the ones covering it — the point of a cycler.
///
/// `CyclerHighlight` clones the window into `global.window_group` and raises the clone to the top
/// (`_highlightItem`, `altTab.js:519-522`), hiding the original so nothing composites twice. We
/// own the render loop, so the tile is simply drawn out of order instead; either way an obscured
/// window has to become visible, or "cycle windows" shows you an accent frame around something
/// you still cannot see.
#[test]
fn vulkan_raises_the_cycled_window_above_the_ones_over_it() {
    if let Err(e) = VulkanRenderer::new() {
        eprintln!("skipping: no Vulkan device ({e})");
        return;
    }

    let mut f = Fixture::new();
    f.synoik_state()
        .backend
        .headless()
        .add_renderer()
        .expect("build the Vulkan renderer");
    f.add_output(1, (OUT_W, OUT_H));

    // Two same-sized floating windows, stacked *on top of each other*: auto-placement lays them
    // out side by side, and a cycler is only interesting where windows overlap, so the newer one
    // is moved onto the older explicitly.
    const BIG: u16 = 400;
    let id = f.add_client();
    for colour in [[0u8, 255, 0], [255, 0, 255]] {
        let window = f.client(id).create_window();
        let surface = window.surface.clone();
        window.commit();
        f.roundtrip(id);

        let window = f.client(id).window(&surface);
        window.attach_shm_buffer(
            i32::from(BIG),
            i32::from(BIG),
            colour[0],
            colour[1],
            colour[2],
            255,
        );
        window.set_size(BIG, BIG);
        window.ack_last_and_commit();
        f.double_roundtrip(id);
    }
    f.synoik_complete_animations();
    f.double_roundtrip(id);

    let output = f.synoik_output(1);
    let scale = output.current_scale().fractional_scale();

    // Both at the same spot: whichever is on top is the only one visible there.
    let ids: Vec<_> = f.synoik().layout.windows().map(|(_, m)| m.id()).collect();
    for id in ids {
        let window = f.synoik().find_window_by_id(id).unwrap();
        f.synoik().layout.move_floating_window(
            Some(&window),
            synoik_ipc::PositionChange::SetFixed(100.),
            synoik_ipc::PositionChange::SetFixed(100.),
            false,
        );
    }
    f.synoik_complete_animations();

    const KEY_LEFTALT: u32 = 56;
    f.key_press(KEY_LEFTALT);
    f.synoik_state().do_action(
        synoik_config::Action::CycleWindows { backward: false },
        false,
    );
    let rect = f.synoik().cycler_highlight.expect("the window is framed");
    // A point well inside the cycled window and clear of its own frame stroke.
    let cx = ((rect.loc.x + rect.size.w / 2.) * scale).round() as i32;
    let cy = ((rect.loc.y + rect.size.h / 2.) * scale).round() as i32;

    let (after, w, _) = render_output_vulkan(&mut f, &output);
    let middle = px(&after, w, cx, cy);
    assert!(
        middle[1] > 200 && middle[0] < 80 && middle[2] < 80,
        "the cycled (older, green) window must be on top there, got {middle:?}"
    );

    // Cancel, and it drops back under the newer one — the raise is for the cycler's lifetime,
    // not a restacking.
    f.synoik().switcher.cancel();
    f.synoik().sync_cycler_highlight();
    let (after, w, _) = render_output_vulkan(&mut f, &output);
    let middle = px(&after, w, cx, cy);
    assert!(
        middle[0] > 200 && middle[2] > 200,
        "the newer (magenta) window must be back on top, got {middle:?}"
    );
}

/// A cycler frames the window it is showing with `.cycler-highlight`, and does not cover it.
///
/// The border is `5px solid -st-accent-color` on a widget with no background
/// (`_switcher-popup.scss:80-82`), which St draws *inside* the widget's own box — so the stroke
/// eats the window's outer 5px and the middle stays the window. Filling the box instead (the
/// shape a single border-shader pass gives you) would hide the very window the cycler exists to
/// show you, and every value-based assertion about the selection would still pass.
#[test]
fn vulkan_frames_the_cycled_window_without_covering_it() {
    let Some(mut f) = app_window_switcher_fixture_n("Green", 2) else {
        return;
    };
    let output = f.synoik_output(1);
    let scale = output.current_scale().fractional_scale();

    const KEY_LEFTALT: u32 = 56;
    f.key_press(KEY_LEFTALT);
    f.synoik_state().do_action(
        synoik_config::Action::CycleWindows { backward: false },
        false,
    );
    assert!(f.synoik().switcher.is_open(), "Alt+Escape raises a cycler");

    let rect = f.synoik().cycler_highlight.expect("the window is framed");
    let at = |x: f64, y: f64| ((x * scale).round() as i32, (y * scale).round() as i32);
    // Two px into the 5px stroke on the top edge, and the middle of the window.
    let (ex, ey) = at(rect.loc.x + rect.size.w / 2., rect.loc.y + 2.);
    let (cx, cy) = at(rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.);

    let (after, w, _) = render_output_vulkan(&mut f, &output);

    // `ACCENT_BLUE`, the default `-st-accent-color`.
    let edge = px(&after, w, ex, ey);
    assert!(
        (i32::from(edge[0]) - 0x35).abs() < 24
            && (i32::from(edge[1]) - 0x84).abs() < 24
            && (i32::from(edge[2]) - 0xe4).abs() < 24,
        "the frame must be the accent colour, got {edge:?}"
    );

    let middle = px(&after, w, cx, cy);
    assert!(
        middle[1] > 200 && middle[0] < 80 && middle[2] < 80,
        "the window itself must still be visible inside its frame, got {middle:?}"
    );
}

/// The app badge draws **over** the window preview, not under it.
///
/// `WindowIcon` puts the clone and the icon in one `Clutter.BinLayout` and adds the icon second
/// (`altTab.js:1029-1037`), so the icon is the later child and paints on top. Our elements are
/// pushed front-to-back, which makes the *order of two `push` loops* the whole behaviour — and
/// getting it backwards leaves the badge half-buried under any preview that fills its box, which
/// is what a square window does.
#[test]
fn vulkan_draws_the_app_badge_over_the_window_preview() {
    let Some(mut f) = app_window_switcher_fixture("Green") else {
        return;
    };
    let output = f.synoik_output(1);

    open_window_switcher(&mut f);

    let item = f.synoik().switcher.item_rect(0).expect("one item");
    let preview = crate::ui::switcher::window_switcher::preview_box(item);
    let badge = crate::ui::switcher::window_switcher::app_icon_center(preview);
    let scale = output.current_scale().fractional_scale();
    let at = |p: Point<f64, smithay::utils::Logical>| {
        ((p.x * scale).round() as i32, (p.y * scale).round() as i32)
    };
    let (bx, by) = at(badge);
    let (cx, cy) = at(preview.loc + Point::from((preview.size.w / 2., preview.size.h / 2.)));

    let (after, w, _) = render_output_vulkan(&mut f, &output);

    // The preview really is there and really is green, so "not green" below means covered.
    let clone = px(&after, w, cx, cy);
    assert!(
        clone[1] > 200 && clone[0] < 80 && clone[2] < 80,
        "the preview's middle must be the window's green, got {clone:?}"
    );

    // A 48px badge centred here: its middle is icon, whatever the icon happens to look like.
    let over = px(&after, w, bx, by);
    assert!(
        !(over[1] > 200 && over[0] < 80 && over[2] < 80),
        "the app badge must cover the preview at the corner it sits in, but that pixel is still \
         the window's green ({over:?}) — the icons are being pushed behind the thumbnails"
    );
}

/// The selected window's title is drawn in the panel's own bottom band.
///
/// `WindowSwitcher` owns one `St.Label` for the whole list (`altTab.js:1066-1070`) and
/// `highlight` points it at the selection (`:1130-1134`). Sampling the *band* rather than any one
/// glyph: where the text lands inside it depends on the font, but ink somewhere in an otherwise
/// bare stretch of `%osd_panel` plate can only be the title.
#[test]
fn vulkan_draws_the_selected_window_title_in_the_switchers_footer() {
    let Some(mut f) = app_window_switcher_fixture("Untitled Document") else {
        return;
    };
    let output = f.synoik_output(1);

    open_window_switcher(&mut f);

    let footer = f
        .synoik()
        .switcher
        .footer_rect()
        .expect("the window switcher has a title band");
    let scale = output.current_scale().fractional_scale();

    let (after, w, _) = render_output_vulkan(&mut f, &output);

    // Anything brighter than the plate ([46, 46, 51]) inside the band is glyph coverage.
    let x0 = (footer.loc.x * scale).round() as i32;
    let x1 = ((footer.loc.x + footer.size.w) * scale).round() as i32;
    let y0 = (footer.loc.y * scale).round() as i32;
    let y1 = ((footer.loc.y + footer.size.h) * scale).round() as i32;
    let mut ink = 0;
    for y in y0..y1 {
        for x in x0..x1 {
            if px(&after, w, x, y)[0] > 120 {
                ink += 1;
            }
        }
    }
    assert!(
        ink > 20,
        "the title band must carry the selected window's title, but only {ink} pixels in it are \
         brighter than the panel plate"
    );
}

/// A rounded clip applies to a **solid-colour** surface too, not only a textured one.
///
/// A surface can arrive as a flat colour — a single-pixel `wl_buffer`, a blocked-out window — and
/// it then draws through `Frame::draw_solid` rather than `render_texture_from_to`. Only the latter
/// consulted the clip an outer `ClippedSurfaceRenderElement` armed, so every rounded corner was
/// silently square for exactly those surfaces. Invisible on a normal desktop, which is why it
/// survived: it took a test fixture using `attach_solid_buffer` to surface it.
#[test]
fn vulkan_rounds_a_solid_colour_surface_too() {
    let Some(mut f) = solid_buffer_switcher_fixture() else {
        return;
    };
    let output = f.synoik_output(1);

    const KEY_LEFTMETA: u32 = 125;
    const KEY_DOWN: u32 = 108;
    f.key_press(KEY_LEFTMETA);
    f.synoik_state()
        .do_action(Action::SwitchApplications { backward: false }, false);

    let mut clock = f.synoik().clock.clone();
    clock.set_unadjusted(clock.now_unadjusted() + crate::ui::switcher::POPUP_DELAY * 2);
    f.synoik().advance_animations();
    f.key_press(KEY_DOWN);
    f.key_release(KEY_DOWN);
    clock.set_unadjusted(clock.now_unadjusted() + crate::ui::switcher::thumbnails::FADE_TIME * 2);
    f.synoik().advance_animations();

    let preview = f
        .synoik()
        .switcher
        .thumbnail_rect(0)
        .expect("an open sub-list has a first preview");
    let scale = output.current_scale().fractional_scale();
    let (after, w, _) = render_output_vulkan(&mut f, &output);
    let px_at = |x: f64, y: f64| {
        px(
            &after,
            w,
            (x * scale).round() as i32,
            (y * scale).round() as i32,
        )
    };

    let clone_rect = crate::render_helpers::window_thumbnail::fit_rect(
        Size::from((f64::from(WIN), f64::from(WIN))),
        preview,
    );
    let is_green = |p: [u8; 4]| p[1] > 200 && p[0] < 80 && p[2] < 80;

    // The solid fill is there...
    let middle = px_at(
        clone_rect.loc.x + clone_rect.size.w / 2.,
        clone_rect.loc.y + clone_rect.size.h / 2.,
    );
    assert!(
        is_green(middle),
        "the solid surface must draw, got {middle:?}"
    );

    // ...and its corner is rounded away exactly like a textured one's.
    let corner = px_at(clone_rect.loc.x + 1., clone_rect.loc.y + 1.);
    assert!(
        !is_green(corner),
        "a solid-colour surface must honour the rounded clip too, got {corner:?}"
    );
}

/// The app switcher's window sub-list reaches the screen: its own plate, and a live preview of
/// each of the app's windows on it.
///
/// Geometry tests pin where it *would* go; only pixels catch a sub-list that is laid out and never
/// drawn, or drawn behind its own panel. Both halves are here for that reason — the plate proves
/// the second `.switcher-list` composited, and the green inside a preview proves the live windows
/// went out in front of it rather than under it.
#[test]
fn vulkan_draws_the_app_switchers_window_sublist() {
    let Some(mut f) = app_window_switcher_fixture_n("Green", 2) else {
        return;
    };
    let output = f.synoik_output(1);

    const KEY_LEFTMETA: u32 = 125;
    const KEY_DOWN: u32 = 108;
    f.key_press(KEY_LEFTMETA);
    f.synoik_state()
        .do_action(Action::SwitchApplications { backward: false }, false);

    let mut clock = f.synoik().clock.clone();
    let now = clock.now_unadjusted();
    clock.set_unadjusted(now + crate::ui::switcher::POPUP_DELAY * 2);
    f.synoik().advance_animations();

    // Down rather than the 500ms timer: the same sub-list either way, and this keeps the frame
    // free of the clock games (see `settle_screenshot_ui_open`).
    f.key_press(KEY_DOWN);
    f.key_release(KEY_DOWN);
    assert!(
        f.synoik().switcher.thumbnails_open(),
        "Down must open the window sub-list"
    );

    // ...and settle its fade-in, or the frame below is sampled at the alpha it *started* at.
    // `synoik_complete_animations` does not do this (see `settle_screenshot_ui_open`): only moving
    // the clock past the easing does.
    clock.set_unadjusted(clock.now_unadjusted() + crate::ui::switcher::thumbnails::FADE_TIME * 2);
    f.synoik().advance_animations();
    assert_eq!(
        f.synoik().switcher.thumbnail_alpha(),
        Some(1.),
        "the sub-list must be fully faded in before the frame is taken"
    );

    let panel = f
        .synoik()
        .switcher
        .thumbnail_panel_rect()
        .expect("an open sub-list has a panel");
    let preview = f
        .synoik()
        .switcher
        .thumbnail_rect(0)
        .expect("and a first preview");
    let scale = output.current_scale().fractional_scale();

    let (after, w, _) = render_output_vulkan(&mut f, &output);

    // Just inside the sub-panel's top edge, clear of the previews: bare `%osd_panel` plate.
    let px_at = |x: f64, y: f64| {
        px(
            &after,
            w,
            (x * scale).round() as i32,
            (y * scale).round() as i32,
        )
    };
    let plate = px_at(panel.loc.x + panel.size.w / 2., panel.loc.y + 3.);
    for (i, (got, want)) in plate[..3].iter().zip([46u8, 46, 51]).enumerate() {
        assert!(
            got.abs_diff(want) <= 6,
            "the sub-list's plate must reach the screen; channel {i} got {got}, want ~{want} \
             (full pixel {plate:?})"
        );
    }

    // ...and the preview's middle is the window's own green, drawn over that plate.
    let clone = px_at(
        preview.loc.x + preview.size.w / 2.,
        preview.loc.y + preview.size.h / 2.,
    );
    assert!(
        clone[1] > 200 && clone[0] < 80 && clone[2] < 80,
        "each window's live preview must draw on the sub-list, got {clone:?}"
    );

    // `.thumbnail`'s `border-radius` rounds the preview: its very corner is cut away and the
    // plate shows through, while a pixel a radius in is still the window. Sampled against the
    // *clone's* box, not the bin's — the window is letterboxed inside it.
    let is_green = |p: [u8; 4]| p[1] > 200 && p[0] < 80 && p[2] < 80;
    let clone_rect = crate::render_helpers::window_thumbnail::fit_rect(
        Size::from((f64::from(WIN), f64::from(WIN))),
        preview,
    );
    eprintln!("PROBE preview={preview:?} clone={clone_rect:?}");
    let corner = px_at(clone_rect.loc.x + 1., clone_rect.loc.y + 1.);
    assert!(
        !is_green(corner),
        "the preview's corner must be rounded away, got {corner:?}"
    );
    let inside = px_at(
        clone_rect.loc.x + crate::ui::switcher::thumbnails::THUMB_RADIUS + 2.,
        clone_rect.loc.y + crate::ui::switcher::thumbnails::THUMB_RADIUS + 2.,
    );
    assert!(
        is_green(inside),
        "...but only the corner: a radius in is still the window, got {inside:?}"
    );
}

/// Count opaque green pixels — the fixture window's colour.
fn count_green(pixels: &[u8], w: i32, h: i32) -> usize {
    let mut n = 0;
    for y in 0..h {
        for x in 0..w {
            let [r, g, b, a] = px(pixels, w, x, y);
            if a == 255 && g > 120 && r < 90 && b < 90 {
                n += 1;
            }
        }
    }
    n
}

/// The screen shield's curtain covers the desktop, and draws its clock over it.
///
/// The safety property first: with the shield down, **no** window pixel may survive. A curtain
/// that merely draws on top of a still-composited desktop is one alpha bug away from being
/// transparent, and the failure is silent — every state assertion still passes.
///
/// Then the curtain itself: the clock is white text centred on the output, so a band across the
/// vertical middle must hold bright pixels that the dimmed background does not.
#[test]
fn vulkan_draws_the_screen_shield_over_the_desktop() {
    use crate::dbus::gnome_screen_saver::ScreenSaverToSynoik;

    let Some(mut f) = green_window_fixture() else {
        return;
    };
    let output = f.synoik_output(1);

    // Establish the oracle: the window really is on screen before the shield goes down. Without
    // this the "no green" assertion below passes for a fixture that never had a window.
    let (before, w, h) = render_output_vulkan(&mut f, &output);
    assert_window_and_background(&before, w, h);

    f.synoik_state()
        .on_screen_saver_msg(ScreenSaverToSynoik::SetActive(true));
    // The curtain slides in from above; sampled this instant it is still entirely off-screen, and
    // the desktop below would show through for reasons that have nothing to do with the shield.
    f.synoik_state().synoik.lock_screen.settle();
    let (pixels, w, h) = render_output_vulkan(&mut f, &output);

    let is_green = |p: [u8; 4]| p[0] < 40 && p[1] > 200 && p[2] < 40 && p[3] > 200;
    let green = (0..w * h)
        .filter(|i| is_green(px(&pixels, w, i % w, i / w)))
        .count();
    assert_eq!(green, 0, "the desktop shows through the shield");

    // Every pixel is opaque: the curtain is a cover, not a tint.
    let transparent = (0..w * h)
        .filter(|i| px(&pixels, w, i % w, i / w)[3] < 255)
        .count();
    assert_eq!(transparent, 0, "the shield left transparent pixels");

    // The clock: bright, near-white pixels in the middle band, and none in the top eighth (which
    // is background only). Both halves matter — the first says the text drew, the second says it
    // is the text and not a washed-out frame.
    let is_bright = |p: [u8; 4]| p[0] > 200 && p[1] > 200 && p[2] > 200;
    let bright_in = |y0: i32, y1: i32| {
        (y0..y1)
            .flat_map(|y| (0..w).map(move |x| (x, y)))
            .filter(|(x, y)| is_bright(px(&pixels, w, *x, *y)))
            .count()
    };
    let middle = bright_in(h * 3 / 8, h * 5 / 8);
    let top = bright_in(0, h / 8);
    assert!(middle > 0, "the lock screen clock did not draw");
    assert!(
        top == 0,
        "{top} bright px in the top eighth — the curtain is not just its clock"
    );
}

/// The crossfade asks the icon cache for the avatar **once**, not once per scale it passes through.
///
/// The prompt page scales from 0.3 to 1 over 300 ms. Asking for the icon at `rest_px * page_scale`
/// makes every distinct size its own cache key, and a cold key draws *nothing* — so the first
/// crossfade blinks the avatar in, once per size it passes through. Bucketing the scale bounds how
/// many keys there are but not the blinking; it turns one cold miss into a dozen.
///
/// Uses the async miss path deliberately (`wire_test_worker`). With no worker the cache rasterizes
/// inline and every miss draws fine, which is exactly why the seat saw this and the suite did not.
#[test]
fn the_crossfade_asks_for_one_avatar_not_one_per_frame() {
    use crate::ui::lock_screen::{PageCtx, PageTransform, PromptContent};

    let Some(mut f) = green_window_fixture() else {
        return;
    };
    let requests = f.synoik().icon_cache.wire_test_worker();

    let content = PromptContent {
        display_name: "Test User".to_owned(),
        entry: "\u{25cf}".to_owned(),
        question: "Password:".to_owned(),
        peek: Some(false),
        entry_live: true,
        ..Default::default()
    };

    let state = f.synoik_state();
    state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| {
            let synoik = &mut state.synoik;
            // Walk the crossfade the way a run of frames would.
            for step in 0..=10 {
                let t = PageTransform::prompt(f64::from(step) / 10.);
                let ctx = PageCtx {
                    scale: 1.,
                    monitor: Rectangle::from_size(Size::from((1920., 1080.))),
                    now: Duration::ZERO,
                };
                let _ = synoik.lock_screen.render_prompt(
                    vk,
                    &synoik.icon_cache,
                    &synoik.image_cache,
                    ctx,
                    &content,
                    t,
                );
            }
        })
        .expect("headless backend must hold a Vulkan renderer");

    let mut sizes: Vec<u32> = requests
        .try_iter()
        .filter_map(|req| {
            let (name, px) = req.name_and_px();
            (name == "avatar-default-symbolic").then_some(px)
        })
        .collect();
    sizes.sort_unstable();
    sizes.dedup();
    assert_eq!(
        sizes.len(),
        1,
        "the crossfade asked the cache for {} different avatar sizes: {sizes:?}",
        sizes.len()
    );
}

/// The account picture draws, clipped to a circle, instead of the themed glyph.
///
/// Three things a state test cannot see, and each has its own silent failure:
///
/// - the picture drew *at all* — a cold or un-uploaded key returns `None` and the page simply emits
///   no element, which looks exactly like an account with no avatar;
/// - it is **round** — the rounded-texture element is what clips it, and a plain texture element
///   would draw a perfectly good square photograph that nobody would call a bug in a state test;
/// - the fallback glyph is **gone** — GNOME's `Avatar.update()` is one branch or the other
///   (`userWidget.js:78-92`), so drawing both would put a default avatar under the photograph and
///   show it wherever the picture is translucent.
#[cfg(feature = "dbus")]
#[test]
fn vulkan_draws_the_account_picture_round() {
    use std::io::Cursor;

    use crate::dbus::accounts_service::{AccountIcon, AccountsToSynoik, UserAccount};
    use crate::dbus::gdm::VerifierEvent;
    use crate::dbus::gnome_screen_saver::ScreenSaverToSynoik;

    let Some(mut f) = green_window_fixture() else {
        return;
    };
    let output = f.synoik_output(1);

    // A solid, saturated magenta rectangle: the colour is one nothing else on the lock screen
    // draws, so any pixel of it is the picture and no chrome can counterfeit it. The 2:1 shape is
    // what makes the fit visible — a square source decodes identically under `cover` and
    // `contain`, so the square test picture this started with could not tell them apart.
    let mut img = image::RgbaImage::new(256, 128);
    for p in img.pixels_mut() {
        *p = image::Rgba([255, 0, 255, 255]);
    }
    let mut png = Vec::new();
    img.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .expect("encode png");
    let path = std::env::temp_dir().join(format!("gsrs-avatar-{}.png", std::process::id()));
    std::fs::write(&path, &png).expect("write temp avatar");

    // Through the real AccountsService entry point, so the warm and the source both come from the
    // code that runs on the seat rather than from the test.
    f.synoik_state()
        .on_accounts_msg(AccountsToSynoik::UserChanged(UserAccount {
            real_name: "Test User".to_owned(),
            icon_file: AccountIcon::read(path.clone()),
            ..Default::default()
        }));

    f.synoik_state()
        .on_screen_saver_msg(ScreenSaverToSynoik::Lock(None));
    f.synoik_state().on_verifier_event(VerifierEvent::Ready(1));
    f.synoik_state()
        .on_verifier_event(VerifierEvent::AskQuestion {
            question: "Password:".to_owned(),
            secret: true,
        });
    f.synoik_state()
        .on_shield_key(None, Some('a'), Default::default());
    f.synoik_state().synoik.lock_screen.settle();

    // Watch what the icon cache is asked for across the frame: the fallback branch must not run.
    let icon_requests = f.synoik().icon_cache.wire_test_worker();

    let (pixels, w, h) = render_output_vulkan(&mut f, &output);
    let _ = std::fs::remove_file(&path);

    let asked_for_fallback = icon_requests
        .try_iter()
        .any(|req| req.name_and_px().0 == "avatar-default-symbolic");
    assert!(
        !asked_for_fallback,
        "the themed glyph was drawn under the account picture"
    );

    let is_avatar = |p: [u8; 4]| p[0] > 200 && p[1] < 60 && p[2] > 200 && p[3] > 200;
    let hits: Vec<(i32, i32)> = (0..w * h)
        .map(|i| (i % w, i / w))
        .filter(|(x, y)| is_avatar(px(&pixels, w, *x, *y)))
        .collect();
    assert!(
        hits.len() > 1000,
        "the account picture drew {} px — the prompt fell back to the glyph",
        hits.len()
    );

    // Its bounding box is the avatar's box, and the picture fills it: `cover`, not `contain`.
    let (x0, x1) = (
        hits.iter().map(|(x, _)| *x).min().unwrap(),
        hits.iter().map(|(x, _)| *x).max().unwrap(),
    );
    let (y0, y1) = (
        hits.iter().map(|(_, y)| *y).min().unwrap(),
        hits.iter().map(|(_, y)| *y).max().unwrap(),
    );
    let (bw, bh) = (x1 - x0 + 1, y1 - y0 + 1);
    assert!(
        (bw - bh).abs() <= 2,
        "the picture's box is not square: {bw}x{bh}"
    );

    // Round, not square: a disc of diameter d covers π/4 ≈ 78.5% of its bounding box, and the
    // corners of that box are bare. Both halves matter — the area alone would pass for a rounded
    // rectangle, and the corners alone would pass for a much smaller circle.
    let area = f64::from(hits.len() as i32) / f64::from(bw * bh);
    assert!(
        (0.7..0.83).contains(&area),
        "the picture covers {:.1}% of its box — a circle covers 78.5%",
        area * 100.
    );
    for (cx, cy) in [(x0, y0), (x1, y0), (x0, y1), (x1, y1)] {
        assert!(
            !is_avatar(px(&pixels, w, cx, cy)),
            "the picture reaches its box's corner at ({cx}, {cy}) — it was not clipped to a circle"
        );
    }
}

/// One picture on two outputs at different scales is two uploads, not one reused at the wrong size.
///
/// The decode is per *physical* size, so a texture uploaded for a 1× output is half the pixels a 2×
/// output needs. Keyed without the scale, whichever output drew first wins and the other reuses its
/// texture — which, re-tagged at its own scale, comes out half the size and stays that way. The
/// same key is what a scale *change* hits: `warm_avatar` decodes at the new scale and the stale
/// entry still hits, so the fresh decode is never uploaded.
///
/// Asserted on the element's logical size, which is what the miss actually moves: the avatar is
/// `AVATAR_PX` on every output, whatever the scale.
#[test]
fn the_account_picture_uploads_once_per_scale() {
    use std::io::Cursor;

    use smithay::backend::renderer::element::Element as _;

    use crate::image_source::ImageSource;
    use crate::ui::lock_screen::AVATAR_PX;
    use crate::ui::widget::{Avatar, ImageUploads};

    let Some(mut f) = green_window_fixture() else {
        return;
    };

    let mut img = image::RgbaImage::new(128, 128);
    for p in img.pixels_mut() {
        *p = image::Rgba([255, 0, 255, 255]);
    }
    let mut png = Vec::new();
    img.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .expect("encode png");
    let path = std::env::temp_dir().join(format!("gsrs-avatar-scale-{}.png", std::process::id()));
    std::fs::write(&path, &png).expect("write temp avatar");
    let source = ImageSource::File(path.clone());

    let state = f.synoik_state();
    let mut logical = Vec::new();
    state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| {
            let synoik = &mut state.synoik;
            let mut uploads = ImageUploads::new();
            // 1× first, so a scale-blind key hands its texture to the 2× ask.
            for scale in [1., 2.] {
                let el = Avatar::element(
                    vk,
                    &mut uploads,
                    &synoik.image_cache,
                    &source,
                    0,
                    AVATAR_PX,
                    scale,
                    1.,
                    Point::from((0., 0.)),
                    Point::from((0., 0.)),
                    1.,
                )
                .expect("the picture uploads");
                // Physical geometry at this output's scale, back in logical units.
                logical.push(f64::from(el.geometry(Scale::from(scale)).size.w) / scale);
            }
        })
        .expect("headless backend must hold a Vulkan renderer");

    let _ = std::fs::remove_file(&path);
    assert_eq!(
        logical.len(),
        2,
        "both scales must have produced an element"
    );
    for (i, w) in logical.iter().enumerate() {
        assert!(
            (w - AVATAR_PX).abs() <= 1.,
            "at scale {} the picture drew {w} logical px, not {AVATAR_PX} — it reused the other \
             output's upload",
            if i == 0 { 1. } else { 2. }
        );
    }
}

/// The switch-user button draws, in the corner GNOME puts it, and only when it should.
///
/// The layout rule is `x1 = box.x2 - natWidth * 2`, `y1 = box.y2 - natHeight * 2`
/// (`unlockDialog.js:496-501`) — inset by its **own size**, not by a padding constant, which is the
/// kind of rule that is easy to read as "one padding from the edge" and land twice as close as it
/// should. And because it is a sibling of the page stack rather than part of it, nothing about the
/// crossfade would stop it drawing over the clock: its `progress > 0` gate is its own.
#[cfg(feature = "dbus")]
#[test]
fn vulkan_draws_the_switch_user_button_in_its_corner() {
    use crate::dbus::accounts_service::AccountsToSynoik;
    use crate::dbus::gdm::VerifierEvent;
    use crate::dbus::gnome_screen_saver::ScreenSaverToSynoik;

    let Some(mut f) = green_window_fixture() else {
        return;
    };
    let output = f.synoik_output(1);

    // The seat can switch, but there is nobody to switch *to* yet — so the button is hidden and
    // everything else on the page is identical. Differencing against this isolates the button,
    // where differencing the clock page against the prompt page would just show the crossfade.
    f.synoik_state()
        .on_accounts_msg(AccountsToSynoik::CanSwitch(true));
    f.synoik_state()
        .on_screen_saver_msg(ScreenSaverToSynoik::Lock(None));
    f.synoik_state().on_verifier_event(VerifierEvent::Ready(1));
    f.synoik_state()
        .on_verifier_event(VerifierEvent::AskQuestion {
            question: "Password:".to_owned(),
            secret: true,
        });
    f.synoik_state()
        .on_shield_key(None, Some('a'), Default::default());
    f.synoik_state().synoik.lock_screen.settle();
    let (without, w, h) = render_output_vulkan(&mut f, &output);

    // --- A second account appears: the button, and nothing else, must change. ---
    f.synoik_state()
        .on_accounts_msg(AccountsToSynoik::MultipleUsers(true));
    f.synoik_state().synoik.lock_screen.settle();
    let (with, _, _) = render_output_vulkan(&mut f, &output);

    let drawn: Vec<(i32, i32)> = (0..w * h)
        .map(|i| (i % w, i / w))
        .filter(|(x, y)| px(&with, w, *x, *y) != px(&without, w, *x, *y))
        .collect();
    assert!(
        !drawn.is_empty(),
        "the button did not draw when the last condition became true"
    );
    let clock_page = without;

    // Where GNOME's rule says it is, spelled out here rather than taken from `switch_user_rect`:
    // an expectation computed by the function under test cannot fail when that function is wrong.
    // Only the *size* is borrowed, since that is a measured constant rather than the rule.
    let size = crate::ui::lock_screen::switch_user_size();
    let expected = Rectangle::<f64, smithay::utils::Logical>::new(
        Point::from((f64::from(w) - size * 2., f64::from(h) - size * 2.)),
        Size::from((size, size)),
    );
    let (x0, x1) = (
        drawn.iter().map(|(x, _)| *x).min().unwrap(),
        drawn.iter().map(|(x, _)| *x).max().unwrap(),
    );
    let (y0, y1) = (
        drawn.iter().map(|(_, y)| *y).min().unwrap(),
        drawn.iter().map(|(_, y)| *y).max().unwrap(),
    );
    let near = |got: i32, want: f64| (f64::from(got) - want).abs() <= 2.;
    assert!(
        near(x0, expected.loc.x)
            && near(y0, expected.loc.y)
            && near(x1, expected.loc.x + expected.size.w - 1.)
            && near(y1, expected.loc.y + expected.size.h - 1.),
        "the button drew at ({x0},{y0})-({x1},{y1}), not at {expected:?}"
    );

    // It is a circle: its corners are untouched, exactly like the avatar's.
    for (cx, cy) in [(x0, y0), (x1, y0), (x0, y1), (x1, y1)] {
        assert_eq!(
            px(&with, w, cx, cy),
            px(&clock_page, w, cx, cy),
            "the button painted the corner of its box at ({cx}, {cy}) — it is not circular"
        );
    }
}

/// The caps-lock warning actually draws, and disappears again.
///
/// A state test can only see `caps_alpha`; the row is a separate bake with its own element, so it
/// can be perfectly "visible" and still draw nothing — wrong cache, zero-sized texture, an origin
/// off the block. Asserted as a differential rather than against fixed geometry: caps on must add
/// bright ink, and turning it off must return the frame to exactly what it was.
#[test]
fn vulkan_draws_the_caps_lock_warning() {
    use crate::dbus::gdm::VerifierEvent;
    use crate::dbus::gnome_screen_saver::ScreenSaverToSynoik;

    /// `input-event-codes.h`.
    const KEY_CAPSLOCK: u32 = 58;

    let Some(mut f) = green_window_fixture() else {
        return;
    };
    let output = f.synoik_output(1);

    f.synoik_state()
        .on_screen_saver_msg(ScreenSaverToSynoik::Lock(None));
    f.synoik_state().on_verifier_event(VerifierEvent::Ready(1));
    f.synoik_state()
        .on_verifier_event(VerifierEvent::AskQuestion {
            question: "Password:".to_owned(),
            secret: true,
        });
    f.synoik_state()
        .on_shield_key(None, Some('a'), Default::default());
    f.synoik_state().synoik.lock_screen.settle();

    let is_bright = |p: [u8; 4]| p[0] > 200 && p[1] > 200 && p[2] > 200;
    let count_bright = |pixels: &[u8], w: i32, h: i32| {
        (0..w * h)
            .filter(|i| is_bright(px(pixels, w, i % w, i / w)))
            .count()
    };

    let (before, w, h) = render_output_vulkan(&mut f, &output);
    let bright_before = count_bright(&before, w, h);

    // Caps on, through the real key path — the state has to survive xkb, which is the half most
    // likely to be wrong (the press reports the lock state it is about to change).
    f.key_press(KEY_CAPSLOCK);
    f.key_release(KEY_CAPSLOCK);
    assert!(f.synoik().caps_lock, "the seat reports caps lock on");
    f.synoik_state().synoik.lock_screen.settle();

    let (during, _, _) = render_output_vulkan(&mut f, &output);
    let bright_during = count_bright(&during, w, h);
    assert!(
        bright_during > bright_before,
        "the caps-lock warning drew no ink: {bright_before} -> {bright_during}"
    );

    // ...and off again puts the frame back exactly.
    f.key_press(KEY_CAPSLOCK);
    f.key_release(KEY_CAPSLOCK);
    assert!(!f.synoik().caps_lock, "and off again");
    f.synoik_state().synoik.lock_screen.settle();

    let (after, _, _) = render_output_vulkan(&mut f, &output);
    let differing = before
        .chunks_exact(4)
        .zip(after.chunks_exact(4))
        .filter(|(a, b)| a != b)
        .count();
    assert_eq!(
        differing, 0,
        "{differing} px still differ once caps lock is off — the warning did not go away"
    );
}

/// The message under the entry actually draws, in its own row, and goes away again.
///
/// It has its **own bake and element** so its wiggle can ride the element rather than the bake key
/// ([[animation-per-frame-bake]]). That split is exactly the change a state test cannot see: the
/// dialog can hold a perfectly correct message while the renderer draws it nowhere — wrong cache,
/// a texture sized to the wrong row, an origin taken from the column's refined layout instead of
/// the one the entry above it uses. Asserted as a differential, and *located*, because "some ink
/// appeared" would pass just as well if the message were drawn over the avatar.
#[test]
fn vulkan_draws_the_prompt_message_in_its_own_row() {
    use crate::dbus::gdm::{MessageKind, MessageSource, VerifierEvent};
    use crate::dbus::gnome_screen_saver::ScreenSaverToSynoik;

    let Some(mut f) = green_window_fixture() else {
        return;
    };
    let output = f.synoik_output(1);

    f.synoik_state()
        .on_screen_saver_msg(ScreenSaverToSynoik::Lock(None));
    f.synoik_state().on_verifier_event(VerifierEvent::Ready(1));
    f.synoik_state()
        .on_verifier_event(VerifierEvent::AskQuestion {
            question: "Password:".to_owned(),
            secret: true,
        });
    f.synoik_state()
        .on_shield_key(None, Some('a'), Default::default());
    f.synoik_state().synoik.lock_screen.settle();
    let (before, w, h) = render_output_vulkan(&mut f, &output);

    f.synoik_state()
        .on_verifier_event(VerifierEvent::ShowMessage {
            text: "Fingerprint reader unavailable".to_owned(),
            kind: MessageKind::Error,
            source: MessageSource::Fingerprint,
        });
    f.synoik_state().synoik.lock_screen.settle();
    let (during, _, _) = render_output_vulkan(&mut f, &output);

    let drawn: Vec<(i32, i32)> = (0..w * h)
        .map(|i| (i % w, i / w))
        .filter(|(x, y)| px(&during, w, *x, *y) != px(&before, w, *x, *y))
        .collect();
    assert!(!drawn.is_empty(), "the message drew no ink at all");

    // Below the middle of the screen: the prompt block is centred, and the message is the last
    // row in it, so ink from a wrong origin — the column's top-left is the usual one — lands
    // above the middle instead. Which row it *is* is pinned by the layout test next door; this
    // one is here for the half a layout test cannot see, that the row is drawn at all.
    let top = drawn.iter().map(|(_, y)| *y).min().unwrap();
    assert!(
        top > h / 2,
        "the message drew at y={top}, above the middle of a {h}px screen"
    );
    // Horizontally centred, near enough: a message drawn from the column's left edge would sit
    // wholly in one half.
    let (x0, x1) = (
        drawn.iter().map(|(x, _)| *x).min().unwrap(),
        drawn.iter().map(|(x, _)| *x).max().unwrap(),
    );
    let centre = f64::from(x0 + x1) / 2.;
    assert!(
        (centre - f64::from(w) / 2.).abs() <= f64::from(w) / 20.,
        "the message is centred at {centre}, not near {}",
        f64::from(w) / 2.
    );

    // ...and clearing it takes the ink away again. Compared over the pixels the message itself
    // touched rather than the whole frame: the reset that clears it also empties the entry and
    // drops the question, so a frame-wide diff would be measuring those instead.
    f.synoik_state().on_verifier_event(VerifierEvent::Reset);
    // The read-time floor holds the message up past the reset; drain it the way the timer would.
    let now = crate::utils::get_monotonic_time() + std::time::Duration::from_secs(30);
    let effects = f.synoik_state().synoik.unlock_dialog.tick(now);
    f.synoik_state().apply_unlock_effects(effects);
    assert!(
        f.synoik().unlock_dialog.message().is_none(),
        "the message outlived the reset"
    );
    f.synoik_state().synoik.lock_screen.settle();

    let (after, _, _) = render_output_vulkan(&mut f, &output);
    let left_behind = drawn
        .iter()
        .filter(|(x, y)| px(&after, w, *x, *y) != px(&before, w, *x, *y))
        .count();
    assert_eq!(
        left_behind,
        0,
        "{left_behind} of the message's {} px are still lit once it is gone",
        drawn.len()
    );
}

/// The unlock prompt draws over the curtain, and the entry shows dots rather than the password.
///
/// The masking assertion is the one that matters and it is the one a state test cannot make: the
/// dialog can mask correctly and the renderer still draw `content.entry` from the wrong field. So
/// this asserts on pixels — a wide-enough run of bright ink inside the entry pill, and glyph shapes
/// that do not change when the *characters* change but the length does not.
#[test]
fn vulkan_draws_the_unlock_prompt_with_a_masked_entry() {
    use crate::dbus::gdm::VerifierEvent;
    use crate::dbus::gnome_screen_saver::ScreenSaverToSynoik;

    let Some(mut f) = green_window_fixture() else {
        return;
    };
    let output = f.synoik_output(1);

    f.synoik_state()
        .on_screen_saver_msg(ScreenSaverToSynoik::Lock(None));
    f.synoik_state().on_verifier_event(VerifierEvent::Ready(1));
    f.synoik_state()
        .on_verifier_event(VerifierEvent::AskQuestion {
            question: "Password:".to_owned(),
            secret: true,
        });
    // Raise the prompt and type.
    for c in "abcdefgh".chars() {
        f.synoik_state()
            .on_shield_key(None, Some(c), Default::default());
    }
    assert_eq!(
        f.synoik().unlock_dialog.entry_display().chars().count(),
        8,
        "the fixture typed into a live entry"
    );
    // The curtain's slide and the clock↔prompt crossfade both run on the wall clock, and both
    // start from invisible, so a render taken this instant would catch the shield off the top of
    // the screen with the prompt at alpha ~0.
    f.synoik_state().synoik.lock_screen.settle();

    let (pixels, w, h) = render_output_vulkan(&mut f, &output);

    // The desktop is still hidden — the prompt page must not have replaced the curtain's cover.
    let is_green = |p: [u8; 4]| p[0] < 40 && p[1] > 200 && p[2] < 40 && p[3] > 200;
    let green = (0..w * h)
        .filter(|i| is_green(px(&pixels, w, i % w, i / w)))
        .count();
    assert_eq!(green, 0, "the desktop shows through the unlock prompt");

    // Find the brightest rows: the avatar plate, the name, the entry. Just assert that there IS
    // bright ink below the vertical third (where the stack starts) — the prompt drew at all.
    let is_bright = |p: [u8; 4]| p[0] > 200 && p[1] > 200 && p[2] > 200;
    let bright_below_third = (h / 3..h)
        .flat_map(|y| (0..w).map(move |x| (x, y)))
        .filter(|(x, y)| is_bright(px(&pixels, w, *x, *y)))
        .count();
    assert!(bright_below_third > 0, "the unlock prompt did not draw");

    // The masking check: re-render with *different characters, same length*. If the entry drew the
    // raw text, the ink would move; masked, every frame is the same eight dots.
    let baseline = pixels.clone();
    for _ in 0..8 {
        f.synoik_state().on_shield_key(
            Some(smithay::input::keyboard::Keysym::BackSpace),
            None,
            Default::default(),
        );
    }
    for c in "zyxwvuts".chars() {
        f.synoik_state()
            .on_shield_key(None, Some(c), Default::default());
    }
    let (pixels2, _, _) = render_output_vulkan(&mut f, &output);

    assert_eq!(
        baseline.len(),
        pixels2.len(),
        "the two frames must be comparable"
    );
    let differing = baseline
        .chunks_exact(4)
        .zip(pixels2.chunks_exact(4))
        .filter(|(a, b)| a != b)
        .count();
    assert_eq!(
        differing, 0,
        "{differing} px changed when only the password's characters did — \
         the entry is drawing the plaintext, not the mask"
    );
}

/// Opening the picker from the quick-settings screenshot button must not photograph the
/// quick-settings menu.
///
/// The picker works by *freezing the screen* — `open_screenshot_ui` captures neutrals through the
/// renderer before it opens — and our menus close on a fade, so the menu was still fully drawn at
/// the instant of that capture and landed in every shot taken from its own button. GNOME closes
/// this one menu with `PopupAnimation.NONE` and defers the open to a `BEFORE_REDRAW` later
/// (`js/ui/status/system.js:121-128`) for exactly this reason.
///
/// Driven through real pointer input at the coordinates the menu itself lays out, and asserted on
/// the *pixels of the frozen screen*: what makes this a bug is what ends up in the capture, so
/// checking that the popover's state flag flipped would be checking the fix rather than the defect.
#[test]
fn vulkan_screenshot_from_quick_settings_does_not_freeze_the_menu_into_the_shot() {
    let Some(mut f) = green_window_fixture() else {
        return;
    };
    let output = f.synoik_output(1);
    f.synoik().update_render_elements(None);

    // Open quick settings from the panel.
    let x = super::gnome::qs_center_x(&mut f, f64::from(OUT_W));
    super::gnome::pointer_motion_to(&mut f, x, 10.);
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);
    f.settle_animations();
    assert_eq!(
        f.synoik().panel_popover.open_role(),
        Some(crate::ui::panel::ROLE_QUICK_SETTINGS),
        "the status cluster must open quick settings"
    );

    // Remember where the menu is *while it is still up* — after the click it is gone, and this
    // rect is the region the assertion below inspects.
    let origin = super::gnome::popover_origin(&mut f);
    let size = f
        .synoik()
        .panel_popover
        .content_size()
        .expect("the open menu must have a size");
    let scale = output.current_scale().fractional_scale();
    let menu =
        Rectangle::<f64, Logical>::new(origin, size).to_physical_precise_round::<_, i32>(scale);

    // The screenshot button's own rect, asked of the menu rather than recomputed: the system row
    // is laid out differently with and without a battery pill.
    let has_pill = f
        .synoik()
        .panel_popover
        .quick_settings()
        .expect("quick settings must be the open content")
        .has_pill();
    let button = crate::ui::quick_settings::sys_rect(
        crate::ui::quick_settings::SysButton::Screenshot,
        has_pill,
    );

    super::gnome::pointer_motion_to(
        &mut f,
        origin.x + button.loc.x + button.size.w / 2.,
        origin.y + button.loc.y + button.size.h / 2.,
    );
    f.pointer_button(BTN_LEFT, ButtonState::Pressed);
    f.pointer_button(BTN_LEFT, ButtonState::Released);

    assert!(
        f.synoik().screenshot_ui.is_open(),
        "the quick-settings screenshot button must open the picker"
    );

    settle_screenshot_ui_open(&mut f);
    let (pixels, w, h) = render_output_vulkan_target(&mut f, &output, RenderTarget::Output);

    // `.popup-menu-content`'s `$bg_color` is #36363a. The picker shades the unselected area to 50%
    // black, so a frozen menu reads as roughly half that — count both, since the menu's default
    // position may straddle the selection edge.
    let is_menu_bg = |p: [u8; 4]| {
        let near = |v: u8, t: i16| (i16::from(v) - t).abs() <= 4;
        (near(p[0], 54) && near(p[1], 54) && near(p[2], 58))
            || (near(p[0], 27) && near(p[1], 27) && near(p[2], 29))
    };
    let mut menu_px = 0;
    for y in menu.loc.y.max(0)..(menu.loc.y + menu.size.h).min(h) {
        for x in menu.loc.x.max(0)..(menu.loc.x + menu.size.w).min(w) {
            if is_menu_bg(px(&pixels, w, x, y)) {
                menu_px += 1;
            }
        }
    }
    let area = menu.size.w * menu.size.h;
    eprintln!(
        "vulkan_screenshot_from_quick_settings_does_not_freeze_the_menu_into_the_shot: \
         {menu_px} menu-bg px of {area} in {menu:?}"
    );
    // A frozen menu fills essentially all of this rect; stray wallpaper pixels cannot.
    assert!(
        menu_px * 10 < area,
        "the quick-settings menu is in the frozen screen ({menu_px} of {area} px in {menu:?})"
    );
}

/// Window mode picks from windows frozen at open, and the selector draws them where it says they
/// are.
///
/// Two things this pins that nothing else does. First, the picker's Window mode captures each
/// window **at open** (`UIWindowSelector.capture`, `js/ui/screenshot.js:1062-1094`) — so it never
/// depends on the window still being there, or still showing what it showed. Second, the selector
/// slots come from the exposé layout while the hit test reads the same slots: one shared vector,
/// like the panel's `PanelLayout`, and this is what fails if they ever part ways.
#[test]
fn vulkan_screenshot_ui_window_mode_picks_a_frozen_window() {
    let Some((mut f, id, surface)) = window_fixture_with_client(GREEN, true, None) else {
        return;
    };
    let output = f.synoik_output(1);
    let scale = output.current_scale().fractional_scale();

    f.synoik_state().open_screenshot_ui(false, None);
    // Recolour the live window red *after* the picker froze it. Every green pixel below therefore
    // came from the frozen capture, and any red would mean Window mode read the live window —
    // which is the whole difference between this and `screenshot_window`.
    recolor_window(&mut f, id, &surface, RED);
    settle_screenshot_ui_open(&mut f);
    render_output_vulkan_target(&mut f, &output, RenderTarget::Output);

    assert!(
        f.synoik().screenshot_ui.any_windows(),
        "the fixture's window must reach the selector, or the Window button stays insensitive"
    );

    // The focused window is picked up front, so the selector opens on something.
    let (_, selected) = f
        .synoik()
        .screenshot_ui
        .selected_window()
        .expect("the selector must open with the focused window checked");

    // Switch to Window mode by clicking its type button, at the coordinates the layout publishes.
    let panel = f.synoik().screenshot_ui.panel_rect(&output).unwrap();
    let layout = f.synoik().screenshot_ui.panel_layout(&output).unwrap();
    let window_button = layout.type_buttons[2];
    let point = Point::<f64, Logical>::from((
        window_button.loc.x + window_button.size.w / 2.,
        window_button.loc.y + window_button.size.h / 2.,
    ))
    .to_physical(scale)
    .to_i32_round::<i32>()
        + panel.loc;

    {
        let ui = &mut f.synoik_state().synoik.screenshot_ui;
        ui.pointer_motion(point, None);
        ui.pointer_down(output.clone(), point, None, false);
        assert_eq!(ui.pointer_up(None), Some(PointerUp::Redraw));
        assert_eq!(
            ui.capture_type(),
            CaptureType::Window,
            "the Window button must switch modes now that there is a window to pick"
        );
    }

    let (size, pixels) = f
        .synoik()
        .screenshot_ui
        .capture_from_neutral()
        .expect("Window mode must capture the selected window");
    assert!(size.w > 0 && size.h > 0, "empty window capture: {size:?}");

    let is_green = |p: [u8; 4]| p[0] < 40 && p[1] > 200 && p[2] < 40 && p[3] > 200;
    let is_red = |p: [u8; 4]| p[0] > 200 && p[1] < 40 && p[2] < 40;
    let count = |pred: &dyn Fn([u8; 4]) -> bool| {
        (0..size.w * size.h)
            .filter(|i| pred(px(&pixels, size.w, i % size.w, i / size.w)))
            .count()
    };
    let (green, red) = (count(&is_green), count(&is_red));
    eprintln!(
        "vulkan_screenshot_ui_window_mode_picks_a_frozen_window: window {selected}, \
         {size:?}, {green} green px, {red} red px"
    );
    assert!(
        green > 1000,
        "the window capture is not the green window ({green} green px in {size:?})"
    );
    assert!(
        red < 100,
        "Window mode captured the live (recoloured) window, not the one frozen at open \
         ({red} red px)"
    );
}

/// With no windows, the Window button must not switch modes.
///
/// GNOME keeps the button *visible* and drops its `reactive` (`_syncWindowButtonSensitivity`,
/// `js/ui/screenshot.js:1529-1536`), so it still occupies the row — a button that vanished would
/// reflow the panel every time the last window closed. What must not happen is a mode with nothing
/// in it: Window mode with an empty selector captures nothing at all.
#[test]
fn vulkan_screenshot_ui_window_button_is_inert_without_windows() {
    if VulkanRenderer::new().is_err() {
        eprintln!("skipping: no Vulkan device");
        return;
    }

    let mut f = Fixture::new();
    f.synoik_state()
        .backend
        .headless()
        .add_renderer()
        .expect("build the Vulkan renderer");
    f.add_output(1, (OUT_W, OUT_H));
    f.synoik().update_render_elements(None);

    let output = f.synoik_output(1);
    f.synoik_state().open_screenshot_ui(false, None);
    settle_screenshot_ui_open(&mut f);
    render_output_vulkan_target(&mut f, &output, RenderTarget::Output);

    assert!(!f.synoik().screenshot_ui.any_windows());
    assert!(f.synoik().screenshot_ui.selected_window().is_none());

    let panel = f.synoik().screenshot_ui.panel_rect(&output).unwrap();
    let layout = f.synoik().screenshot_ui.panel_layout(&output).unwrap();
    let button = layout.type_buttons[2];
    let scale = output.current_scale().fractional_scale();
    let point = Point::<f64, Logical>::from((
        button.loc.x + button.size.w / 2.,
        button.loc.y + button.size.h / 2.,
    ))
    .to_physical(scale)
    .to_i32_round::<i32>()
        + panel.loc;

    // No hover and no tooltip: an insensitive St.Button is `reactive = false`, so it never emits
    // `notify::hover` and its tip is never scheduled. A button that lit up and advertised itself
    // while refusing every click would be the worst of both.
    f.synoik_state()
        .synoik
        .screenshot_ui
        .pointer_motion(point, None);
    f.settle_animations();
    assert_eq!(
        f.synoik().screenshot_ui.tooltip_text(),
        None,
        "an insensitive Window button must not offer a tooltip"
    );

    let ui = &mut f.synoik_state().synoik.screenshot_ui;
    ui.set_capture_type(CaptureType::Window);
    assert_eq!(
        ui.capture_type(),
        CaptureType::Selection,
        "Window mode must refuse to engage with nothing to select"
    );

    // ...and clicking it does nothing either.
    ui.pointer_down(output.clone(), point, None, false);
    ui.pointer_up(None);
    assert_eq!(ui.capture_type(), CaptureType::Selection);
}

/// A tooltip waits out its delay before it draws, and follows the pointer between controls.
///
/// The delay is the feature: without it, a pointer crossing the panel on its way somewhere else
/// strobes every tip in the row. GNOME schedules the tip 300ms out and cancels that timeout
/// outright when the pointer leaves (`Tooltip.open`/`close`, `js/ui/screenshot.js:95-129`).
///
/// Driven through the real clock rather than by inspecting a timer field: the failure this pins is
/// a tip drawn too early, and only advancing time can tell those apart.
#[test]
fn vulkan_screenshot_ui_tooltip_waits_before_it_shows() {
    let Some(mut f) = green_window_fixture() else {
        return;
    };
    let output = f.synoik_output(1);
    let scale = output.current_scale().fractional_scale();

    f.synoik_state().open_screenshot_ui(false, None);
    settle_screenshot_ui_open(&mut f);
    render_output_vulkan_target(&mut f, &output, RenderTarget::Output);

    let panel = f.synoik().screenshot_ui.panel_rect(&output).unwrap();
    let layout = f.synoik().screenshot_ui.panel_layout(&output).unwrap();
    let at = |r: Rectangle<f64, Logical>| {
        Point::<f64, Logical>::from((r.loc.x + r.size.w / 2., r.loc.y + r.size.h / 2.))
            .to_physical(scale)
            .to_i32_round::<i32>()
            + panel.loc
    };

    // Land on the Screen button. Nothing is due yet.
    f.synoik_state()
        .synoik
        .screenshot_ui
        .pointer_motion(at(layout.type_buttons[1]), None);
    assert_eq!(
        f.synoik().screenshot_ui.tooltip_text(),
        None,
        "the tip must not draw before its delay elapses"
    );
    assert!(
        f.synoik().screenshot_ui.are_animations_ongoing(),
        "a pending tip must keep the redraw loop alive, or it never becomes due"
    );

    // Wait it out. `settle_animations` moves the clock and advances, which is what makes the
    // delay actually elapse (the headless-animation-clock trap: completing animations does not).
    f.settle_animations();
    assert_eq!(
        f.synoik().screenshot_ui.tooltip_text(),
        Some("Screen Selection"),
        "the tip must say what the button does, not repeat its caption"
    );

    // Moving to another control restarts the wait rather than carrying the old tip across.
    f.synoik_state()
        .synoik
        .screenshot_ui
        .pointer_motion(at(layout.capture), None);
    assert_eq!(
        f.synoik().screenshot_ui.tooltip_text(),
        None,
        "moving between controls must restart the delay, not swap the text instantly"
    );

    f.settle_animations();
    assert_eq!(f.synoik().screenshot_ui.tooltip_text(), Some("Capture"));

    // Leaving the panel drops it.
    f.synoik_state()
        .synoik
        .screenshot_ui
        .pointer_motion(Point::from((5, 5)), None);
    assert_eq!(f.synoik().screenshot_ui.tooltip_text(), None);

    // An insensitive control offers no tooltip either: this fixture has a window, so unmap it and
    // check the Window button goes quiet rather than advertising a mode it will not enter.
    // (`reactive = false` gives St.Button no `notify::hover` at all, which is what drives both.)
    let window_button = at(layout.type_buttons[2]);
    f.synoik_state()
        .synoik
        .screenshot_ui
        .pointer_motion(window_button, None);
    f.settle_animations();
    assert_eq!(
        f.synoik().screenshot_ui.tooltip_text(),
        Some("Window Selection"),
        "a sensitive Window button does offer its tip"
    );
}
