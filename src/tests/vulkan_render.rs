//! End-to-end proof that the live `Niri::render` compositing path runs on the **owned Vulkan
//! renderer**, not just GLES: a real client window is mapped through the headless test harness and
//! the whole scene is composited through `VulkanRenderer`, both into an offscreen buffer (the
//! screenshot path) and into a **GBM-allocated scanout dmabuf** (the KMS-present path — everything
//! except the DRM page-flip, which is validated live). Exercises the renderer-agnostic render
//! helpers (Brick 2), the `try_as_gles` degradation guards (Brick 3), and `Bind<Dmabuf>` (Brick A).
//!
//! Skips gracefully when no Vulkan device is present. The scanout test additionally needs a real
//! GBM device (the render node), so it is Venus-only (lavapipe/CPU has no GBM).

use std::time::Duration;

use niri_config::{Action, Config, CornerRadius, WindowRule};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::{Element, RenderElement};
use smithay::backend::renderer::{Bind, Color32F, ExportMem, Frame, Renderer};
use smithay::output::Output;
use smithay::utils::{Buffer as BufferCoord, Physical, Point, Rectangle, Scale, Size, Transform};
use wayland_client::protocol::wl_shm;
use wayland_client::protocol::wl_surface::WlSurface;

use super::client::ClientId;
use super::fixture::Fixture;
use crate::niri::OutputRenderElements;
use crate::render_helpers::vulkan::VulkanRenderer;
use crate::render_helpers::{render_to_vec, RenderCtx, RenderTarget};
use crate::ui::mru::WindowMruUiRenderElement;
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
    f.niri_state()
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
        f.niri_complete_animations();
        f.double_roundtrip(id);
    }

    Some((f, id, surface))
}

/// Put the screenshot UI's chrome on screen by settling its open animation.
///
/// **`niri_complete_animations` does NOT settle this one.** It sets the clock's
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
/// event loop clears the clock (`Niri::refresh`).
fn settle_screenshot_ui_open(f: &mut Fixture) {
    let mut clock = f.niri().clock.clone();
    let now = clock.now_unadjusted();
    clock.set_unadjusted(now + Duration::from_millis(500));
    f.niri_complete_animations();
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

/// White pixels in the strip `y ∈ [h/16, h/8)`, where the MRU's scope panel draws its text.
///
/// Deliberately *below* the GNOME top panel, whose own white clock text sits in `y < h/16` and
/// would otherwise mask the MRU's absence — the caller asserts this strip is empty with the
/// switcher closed, so the measurement cannot quietly become vacuous.
fn white_px_in_scope_panel_strip(pixels: &[u8], w: i32, h: i32) -> usize {
    let is_white = |p: [u8; 4]| p[0] > 200 && p[1] > 200 && p[2] > 200;
    // The panel sits just below the GNOME top strip (its white clock is above `h / 24`); the owned
    // renderer places the scope text ink-tight, a touch higher than the old cairo layout did.
    ((h / 24) * w..(h / 8) * w)
        .filter(|i| is_white(px(pixels, w, i % w, i / w)))
        .count()
}

/// The alt-tab MRU draws its window titles and scope panel as CPU/cairo text that was uploaded
/// through GLES-locked elements — blank on the owned Vulkan renderer. Now the text is a
/// renderer-neutral buffer uploaded through the active renderer, so the scope panel must appear.
///
/// Measured against a closed-MRU baseline on purpose: the GNOME top panel draws its own white clock
/// text in the same strip, so a bare "white pixels exist up top" assert passes with the MRU absent
/// entirely — this test did exactly that until the clock pinning in `open_mru` landed.
#[test]
fn vulkan_mru_draws_the_scope_panel() {
    let Some(mut f) = green_window_fixture() else {
        return;
    };
    let output = f.niri_output(1);

    let (before, w, h) = render_output_vulkan_target(&mut f, &output, RenderTarget::Output);
    let baseline = white_px_in_scope_panel_strip(&before, w, h);
    // Guards the measurement itself: if anything else ever draws white here, the assert below stops
    // meaning "the MRU drew" and this fails instead of silently passing.
    assert_eq!(
        baseline, 0,
        "the scope-panel strip must be empty with the MRU closed, else it cannot witness the MRU"
    );

    open_mru(&mut f, &output);

    let (after, w, h) = render_output_vulkan_target(&mut f, &output, RenderTarget::Output);
    let white = white_px_in_scope_panel_strip(&after, w, h);

    eprintln!("vulkan_mru_draws_the_scope_panel: {white} white px in the scope-panel strip");
    assert!(
        white > 10,
        "the MRU scope panel text did not draw on Vulkan (blank overlay?): {white} white px"
    );
}

/// Open the alt-tab MRU over `output` and leave it fully open, so it actually composites.
///
/// The MRU renders nothing until `Inner::is_fully_open()`, which compares against the clock's
/// *unadjusted* time — `niri_complete_animations`'s `complete_instantly` does not move it, so the
/// switcher stays invisible for `recent_windows.open_delay_ms` (150ms by default) of real time.
/// Pinning the clock past the delay is what actually puts it on screen; without this, a test that
/// "opens" the MRU composites a frame the switcher has never appeared in, and every assertion
/// about its contents is vacuous.
fn open_mru(f: &mut Fixture, output: &Output) {
    let mut clock = f.niri().clock.clone();
    let wmru = crate::ui::mru::WindowMru::new(f.niri());
    f.niri()
        .window_mru_ui
        .open(clock.clone(), wmru, output.clone());
    assert!(f.niri().window_mru_ui.is_open(), "MRU must be open");

    let now = clock.now_unadjusted();
    clock.set_unadjusted(now + Duration::from_millis(500));
    f.niri_complete_animations();
}

/// [`open_mru`], returning the `WindowMruUiRenderElement`s the switcher contributes to the frame.
fn open_mru_and_collect(f: &mut Fixture, output: &Output) -> Vec<WindowMruUiRenderElement> {
    open_mru(f, output);

    let state = f.niri_state();
    let elements = state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| {
            let niri = &mut state.niri;
            niri.update_render_elements(Some(output));
            let ctx = RenderCtx {
                renderer: vk,
                target: RenderTarget::Output,
                xray: None,
            };
            let mut collected = Vec::new();
            niri.window_mru_ui
                .render_output(niri, output, ctx, &mut |elem| collected.push(elem));
            collected
        })
        .expect("headless backend must hold a Vulkan renderer");

    assert!(
        !elements.is_empty(),
        "the MRU contributed no render elements at all — it is not actually on screen, so any \
         assertion about its contents would be vacuous"
    );
    elements
}

/// The MRU soft-fades the right edge of a window title too long for its preview. A complete Vulkan
/// gradient-fade pipeline existed, but `mru.rs` only built the element inside a `try_as_gles()`
/// branch — always `None` once the GLES renderer went, so every title silently fell through to the
/// plain, hard-clipped texture. The fade itself is covered element-level in `vulkan/tests.rs`; what
/// was missing is the *wiring*, so assert the composited element list carries the Vulkan variant.
#[test]
fn vulkan_mru_titles_use_the_gradient_fade_element() {
    // A title far wider than the preview, so the element's cutoff is a real fade band rather than
    // the no-op `(1, 1)` a title that fits produces.
    let Some(mut f) = window_fixture_settled(
        GREEN,
        true,
        Some("A window title long enough to overflow its alt-tab preview and need fading"),
    ) else {
        return;
    };
    let output = f.niri_output(1);
    let mru_elements = open_mru_and_collect(&mut f, &output);

    let fades = mru_elements
        .iter()
        .filter(|elem| matches!(elem, WindowMruUiRenderElement::GradientFade(_)))
        .count();

    // One per window preview title. The GLES-gated (broken) path emits zero — every title took the
    // plain, hard-clipped `UiTexture` arm instead.
    assert!(
        fades > 0,
        "the MRU emitted no Vulkan gradient-fade element ({} MRU elements total); titles are \
         hard-clipped",
        mru_elements.len(),
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

    let (before, w, h) = render_output_vulkan_target(&mut f, &output, RenderTarget::Output);
    assert_eq!(
        white_px_in_scope_panel_strip(&before, w, h),
        0,
        "the scope-panel strip must be empty with the MRU closed, else it cannot witness the fade"
    );

    open_mru(&mut f, &output);

    // Start the close, then advance ~16 ms into the critically-damped fade spring so `alpha` is a
    // little below 1 (≈0.9) — enough to take the offscreen path yet keep the text bright.
    f.niri()
        .window_mru_ui
        .close(crate::ui::mru::MruCloseRequest::Cancel);
    let now = f.niri().clock.now_unadjusted();
    f.niri()
        .clock
        .set_unadjusted(now + Duration::from_millis(16));
    f.niri().advance_animations();

    let (pixels, w, h) = render_output_vulkan_target(&mut f, &output, RenderTarget::Output);

    // Same discriminator as the open case: at ~0.9 alpha the scope-panel text stays well above 200,
    // while the old blank-offscreen fade left only the backdrop faded over the desktop.
    let white = white_px_in_scope_panel_strip(&pixels, w, h);
    eprintln!(
        "vulkan_mru_closing_fade_draws_through_the_offscreen: {white} white px in the \
         scope-panel strip"
    );
    assert!(
        white > 10,
        "the MRU closing fade did not render through the Vulkan offscreen (blank fade?): {white} \
         white px"
    );
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
    let output = f.niri_output(1);

    f.niri_state().open_screenshot_ui(false, None);
    assert!(
        f.niri().screenshot_ui.is_open(),
        "screenshot UI must be open"
    );

    // Recolour the live window red. `Niri::render` early-returns after the screenshot UI's
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

/// The screenshot UI's help panel must actually draw.
///
/// The panel is the UI's most fragile element: drawn into an offscreen, gated on the open
/// animation's progress — chances to end up invisible while the frozen screenshot (which is *not*
/// progress-gated) still draws and makes the frame look right. Measure inside
/// [`ScreenshotUi::panel_rect`]: whole-frame white does not discriminate the panel, since the four
/// selection-border buffers alone score thousands of white px without it.
#[test]
fn vulkan_screenshot_ui_draws_the_help_panel() {
    let Some(mut f) = green_window_fixture() else {
        return;
    };
    let output = f.niri_output(1);

    f.niri_state().open_screenshot_ui(false, None);
    settle_screenshot_ui_open(&mut f);

    // The panel is built lazily on the first render, so render before reading its rect.
    let (pixels, w, h) = render_output_vulkan_target(&mut f, &output, RenderTarget::Output);

    let rect = f
        .niri()
        .screenshot_ui
        .panel_rect(&output)
        .expect("the open screenshot UI must have a help panel");

    // `generate_panel` fills the panel with rgb(0.1) — 26/255 — and writes the help text in white.
    let mut background = 0;
    let mut text = 0;
    for y in rect.loc.y..(rect.loc.y + rect.size.h).min(h) {
        for x in rect.loc.x..(rect.loc.x + rect.size.w).min(w) {
            let p = px(&pixels, w, x, y);
            if p[0] < 40 && p[1] < 40 && p[2] < 40 {
                background += 1;
            }
            if p[0] > 200 && p[1] > 200 && p[2] > 200 {
                text += 1;
            }
        }
    }
    eprintln!(
        "vulkan_screenshot_ui_draws_the_help_panel: {background} panel-bg px, {text} text px in \
         {rect:?}"
    );
    assert!(
        background > 1000,
        "the help panel's background did not draw ({background} px in {rect:?})"
    );
    assert!(
        text > 100,
        "the help panel drew no text or capture button ({text} white px in {rect:?})"
    );
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
    config.outputs.0.push(niri_config::Output {
        name: "headless-1".to_string(),
        scale: Some(niri_config::FloatOrInt(scale)),
        ..Default::default()
    });

    let mut f = Fixture::with_config(config);
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
    window.attach_solid_buffer(GREEN[0], GREEN[1], GREEN[2], GREEN[3]);
    window.set_size(WIN, WIN);
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    f.niri_complete_animations();
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
    let output = f.niri_output(1);
    let scale = output.current_scale().fractional_scale();
    assert!(
        (scale - 2.0).abs() < 1e-6,
        "expected a scale-2 output, got {scale}; the guard is vacuous otherwise"
    );

    f.niri_state().open_screenshot_ui(false, None);
    settle_screenshot_ui_open(&mut f);

    let (pixels, w, h) = render_output_vulkan_target(&mut f, &output, RenderTarget::Output);
    let rect = f
        .niri()
        .screenshot_ui
        .panel_rect(&output)
        .expect("the open screenshot UI must have a help panel");

    // The button sits at the panel's left: centre x = left + PADDING + RADIUS (both logical, in
    // screenshot_ui.rs: PADDING = 8, RADIUS = 16). Scan that column across the whole output and
    // measure the vertical extent of the white shutter ring/disk.
    let radius_phys = to_physical_precise_round::<i32>(scale, 16.);
    let padding_phys = to_physical_precise_round::<i32>(scale, 8.);
    // The button occupies the panel's left column: x in [left+PADDING, left+PADDING+2·RADIUS].
    // Bound the scan to the left of the help text (text_x ≈ PADDING + 2·RADIUS + PADDING) so
    // only the white shutter — not the white glyphs — is measured. Scan only WITHIN the panel
    // box vertically (the selection-rectangle chrome above the panel is white too). The button
    // is the only white here; measure its bounding-box height: a correct button is 2·RADIUS
    // (64px at 2×); the bug's 2× button clips to the ~106px panel, well outside tolerance
    // either way.
    let left = rect.loc.x;
    let text_x = left + 2 * radius_phys + padding_phys;
    let mut top: Option<i32> = None;
    let mut bot = 0;
    for y in rect.loc.y..(rect.loc.y + rect.size.h).min(h) {
        for x in (left + padding_phys)..text_x.min(w) {
            let p = px(&pixels, w, x, y);
            // Pure white shutter over the rgb(26) panel — a mid threshold catches the AA edge too.
            if p[0] > 128 && p[1] > 128 && p[2] > 128 {
                top.get_or_insert(y);
                bot = y;
                break;
            }
        }
    }
    let top = top.expect("no capture-button pixels found in the panel's left column");
    let extent = bot - top + 1;
    let expected = 2 * radius_phys; // 2·RADIUS logical, expressed at the output scale
    eprintln!(
        "button vertical extent {extent}px at scale {scale} (expected ~{expected}); panel {rect:?}"
    );
    // The bug doubled the button (~4·RADIUS·scale, clipped by the panel). Allow AA slack but
    // reject.
    assert!(
        (expected - 8..=expected + 8).contains(&extent),
        "capture-button vertical extent {extent}px != expected ~{expected}px — button tagged at \
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
    let output = f.niri_output(1);

    // `open_screenshot_ui` primes render elements before both passes; mirror that here.
    f.niri().update_render_elements(None);

    // Drive the Vulkan capture pass directly (disjoint borrows of niri + backend).
    let neutrals = {
        let state = f.niri_state();
        state
            .backend
            .headless()
            .with_vulkan_renderer(|vk| state.niri.capture_screenshot_neutrals(vk))
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

/// Freeze the current screen into a crossfade, and return the time it starts at — feed that to
/// [`pin_crossfade_at_start`] before rendering.
///
/// `ScreenTransition::alpha` reads the clock's *unadjusted* time, deliberately ignoring animation
/// slowdown, so neither `niri_complete_animations` nor a zero clock rate holds the crossfade still
/// — it advances with real monotonic time. Every event-loop iteration also calls `Clock::clear`
/// (`Niri::refresh`), so the first read after any roundtrip jumps to however long the test really
/// took. An unpinned test is therefore racing the 500ms crossfade: past ~78ms of real time, enough
/// of the live window bleeds through that the blend matches *neither* colour, and the test blank-
/// fails with `0 green, 0 red`. Three full-screen texture uploads fit inside that budget on a bad
/// day, which is what made this flaky.
fn start_screen_transition(f: &mut Fixture) -> Duration {
    // The clock is lazy, so this is the same value the transition records as its start.
    let start_at = f.niri().clock.now_unadjusted();
    f.niri_state()
        .do_action(Action::DoScreenTransition(Some(0)), false);
    start_at
}

/// Pin the unadjusted clock to the crossfade's start, fixing alpha at exactly 1.0 (fully the frozen
/// capture). Must be called after the last roundtrip, since the event loop clears the clock.
fn pin_crossfade_at_start(f: &mut Fixture, start_at: Duration) {
    f.niri().clock.clone().set_unadjusted(start_at);
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
    window.attach_solid_buffer(GREEN[0], GREEN[1], GREEN[2], GREEN[3]);
    window.set_size(WIN, WIN);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.niri_complete_animations();
    f.double_roundtrip(id);

    let output = f.niri_output(1);

    // Freeze the green-window screen into a transition, pinned at alpha = 1.0 (fully the capture).
    let start_at = start_screen_transition(&mut f);
    assert!(
        f.niri()
            .output_state
            .values()
            .any(|s| s.screen_transition.is_some()),
        "screen transition must be active after DoScreenTransition"
    );

    // *Every* target must take the Vulkan upload path. The Gles arm draws nothing on the owned
    // renderer, so a target that fell through to it would crossfade from a blank screen.
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
    window.attach_solid_buffer(GREEN[0], GREEN[1], GREEN[2], GREEN[3]);
    window.set_size(WIN, WIN);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.niri_complete_animations();
    f.double_roundtrip(id);

    let output = f.niri_output(1);

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
    window.attach_solid_buffer(GREEN[0], GREEN[1], GREEN[2], GREEN[3]);
    window.set_size(WIN, WIN);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.niri_complete_animations();
    f.double_roundtrip(id);

    let output = f.niri_output(1);

    // Drive the Vulkan capture pass directly (disjoint borrows of niri + backend).
    let neutrals = {
        let state = f.niri_state();
        state
            .backend
            .headless()
            .with_vulkan_renderer(|vk| state.niri.capture_screen_transition_neutrals(vk))
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

    let state = f.niri_state();
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

    let state = f.niri_state();
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
            let elements: Vec<OutputRenderElements> = niri.render_to_vec(ctx, &output, false);

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
            let elements: Vec<OutputRenderElements> = niri.render_to_vec(ctx, &output, false);

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
    let output = f.niri_output(1);
    let scale = Scale::from(output.current_scale().fractional_scale());
    let width = to_physical_precise_round(scale.x, output_size(&output).w);
    let bar_h = to_physical_precise_round(scale.x, crate::ui::panel::PANEL_HEIGHT);

    let state = f.niri_state();
    let opaque = state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| {
            let ws = state.niri.workspace_state_for(&output);
            let elems = state
                .niri
                .panel
                .render(vk, &output, ws, &state.niri.icon_cache);
            assert!(
                !elems.is_empty(),
                "panel produced no element on Vulkan (still blank)"
            );
            let pixels = render_to_vec(
                vk,
                Size::<i32, Physical>::from((width, bar_h)),
                scale,
                Transform::Normal,
                Fourcc::Abgr8888,
                elems.into_iter(),
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

/// The dateMenu calendar popover renders on the owned Vulkan renderer when open:
/// the calendar box is drawn offscreen and composited as a positioned element.
/// Assert `render` yields an element that composites opaque (the dark box) pixels.
#[test]
fn vulkan_renders_the_calendar_popover() {
    let Some(mut f) = window_fixture(GREEN) else {
        return;
    };
    let output = f.niri_output(1);
    let scale = Scale::from(output.current_scale().fractional_scale());

    // Open the calendar popover under the clock.
    {
        let anchor = f.niri().panel.date_menu_rect(output_size(&output).w);
        let cal = f.niri().gnome_settings.calendar;
        let accent = f.niri().gnome_settings.accent_color;
        f.niri().panel_popover.toggle_calendar(
            output.clone(),
            anchor,
            cal.week_start,
            cal.show_week_numbers,
            accent,
        );
    }
    assert!(f.niri().panel_popover.is_open());

    let state = f.niri_state();
    let opaque = state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| {
            let elems = state
                .niri
                .panel_popover
                .render(vk, &state.niri.icon_cache, &output);
            assert!(
                !elems.is_empty(),
                "an open popover must produce a render element"
            );
            // The popover composites centered under the clock, so capture the full
            // output width and enough height to include the calendar box.
            let w = to_physical_precise_round(scale.x, output_size(&output).w);
            let h = to_physical_precise_round(scale.x, 400.);
            let pixels = render_to_vec(
                vk,
                Size::<i32, Physical>::from((w, h)),
                scale,
                Transform::Normal,
                Fourcc::Abgr8888,
                elems.into_iter(),
            )
            .expect("render popover");
            pixels.chunks_exact(4).filter(|p| p[3] == 255).count()
        })
        .expect("vulkan renderer");

    assert!(
        opaque > 0,
        "the calendar popover did not composite any opaque pixels on Vulkan"
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
    let output = f.niri_output(1);
    let scale = Scale::from(output.current_scale().fractional_scale());

    // Open the quick-settings popover under the right-box indicator.
    {
        let output_w = output_size(&output).w;
        let toggles = f.niri().gnome_settings.quick_toggles;
        let anchor = f.niri().panel.quick_settings_rect(output_w);
        let accent = f.niri().gnome_settings.accent_color;
        f.niri()
            .panel_popover
            .toggle_quick_settings(output.clone(), anchor, toggles, accent);
    }
    assert!(f.niri().panel_popover.is_open());

    let state = f.niri_state();
    let opaque = state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| {
            let elems = state
                .niri
                .panel_popover
                .render(vk, &state.niri.icon_cache, &output);
            assert!(
                !elems.is_empty(),
                "an open quick-settings popover must produce render elements"
            );
            let w = to_physical_precise_round(scale.x, output_size(&output).w);
            let h = to_physical_precise_round(scale.x, 300.);
            let pixels = render_to_vec(
                vk,
                Size::<i32, Physical>::from((w, h)),
                scale,
                Transform::Normal,
                Fourcc::Abgr8888,
                elems.into_iter(),
            )
            .expect("render quick-settings popover");
            pixels.chunks_exact(4).filter(|p| p[3] == 255).count()
        })
        .expect("vulkan renderer");

    assert!(
        opaque > 0,
        "the quick-settings popover did not composite any opaque pixels on Vulkan"
    );
}

/// A resize animation on a Vulkan session must draw the cross-fade (`render_resize`), not the red
/// `SolidColorBuffer` placeholder. Reproduces the live "the window becomes a red rect while
/// maximizing/restoring" bug: map a window, issue a niri-driven (animated) resize, commit the new
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

    let mut f = Fixture::with_config(config);
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
    f.niri_complete_animations();
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

    // The server-side surface of the mapped window (cloned so the `f.niri()` borrow ends before we
    // borrow the backend's Vulkan renderer).
    let server_surface = {
        let mapped = f.niri().layout.windows().next().expect("a mapped window").1;
        mapped.toplevel().wl_surface().clone()
    };

    // buf_pos is irrelevant to the buffer content (we relocate by -geo.loc); use the origin.
    let scale = Scale::from(1.);
    let captured = f
        .niri_state()
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

    // The smithay `Window` backing the mapped tile (cloned so the `f.niri()` borrow ends). Uses the
    // `LayoutElement::id` trait method (`= &Window`), not the inherent `Mapped::id() -> MappedId`.
    let window_id = crate::layout::LayoutElement::id(
        f.niri().layout.windows().next().expect("a mapped window").1,
    )
    .clone();

    // Capture the unmap snapshot. On a Vulkan session this goes through the owned renderer and
    // bakes no GLES texture at all. `None` output → no xray background, which is all a plain window
    // needs.
    f.niri_state().store_unmap_snapshot(&window_id, None);

    // Inspect the tile's captured snapshot. The window is still mapped (storing a snapshot does not
    // unmap it), so the tile is still in the active workspace.
    let snapshot = f
        .niri_state()
        .niri
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

    // Map a real client window so the composited scene is non-trivial (the oracle samples the
    // backdrop below it, which is all the pick-path plumbing needs to exercise).
    let window = f.client(id).window(&surface);
    window.attach_shm_buffer(WIN as i32, WIN as i32, 0, 255, 0, 255);
    window.set_size(WIN, WIN);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.niri_complete_animations();
    f.double_roundtrip(id);

    let output = f.niri_output(1);
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

    let state = f.niri_state();
    state.niri.update_render_elements(Some(&output));

    for (probe, name) in probes {
        let want = px(&frame, fw, probe.x, probe.y);

        let vk_color = state
            .backend
            .headless()
            .with_vulkan_renderer(|vk| {
                crate::input::pick_color_grab::PickColorGrab::pick_color_with_renderer(
                    &state.niri,
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
    // genericized `Niri::screenshot_window` on the owned Vulkan renderer end-to-end (no disk write,
    // so no async encode thread to await) — it must run the full render + readback path without
    // erroring. Pixel correctness of the composited scene is proven by the whole-scene tests above.
    if VulkanRenderer::new().is_err() {
        eprintln!("skipping vulkan_screenshots_a_window_through_vulkan: no Vulkan device");
        return;
    }

    let mut f = Fixture::new();
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
    let state = f.niri_state();
    state.niri.update_render_elements(Some(&output));
    let mapped = state
        .niri
        .layout
        .windows()
        .next()
        .expect("a mapped window")
        .1;

    let ran = state.backend.headless().with_vulkan_renderer(|vk| {
        state
            .niri
            .screenshot_window(vk, &output, mapped, false, false, None)
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
/// This is the only test that drives `render_to_shm` (`niri.rs`'s shm screencopy branch) as a
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
    let output = f.niri_output(1);

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
            let elements: Vec<OutputRenderElements> = niri.render_to_vec(ctx, &output, false);

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

/// A configured custom **open** shader must actually be reached by `Tile::render`, not merely
/// compile. `vulkan_custom_anim_element_draws_the_open_shader` builds the element by hand, so it
/// proves the shader works while saying nothing about whether the compositor ever gets there.
///
/// The wiring is what is fragile: the install runs inside `reload_config`
/// (`niri.rs:1717` — `with_vulkan_renderer(|vk| vk.set_custom_open_shader(src))`), and
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
    f.niri_state()
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
    f.niri_state().reload_config(Ok(config));

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

    let output = f.niri_output(1);
    assert!(
        f.niri().layout.are_animations_ongoing(Some(&output)),
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
    use niri_config::animations::{Curve, EasingParams, Kind};

    let Some(mut f) = green_window_fixture() else {
        return;
    };
    let output = f.niri_output(1);

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
    let anim = niri_config::Animation {
        off: false,
        kind: Kind::Easing(EasingParams {
            duration_ms: 1000,
            curve: Curve::Linear,
        }),
    };
    f.niri_state()
        .niri
        .layout
        .active_workspace_mut()
        .expect("active workspace")
        .tiles_mut()
        .next()
        .expect("a tile")
        .animate_alpha(1., 0., anim);

    let now = f.niri().clock.now_unadjusted();
    f.niri()
        .clock
        .set_unadjusted(now + std::time::Duration::from_millis(500));
    f.niri().advance_animations();
    assert!(
        f.niri().layout.are_animations_ongoing(Some(&output)),
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
        .create_buffer(Fourcc::Abgr8888, Size::from((S, S)))
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
    use niri_vk::render::PostprocessPush;
    use smithay::backend::renderer::element::Kind;
    use smithay::backend::renderer::{Offscreen, Texture as _};

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
        niri_scale: 1.0,
        niri_alpha: 1.0,
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
            .create_buffer(Fourcc::Abgr8888, Size::<i32, BufferCoord>::from((s, s)))
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

    // The states of the Vulkan render must be observable: `Niri::update_primary_scanout_output`
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
    use niri_config::CornerRadius;
    use smithay::backend::renderer::element::Kind;
    use smithay::utils::Logical;

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

    let state = f.niri_state();

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
    let output = f.niri_output(1);

    let state = f.niri_state();
    let found = state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| {
            let niri = &mut state.niri;
            niri.update_render_elements(Some(&output));
            let ctx = RenderCtx {
                renderer: vk,
                target: RenderTarget::Output,
                xray: None,
            };
            // Same `{elem:?}`-sniffing idiom as `render_helpers::debug::push_opaque_regions`: the
            // element list is an opaque enum tree, and `ExtraDamage` is the only thing in this
            // scene that prints as one (blur/background-effect, the other user, is off here).
            niri.render_to_vec(ctx, &output, false)
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

    let mut f = Fixture::with_config(config);
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
    let output = f.niri_output(1);

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
    config.layout.shadow.offset = niri_config::ShadowOffset {
        x: niri_config::FloatOrInt(0.),
        y: niri_config::FloatOrInt(0.),
    };

    let mut f = Fixture::with_config(config);
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
    use niri_config::animations::{Curve, EasingParams, Kind};
    use niri_config::BlockOutFrom;
    use niri_ipc::SizeChange;

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

    // Start an animated resize, and let the client commit the new size so the crossfade begins.
    f.niri().layout.set_column_width(SizeChange::SetFixed(900));
    f.double_roundtrip(id);
    let window = f.client(id).window(&surface);
    window.attach_shm_buffer(900, WIN as i32, 0, 255, 0, 255);
    window.set_size(900, WIN);
    window.ack_last_and_commit();
    f.roundtrip(id);

    let output = f.niri_output(1);
    assert!(
        f.niri().layout.are_animations_ongoing(Some(&output)),
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
    let output = f.niri_output(1);

    f.niri_state().open_screenshot_ui(false, None);
    assert!(
        f.niri().screenshot_ui.is_open(),
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
        f.niri().layout.windows().next().expect("a mapped window").1,
    )
    .clone();

    f.niri_state()
        .store_unmap_snapshot(&window_id, Some(output));

    let transaction = Transaction::new();
    let blocker = transaction.blocker();
    let state = f.niri_state();
    // `None`, as a Vulkan session does: the snapshot is renderer-neutral and no GLES renderer is
    // involved in starting the animation.
    state
        .niri
        .layout
        .start_close_animation_for_window(&window_id, blocker);
    state.niri.layout.remove_window(&window_id, transaction);
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
    let output = f.niri_output(1);

    close_the_only_window(&mut f, &output);
    assert!(
        f.niri().layout.are_animations_ongoing(Some(&output)),
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

    use niri_config::BlockOutFrom;

    let mut config = Config::default();
    config.window_rules.push(WindowRule {
        block_out_from: Some(BlockOutFrom::Screencast),
        ..Default::default()
    });

    let mut f = Fixture::with_config(config);
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
    close_the_only_window(&mut f, &output);
    assert!(
        f.niri().layout.are_animations_ongoing(Some(&output)),
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
    let output = f.niri_output(1);

    // The overlay is shown at startup by default; hide it for a clean baseline.
    f.niri().hotkey_overlay.hide();
    let (before, w, h) = render_output_vulkan_target(&mut f, &output, RenderTarget::Output);
    assert_eq!(
        white_px_in_overlay_band(&before, w, h),
        0,
        "the overlay band must be empty with the overlay closed, else it cannot witness it"
    );

    f.niri().hotkey_overlay.show();

    let (after, w, h) = render_output_vulkan_target(&mut f, &output, RenderTarget::Output);
    let white = white_px_in_overlay_band(&after, w, h);
    eprintln!("vulkan_hotkey_overlay_draws: {white} white px in the overlay band");
    assert!(
        white > 500,
        "the hotkey overlay text did not draw on Vulkan (blank overlay?): {white} white px"
    );
}
