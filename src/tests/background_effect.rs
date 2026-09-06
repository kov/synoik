// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! `ext-background-effect-v1` end to end: a real client sets a blur region, and the compositor
//! side is asked what it made of it.
//!
//! The resolution *rule* (region → blur, and which of the two draw paths) is pinned next to it in
//! `render_helpers::background_effect`. What these cover is the seam between: the protocol object,
//! the double-buffered commit, our post-commit hook, and the lazily-recomputed rect cache. A bug
//! anywhere along there means no blur at all, silently — the surface just renders as it always did.

use std::time::Duration;

use smithay::reexports::wayland_protocols::ext::background_effect::v1::client::ext_background_effect_manager_v1::Capability;
use smithay::wayland::compositor::with_states;

use super::*;
use crate::handlers::background_effect::get_cached_blur_region;

/// Map a window and hand back the fixture plus the client's surface.
fn mapped_window(
    f: &mut Fixture,
) -> (
    crate::tests::client::ClientId,
    wayland_client::protocol::wl_surface::WlSurface,
) {
    f.add_output(1, (1920, 1080));
    let id = f.add_client();

    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    (id, surface)
}

/// The compositor-side rects for the client's surface, as the renderer would read them.
fn compositor_blur_rects(
    f: &mut Fixture,
) -> Option<Vec<smithay::utils::Rectangle<i32, smithay::utils::Logical>>> {
    let mapped = f.synoik().layout.windows().next().unwrap().1;
    let surface = mapped.toplevel().wl_surface().clone();
    with_states(&surface, |states| {
        get_cached_blur_region(states).map(|rects| rects.as_ref().clone())
    })
}

/// Render one full frame through the real path and drain what `render_for_tile` resolved.
///
/// Rendering is what runs the resolution, so there is no cheaper way to see it:
/// `update_render_elements` settles the *options*, but the geometry the effect lands on is computed
/// per frame.
fn sample_effect(
    f: &mut Fixture,
) -> Vec<crate::render_helpers::background_effect::trace::EffectSample> {
    use crate::render_helpers::background_effect::trace;
    use crate::render_helpers::{RenderCtx, RenderTarget};

    // Drop anything an earlier frame left behind, so a sample is only this frame.
    let _ = trace::take();

    let output = f.synoik_output(1);
    let state = f.synoik_state();
    state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| {
            let synoik = &mut state.synoik;
            synoik.update_render_elements(Some(&output));
            let ctx = RenderCtx {
                renderer: vk,
                target: RenderTarget::Output,
                appearance: Some(synoik.appearance()),
            };
            // Building the elements is what runs the resolution; nothing needs to be drawn.
            let _ = synoik.render_to_vec(ctx, &output, false);
        })
        .expect("the fixture must have a Vulkan renderer");

    trace::take()
}

/// Compare two rects up to float slop.
///
/// The subregion's corners are scaled and offset per rect, so a bbox that *is* the tile still
/// comes back as `401.99999999999994` against `402.0`. Exact equality here is a flake generator:
/// which side of the epsilon it lands on depends on the scale factor the animation happens to be
/// at when the sample is taken, so it passes alone and fails in the full suite.
fn rect_approx_eq(
    a: smithay::utils::Rectangle<f64, smithay::utils::Logical>,
    b: smithay::utils::Rectangle<f64, smithay::utils::Logical>,
) -> bool {
    const EPS: f64 = 1e-6;
    (a.loc.x - b.loc.x).abs() < EPS
        && (a.loc.y - b.loc.y).abs() < EPS
        && (a.size.w - b.size.w).abs() < EPS
        && (a.size.h - b.size.h).abs() < EPS
}

/// Build this frame's elements and hand them to a damage tracker, returning the damage it computed
/// plus what the effect resolved.
///
/// The headless backend's `render` is bookkeeping only — it never builds a damage tracker — so the
/// whole corpus composites every frame from scratch and *cannot* see a damage bug. The live path
/// does the opposite: `OutputDamageTracker` redraws only what it is told changed, and whatever it
/// skips keeps whatever the recycled buffer already held. Driving the tracker by hand is the only
/// way to put that logic under test.
fn sample_with_damage(
    f: &mut Fixture,
    tracker: &mut smithay::backend::renderer::damage::OutputDamageTracker,
) -> (
    Vec<smithay::utils::Rectangle<i32, smithay::utils::Physical>>,
    Vec<crate::render_helpers::background_effect::trace::EffectSample>,
) {
    use crate::render_helpers::background_effect::trace;
    use crate::render_helpers::{RenderCtx, RenderTarget};

    let _ = trace::take();

    let output = f.synoik_output(1);
    let state = f.synoik_state();
    let damage = state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| {
            let synoik = &mut state.synoik;
            synoik.update_render_elements(Some(&output));
            let ctx = RenderCtx {
                renderer: vk,
                target: RenderTarget::Output,
                appearance: Some(synoik.appearance()),
            };
            let elements = synoik.render_to_vec(ctx, &output, false);
            // age 1: the buffer we would be drawing into holds the previous frame, which is the
            // situation partial damage exists for and the one that can show stale content.
            let (damage, _states) = tracker.damage_output(1, &elements).expect("damage output");
            damage.cloned().unwrap_or_default()
        })
        .expect("the fixture must have a Vulkan renderer");

    (damage, trace::take())
}

/// Render one real frame through `tracker`, returning what the capture grabbed and what the effect
/// resolved. Unlike `sample_with_damage` this actually draws, which is the only way
/// `capture_framebuffer` runs at all.
/// As [`render_frame`], but cycling `n` distinct framebuffers the way a real swapchain does, and
/// reporting each one's true age.
///
/// The headless harness has no `DrmCompositor` and no KMS, so a test that renders into one reused
/// offscreen at `age = 1` is not exercising the buffer cycling a seat actually runs. That matters
/// here because the damage a frame must repaint is a function of its target's age: a buffer last
/// drawn three frames ago owes three frames of damage, and whether an effect's "did anything below
/// me change" test sees one frame of damage or three is exactly the question.
fn render_frame_aged(
    f: &mut Fixture,
    tracker: &mut smithay::backend::renderer::damage::OutputDamageTracker,
    buffers: &mut Vec<(crate::render_helpers::vulkan::VkTexture, u64)>,
    slots: usize,
    frame_no: u64,
) -> (
    Vec<crate::render_helpers::background_effect::trace::CaptureSample>,
    Vec<crate::render_helpers::background_effect::trace::EffectSample>,
) {
    use smithay::backend::renderer::Bind;
    use smithay::utils::{Physical, Size};

    use crate::render_helpers::background_effect::trace;
    use crate::render_helpers::{create_texture, RenderCtx, RenderTarget, NATIVE_FOURCC};

    let _ = trace::take();
    let _ = trace::take_captures();

    let output = f.synoik_output(1);
    let size: Size<i32, Physical> = output.current_mode().unwrap().size;
    let state = f.synoik_state();
    state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| {
            while buffers.len() < slots {
                buffers.push((
                    create_texture(vk, size, NATIVE_FOURCC).expect("create offscreen"),
                    0,
                ));
            }
            let idx = (frame_no as usize) % slots;
            // Age is how many frames back this slot was last rendered into; 0 means never, which
            // is what smithay reads as "no usable history, repaint everything".
            let age = if buffers[idx].1 == 0 {
                0
            } else {
                frame_no - buffers[idx].1
            };
            buffers[idx].1 = frame_no;

            let synoik = &mut state.synoik;
            synoik.update_render_elements(Some(&output));
            let ctx = RenderCtx {
                renderer: vk,
                target: RenderTarget::Output,
                appearance: Some(synoik.appearance()),
            };
            let elements = synoik.render_to_vec(ctx, &output, false);
            let mut fb = vk.bind(&mut buffers[idx].0).expect("bind offscreen");
            tracker
                .render_output(vk, &mut fb, age as usize, &elements, [0., 0., 0., 1.])
                .expect("render output");
        })
        .expect("the fixture must have a Vulkan renderer");

    (trace::take_captures(), trace::take())
}

fn render_frame(
    f: &mut Fixture,
    tracker: &mut smithay::backend::renderer::damage::OutputDamageTracker,
) -> (
    Vec<crate::render_helpers::background_effect::trace::CaptureSample>,
    Vec<crate::render_helpers::background_effect::trace::EffectSample>,
) {
    use smithay::backend::renderer::Bind;
    use smithay::utils::{Physical, Size};

    use crate::render_helpers::background_effect::trace;
    use crate::render_helpers::{create_texture, RenderCtx, RenderTarget, NATIVE_FOURCC};

    let _ = trace::take();
    let _ = trace::take_captures();

    let output = f.synoik_output(1);
    let size: Size<i32, Physical> = output.current_mode().unwrap().size;
    let state = f.synoik_state();
    state
        .backend
        .headless()
        .with_vulkan_renderer(|vk| {
            let synoik = &mut state.synoik;
            synoik.update_render_elements(Some(&output));
            let ctx = RenderCtx {
                renderer: vk,
                target: RenderTarget::Output,
                appearance: Some(synoik.appearance()),
            };
            let elements = synoik.render_to_vec(ctx, &output, false);
            let mut texture = create_texture(vk, size, NATIVE_FOURCC).expect("create offscreen");
            let mut fb = vk.bind(&mut texture).expect("bind offscreen");
            tracker
                .render_output(vk, &mut fb, 1, &elements, [0., 0., 0., 1.])
                .expect("render output");
        })
        .expect("the fixture must have a Vulkan renderer");

    (trace::take_captures(), trace::take())
}

/// The effect must track the *tile* through a resize, subregion included — including the frames
/// in the middle of the resize animation, where the client's buffer has already grown but the
/// tile has not caught up.
///
/// This is the shape of bug an end-state test cannot see: at both ends of a resize every rect
/// agrees, and only the frames between can disagree. It is pinned here because that mid-flight
/// consistency is what "the blur trails the window" would break, and reading the code is not
/// enough to know it holds — the effect geometry, the client's committed geometry and the region
/// are three values updated on two different clocks.
#[test]
fn the_effect_tracks_the_tile_through_a_resize() {
    use crate::render_helpers::vulkan::VulkanRenderer;

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
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    window.attach_solid_buffer(0, u32::MAX, 0, u32::MAX);
    window.set_size(400, 300);
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    let effect = f.client(id).set_blur_region(&surface, (0, 0, 400, 300));
    f.double_roundtrip(id);
    f.settle();
    f.double_roundtrip(id);

    // Settled: the effect covers the tile exactly.
    for s in sample_effect(&mut f) {
        assert_eq!(s.effect_geometry, s.tile_geometry);
        assert!(s
            .subregion_bbox
            .is_some_and(|r| rect_approx_eq(r, s.tile_geometry)));
    }

    // Hold the clock from here on. Everything below round-trips a client between the resize and
    // the samples, and each round trip runs the event loop, whose last act is to clear the lazy
    // clock — so without this the animation advances by however long those round trips take in
    // real time. That is a race against the machine: under load the 250 ms resize settled before
    // the `mid` sample and the precondition below tripped. Frozen, the sampled instant is the
    // `advance_clock` call and nothing else.
    f.freeze_clock();

    // The compositor decides on a new size before the client can answer.
    f.synoik_state()
        .do_action(synoik_config::Action::Maximize, false);
    f.double_roundtrip(id);
    for s in sample_effect(&mut f) {
        assert_eq!(
            s.effect_geometry, s.tile_geometry,
            "an unacked resize must not detach the effect from the tile",
        );
    }

    // The client answers with a bigger buffer and a matching region, in **one** commit — which is
    // what a real client does (verified on the wire against ghost). The resize animation is still
    // running, so the tile is between the two sizes here.
    //
    // It answers with the size it was actually configured with, rather than one of the test's
    // choosing. A client that commits an arbitrary size drags the tile's target to that size, so
    // the assertions below would be pinned to whatever number the test picked instead of to the
    // maximized geometry — and the mid-flight window they need would exist only while the two
    // happened to line up. `update_blur_region` carries the commit, so the buffer, the size, the
    // ack and the region all land together the way they do on the wire.
    let window = f.client(id).window(&surface);
    let (cw, ch) = window.configures_received.last().unwrap().1.size;
    assert!(
        cw > 400 && ch > 300,
        "the maximize must have configured a bigger size than the window started at, got {cw}x{ch}",
    );
    window.attach_solid_buffer(0, u32::MAX, 0, u32::MAX);
    window.set_size(u16::try_from(cw).unwrap(), u16::try_from(ch).unwrap());
    window.ack_last();
    f.client(id)
        .update_blur_region(&effect, &surface, (0, 0, cw, ch));
    f.double_roundtrip(id);

    // Step to a genuinely mid-flight instant: well inside the 250 ms window-resize easing
    // (`WindowResizeAnim`), so the tile has moved but has not arrived.
    f.advance_clock(Duration::from_millis(100));

    let mid = sample_effect(&mut f);
    assert!(!mid.is_empty(), "the effect must still render mid-resize");
    for s in &mid {
        assert!(
            s.surface_geo.size.w > s.tile_geometry.size.w,
            "precondition: the client should have outgrown the tile mid-animation \
             (surf {:?} vs tile {:?}) — if this trips, the animation settled early and the \
             test is no longer sampling the interesting frames",
            s.surface_geo.size,
            s.tile_geometry.size,
        );
        assert_eq!(
            s.effect_geometry, s.tile_geometry,
            "mid-resize the effect must follow the tile, not the client's committed size",
        );
        assert!(
            s.subregion_bbox
                .is_some_and(|r| rect_approx_eq(r, s.tile_geometry)),
            "the client's region must be scaled onto the tile, not left at buffer scale \
             (sub {:?} vs tile {:?})",
            s.subregion_bbox,
            s.tile_geometry,
        );
    }
}

#[test]
fn the_compositor_announces_the_blur_capability() {
    let mut f = Fixture::new();
    f.add_output(1, (1920, 1080));
    let id = f.add_client();
    f.roundtrip(id);

    assert_eq!(
        f.client(id).background_effect_capabilities(),
        Some(Capability::Blur),
    );
}

#[test]
fn a_committed_blur_region_reaches_the_renderer() {
    let mut f = Fixture::new();
    let (id, surface) = mapped_window(&mut f);

    f.client(id).set_blur_region(&surface, (10, 20, 100, 50));
    f.double_roundtrip(id);

    let rects = compositor_blur_rects(&mut f).expect("blur region should have reached the surface");
    assert_eq!(rects.len(), 1);
    assert_eq!(rects[0].loc, (10, 20).into());
    assert_eq!(rects[0].size, (100, 50).into());
}

#[test]
fn unsetting_the_blur_region_removes_it() {
    let mut f = Fixture::new();
    let (id, surface) = mapped_window(&mut f);

    let effect = f.client(id).set_blur_region(&surface, (0, 0, 100, 100));
    f.double_roundtrip(id);
    assert!(compositor_blur_rects(&mut f).is_some());

    // A NULL region removes the effect, on the next commit.
    effect.set_blur_region(None);
    surface.commit();
    f.double_roundtrip(id);

    assert!(
        compositor_blur_rects(&mut f).is_none(),
        "a NULL region must clear the blur, not leave the last one in the cache",
    );
}

/// A resize must damage everything the effect used to cover as well as everything it now covers.
///
/// This is the assertion the corpus structurally could not make before: with partial damage, any
/// pixel outside the reported damage keeps whatever the recycled buffer held, so an effect that
/// moves or resizes without damaging the ground it vacated leaves the old blur on screen — a glass
/// pane sitting where the window no longer is.
#[test]
fn a_resize_damages_the_ground_the_effect_vacated() {
    use smithay::backend::renderer::damage::OutputDamageTracker;
    use smithay::utils::Rectangle;

    use crate::render_helpers::vulkan::VulkanRenderer;

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
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    window.attach_solid_buffer(0, u32::MAX, 0, u32::MAX);
    window.set_size(600, 500);
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    let effect = f.client(id).set_blur_region(&surface, (0, 0, 600, 500));
    f.double_roundtrip(id);
    f.settle();
    f.double_roundtrip(id);

    let output = f.synoik_output(1);
    let mut tracker = OutputDamageTracker::from_output(&output);
    let scale = output.current_scale().fractional_scale();

    // Two settled frames, so the tracker has history and stops reporting full damage.
    let _ = sample_with_damage(&mut f, &mut tracker);
    let (_, before) = sample_with_damage(&mut f, &mut tracker);
    let old_effect = before
        .first()
        .expect("the effect must render before the resize")
        .effect_geometry;

    // Shrink: the effect gives up ground, which is the direction that can leave residue.
    let window = f.client(id).window(&surface);
    window.attach_solid_buffer(0, u32::MAX, 0, u32::MAX);
    window.set_size(300, 250);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.client(id)
        .update_blur_region(&effect, &surface, (0, 0, 300, 250));
    f.double_roundtrip(id);
    f.settle();
    f.double_roundtrip(id);

    let (damage, after) = sample_with_damage(&mut f, &mut tracker);
    let new_effect = after
        .first()
        .expect("the effect must still render after the resize")
        .effect_geometry;

    assert!(
        new_effect.size.w < old_effect.size.w,
        "precondition: the effect should have shrunk ({:?} -> {:?})",
        old_effect.size,
        new_effect.size,
    );

    // Every pixel the effect used to cover has to be repainted by somebody this frame.
    let vacated: Rectangle<i32, smithay::utils::Physical> =
        old_effect.to_physical_precise_round(scale);
    let uncovered = damage.iter().fold(vec![vacated], |acc, d| {
        acc.into_iter()
            .flat_map(|r| Rectangle::subtract_rect(r, *d))
            .collect()
    });

    assert!(
        uncovered.is_empty(),
        "the effect shrank from {old_effect:?} to {new_effect:?}, but {} sub-rect(s) of the \
         ground it vacated were never damaged: {uncovered:?}\ndamage was: {damage:?}",
        uncovered.len(),
    );
}

/// Does the framebuffer capture grab the rectangle the effect is being drawn at, or the one it was
/// at last frame?
///
/// The recording of the live bug shows the blurred *content* stopping short of the window's leading
/// edge during a drag while the effect's geometry tracks correctly — so the disagreement, if there
/// is one, is between the rect handed to `capture_framebuffer` and the sub-region actually blitted
/// out of the framebuffer. Nothing cheaper can see this: `damage_output` never calls the capture,
/// so the frame has to be really rendered.
#[test]
fn the_capture_grabs_the_rect_the_effect_is_drawn_at() {
    use smithay::backend::renderer::damage::OutputDamageTracker;
    use smithay::backend::renderer::Bind;
    use smithay::utils::{Physical, Size};

    use crate::render_helpers::background_effect::trace;
    use crate::render_helpers::vulkan::VulkanRenderer;
    use crate::render_helpers::{RenderCtx, RenderTarget, NATIVE_FOURCC};

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
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    window.attach_solid_buffer(0, u32::MAX, 0, u32::MAX / 2);
    window.set_size(400, 300);
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    let effect = f.client(id).set_blur_region(&surface, (0, 0, 400, 300));
    f.double_roundtrip(id);
    f.settle();
    f.double_roundtrip(id);

    let output = f.synoik_output(1);
    let size: Size<i32, Physical> = output.current_mode().unwrap().size;
    let mut tracker = OutputDamageTracker::from_output(&output);

    // One rendered frame, returning what the capture grabbed and where the effect was drawn.
    let mut frame = |f: &mut Fixture| -> (Vec<trace::CaptureSample>, Vec<trace::EffectSample>) {
        let _ = trace::take();
        let _ = trace::take_captures();
        let output = f.synoik_output(1);
        let state = f.synoik_state();
        state
            .backend
            .headless()
            .with_vulkan_renderer(|vk| {
                let synoik = &mut state.synoik;
                synoik.update_render_elements(Some(&output));
                let ctx = RenderCtx {
                    renderer: vk,
                    target: RenderTarget::Output,
                    appearance: Some(synoik.appearance()),
                };
                let elements = synoik.render_to_vec(ctx, &output, false);
                let mut texture = crate::render_helpers::create_texture(vk, size, NATIVE_FOURCC)
                    .expect("create offscreen");
                let mut fb = vk.bind(&mut texture).expect("bind offscreen");
                // Elements come back front-to-back; the tracker wants them in that same order.
                tracker
                    .render_output(vk, &mut fb, 1, &elements, [0., 0., 0., 1.])
                    .expect("render output");
            })
            .expect("the fixture must have a Vulkan renderer");
        (trace::take_captures(), trace::take())
    };

    let _ = frame(&mut f);
    let (c0, e0) = frame(&mut f);
    eprintln!("--- settled ---");
    for (c, e) in c0.iter().zip(e0.iter()) {
        eprintln!("  capture dst={:?} src={:?}", c.dst, c.src);
        eprintln!("  effect  geo={:?} blur={}", e.effect_geometry, e.blur);
    }

    // Grow the window a step at a time, the way a drag does. It has to keep moving: the capture
    // is a picture of what is behind the effect, so a frame where nothing changed is *correct* to
    // reuse the cached blur, and asserting on one asserts the absence of that optimization. A lag
    // between the captured rect and the drawn one only shows while the geometry is in motion.
    for (i, width) in [700u16, 750, 800].into_iter().enumerate() {
        let window = f.client(id).window(&surface);
        window.attach_solid_buffer(0, u32::MAX, 0, u32::MAX / 2);
        window.set_size(width, 300);
        // Plain commit, not an ack: the client is resizing itself, not answering a configure.
        window.commit();
        f.double_roundtrip(id);
        f.client(id)
            .update_blur_region(&effect, &surface, (0, 0, i32::from(width), 300));
        f.double_roundtrip(id);

        let (c, e) = frame(&mut f);
        eprintln!("--- frame {i} after growing to {width} ---");
        for s in &c {
            eprintln!("  capture dst={:?} src={:?}", s.dst, s.src);
        }
        for s in &e {
            eprintln!(
                "  effect  geo={:?} tile={:?}",
                s.effect_geometry, s.tile_geometry
            );
        }
        assert!(
            !c.is_empty(),
            "the capture must run on a real rendered frame"
        );
        for s in &c {
            assert_eq!(
                s.src, s.dst,
                "the capture must grab the rect the effect is drawn at",
            );
        }
    }
}

/// Moving a backdrop effect must re-capture, even when nothing behind it changed.
///
/// The tracker asks for a recapture only when damage from something *below* the element overlaps
/// it — an element's own damage is skipped (`element_damage_index` starts past its own z-index).
/// That is right for an ordinary element and wrong for one whose content is a picture of what sits
/// behind it: move it and it should be showing a different part of the desktop, but nothing in that
/// condition says so, so the cached blur from the old position is reused at the new one.
///
/// `CenterWindow` is the clean form of the case: the geometry's origin moves, the size does not,
/// the client commits nothing and the wallpaper below is untouched.
#[test]
fn moving_the_effect_recaptures_even_with_a_static_backdrop() {
    use smithay::backend::renderer::damage::OutputDamageTracker;

    use crate::render_helpers::vulkan::VulkanRenderer;

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
    f.add_output(1, (1280, 720));
    // Placement centers new windows by default, which would make the
    // `CenterWindow` action below a no-op and void this test's precondition.
    f.synoik().layout.set_gnome_center_new_windows(false);

    let id = f.add_client();
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    // Translucent on purpose: a client that asks for a backdrop blur is asking for the backdrop to
    // show through it. An *opaque* buffer would fully occlude the effect below it, and the damage
    // tracker culls fully-occluded elements before the framebuffer-effect scan ever sees them —
    // the effect would then never capture for a reason that has nothing to do with this test.
    let window = f.client(id).window(&surface);
    window.attach_solid_buffer(0, u32::MAX, 0, u32::MAX / 2);
    window.set_size(400, 300);
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    f.client(id).set_blur_region(&surface, (0, 0, 400, 300));
    f.double_roundtrip(id);
    f.settle();
    f.double_roundtrip(id);

    let output = f.synoik_output(1);
    let mut tracker = OutputDamageTracker::from_output(&output);

    // Settle: a few frames so the tracker has history and the caches are warm.
    let mut before = Vec::new();
    for _ in 0..3 {
        let (_, sample) = render_frame(&mut f, &mut tracker);
        if !sample.is_empty() {
            before = sample;
        }
    }
    let old_geo = before
        .first()
        .expect("the effect must render before the move")
        .effect_geometry;

    // The control that keeps this test honest. A settled frame with nothing happening must NOT
    // re-capture: if it does, the assertion below passes no matter what the tracker decides about
    // the move, and the test is decoration. This is not hypothetical — it was exactly that until
    // the panel stopped minting a fresh element `Id` every frame, which set `force_effect_redraw`
    // and made every effect on the output re-capture unconditionally
    // (`nothing_churns_its_element_id_per_frame`).
    let (idle, _) = render_frame(&mut f, &mut tracker);
    let idle_geo =
        old_geo.to_physical_precise_round(f.synoik_output(1).current_scale().fractional_scale());
    assert!(
        !idle.iter().any(|c| c.dst == idle_geo),
        "control failed: the effect re-captured on an idle frame, so this test cannot tell \
         whether *moving* it is what causes a re-capture.\ncaptures: {idle:?}",
    );

    // Move it. Nothing behind it changes; the client is not even told.
    // (The fixture turns `center-new-windows` off precisely so this action has
    // somewhere to move the window to.)
    f.synoik_state()
        .do_action(synoik_config::Action::CenterWindow, false);
    f.double_roundtrip(id);
    f.settle();
    f.double_roundtrip(id);

    let (captures, after) = render_frame(&mut f, &mut tracker);
    let new_geo = after
        .first()
        .expect("the effect must render after the move")
        .effect_geometry;

    assert_ne!(
        old_geo.loc, new_geo.loc,
        "precondition: the window should have moved",
    );

    let scale = f.synoik_output(1).current_scale().fractional_scale();
    let want = new_geo.to_physical_precise_round(scale);
    assert!(
        captures.iter().any(|c| c.dst == want),
        "the effect moved from {:?} to {:?} but never re-captured the framebuffer, so it is \
         still showing the backdrop from where it used to be.\ncaptures this frame: {:?}",
        old_geo.loc,
        new_geo.loc,
        captures,
    );
}

/// A client repainting its **own** surface must not make its backdrop re-capture and re-blur.
///
/// This is the whole cost argument for a blurred window on a desktop that is not idle. A terminal
/// running a spinner commits ~10 times a second, and Mesa's Vulkan WSI marks the *entire* surface
/// damaged on every present (measured on a real ghost session: 821 of 821 `damage_buffer` calls
/// were `0,0,INT32_MAX,INT32_MAX`). None of that changes what is *behind* the window, so the
/// captured backdrop is still valid and the gaussian chain must not run again. Measured on the
/// live seat, re-running it costs 0.250 ms of a 1.310 ms frame — 19%.
///
/// The sibling test `moving_the_effect_recaptures_even_with_a_static_backdrop` pins the opposite
/// direction, and its idle control pins the third case (nothing happens at all). This one is the
/// case in between, and it is the one a real session spends its time in: damage every frame,
/// none of it below the effect.
///
/// **The assertion is absolute, not relative.** "Captured no more often than some other frame"
/// passes when every frame re-captures, which is exactly the bug — the same trap that made a
/// previous cached-blur regression invisible to its own test. So this counts captures across a
/// run of foreground-only commits and requires *zero*.
#[test]
fn a_client_repainting_itself_does_not_recapture_its_backdrop() {
    use smithay::backend::renderer::damage::OutputDamageTracker;

    use crate::render_helpers::vulkan::VulkanRenderer;

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
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    // Translucent, for the same reason as the sibling test: an opaque buffer occludes the effect
    // and the tracker culls it before the framebuffer-effect scan runs, so the effect would never
    // capture for a reason that has nothing to do with what this test is about.
    let window = f.client(id).window(&surface);
    window.attach_solid_buffer(0, u32::MAX, 0, u32::MAX / 2);
    window.set_size(400, 300);
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    f.client(id).set_blur_region(&surface, (0, 0, 400, 300));
    f.double_roundtrip(id);
    f.settle();
    f.double_roundtrip(id);

    let output = f.synoik_output(1);
    let mut tracker = OutputDamageTracker::from_output(&output);

    // Warm the tracker and the caches, and confirm the effect is really there: a test that never
    // rendered the effect at all would count zero captures for the wrong reason.
    let mut seen_effect = false;
    for _ in 0..3 {
        let (_, effects) = render_frame(&mut f, &mut tracker);
        seen_effect |= !effects.is_empty();
    }
    assert!(
        seen_effect,
        "precondition: the effect never rendered, so counting its captures proves nothing",
    );

    // Now do what the client does: attach a new buffer, damage the whole surface, commit. Nothing
    // moves and nothing behind the window changes.
    let mut captures = Vec::new();
    for i in 0..5 {
        let window = f.client(id).window(&surface);
        // Alternate the colour so each commit really is new content, not a buffer the compositor
        // could recognise as identical.
        let g = if i % 2 == 0 { u32::MAX } else { u32::MAX / 2 };
        window.attach_solid_buffer(0, g, 0, u32::MAX / 2);
        window.damage_all();
        window.commit();
        f.double_roundtrip(id);

        let (frame_captures, _) = render_frame(&mut f, &mut tracker);
        captures.extend(frame_captures);
    }

    assert!(
        captures.is_empty(),
        "the client repainted only itself, but its backdrop re-captured and re-blurred {} \
         time(s). Nothing below the effect changed, so the cached capture was still valid and \
         the gaussian chain ran for nothing.\ncaptures: {captures:?}",
        captures.len(),
    );
}

/// Two overlapping blurred windows: repainting the front one must not recapture *either*
/// backdrop.
///
/// The single-window case is pinned above; this is the shape the live seat actually runs, and the
/// one that shows 4092 captures in 4651 frames. Two ghost terminals overlapping by roughly a third
/// of the output, only the front one animating.
///
/// Both directions matter and they fail differently. The front window's own damage must not reach
/// **its own** backdrop, which sits directly beneath it — that is a z-order question, and getting
/// the "damage from below" slice backwards would make every self-repaint recapture. And it must
/// not reach the **rear** window's backdrop either, which is further down still but whose region
/// the front window overlaps: a trigger keyed on plain rect overlap rather than on z-order would
/// fire there even with the slice the right way round.
#[test]
fn a_front_window_repainting_does_not_recapture_a_window_behind_it() {
    use smithay::backend::renderer::damage::OutputDamageTracker;

    use crate::render_helpers::vulkan::VulkanRenderer;

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
    f.add_output(1, (1280, 720));

    // Two blurred, translucent windows. Both are centered by the default placement, so they
    // overlap heavily — which is the point: the front one's damage covers the rear one's backdrop.
    let mut surfaces = Vec::new();
    for _ in 0..2 {
        let id = f.add_client();
        let window = f.client(id).create_window();
        let surface = window.surface.clone();
        window.commit();
        f.roundtrip(id);

        let window = f.client(id).window(&surface);
        window.attach_solid_buffer(0, u32::MAX, 0, u32::MAX / 2);
        window.set_size(400, 300);
        window.ack_last_and_commit();
        f.double_roundtrip(id);

        f.client(id).set_blur_region(&surface, (0, 0, 400, 300));
        f.double_roundtrip(id);
        f.settle();
        f.double_roundtrip(id);
        surfaces.push((id, surface));
    }

    let output = f.synoik_output(1);
    let mut tracker = OutputDamageTracker::from_output(&output);

    // Warm up, and require that *both* effects are really in the scene: if only one ever renders,
    // a zero-capture result says nothing about the pair.
    let mut most_effects = 0;
    for _ in 0..3 {
        let (_, effects) = render_frame(&mut f, &mut tracker);
        most_effects = most_effects.max(effects.len());
    }
    assert_eq!(
        most_effects, 2,
        "precondition: expected two blurred windows in the scene, saw {most_effects}",
    );

    // Repaint only the front window — the last one mapped, which is on top.
    let (front_id, front_surface) = surfaces.last().cloned().expect("two windows");
    let mut captures = Vec::new();
    for i in 0..5 {
        let window = f.client(front_id).window(&front_surface);
        let g = if i % 2 == 0 { u32::MAX } else { u32::MAX / 2 };
        window.attach_solid_buffer(0, g, 0, u32::MAX / 2);
        window.damage_all();
        window.commit();
        f.double_roundtrip(front_id);

        let (frame_captures, _) = render_frame(&mut f, &mut tracker);
        captures.extend(frame_captures);
    }

    assert!(
        captures.is_empty(),
        "only the front window repainted, but {} backdrop capture(s) ran. Its own damage is \
         above its backdrop, and above the window behind it, so neither backdrop's input \
         changed.\ncaptures: {captures:?}",
        captures.len(),
    );
}

/// The same self-repaint, rendered through a **cycling swapchain** instead of one reused buffer.
///
/// This is the variable the two tests above hold fixed and a real seat does not. They render into
/// a single offscreen at `age = 1`, so the damage a frame owes is always just that frame's. A seat
/// cycles three or four buffers, and a buffer last drawn three frames ago owes three frames of
/// damage — which puts *earlier* frames' damage, including the client's own previous repaints,
/// inside the window the effect's "did anything below me change" test looks at.
///
/// If a self-repaint recaptures here but not above, the age is the trigger and the fix belongs in
/// how the effect's damage window is computed, not in how the client behaves.
#[test]
fn a_self_repaint_does_not_recapture_across_a_cycling_swapchain() {
    use smithay::backend::renderer::damage::OutputDamageTracker;

    use crate::render_helpers::vulkan::VulkanRenderer;

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
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    window.attach_solid_buffer(0, u32::MAX, 0, u32::MAX / 2);
    window.set_size(400, 300);
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    f.client(id).set_blur_region(&surface, (0, 0, 400, 300));
    f.double_roundtrip(id);
    f.settle();
    f.double_roundtrip(id);

    let output = f.synoik_output(1);
    let mut tracker = OutputDamageTracker::from_output(&output);

    // Three slots, the shallow end of what a seat runs.
    const SLOTS: usize = 3;
    let mut buffers = Vec::new();
    let mut frame_no = 0u64;

    // Warm every slot, so no capture below can be blamed on a first-ever render into it.
    let mut seen_effect = false;
    for _ in 0..(SLOTS * 2) {
        frame_no += 1;
        let (_, effects) = render_frame_aged(&mut f, &mut tracker, &mut buffers, SLOTS, frame_no);
        seen_effect |= !effects.is_empty();
    }
    assert!(
        seen_effect,
        "precondition: the effect never rendered, so counting its captures proves nothing",
    );

    let mut captures = Vec::new();
    for i in 0..9 {
        let window = f.client(id).window(&surface);
        let g = if i % 2 == 0 { u32::MAX } else { u32::MAX / 2 };
        window.attach_solid_buffer(0, g, 0, u32::MAX / 2);
        window.damage_all();
        window.commit();
        f.double_roundtrip(id);

        frame_no += 1;
        let (frame_captures, _) =
            render_frame_aged(&mut f, &mut tracker, &mut buffers, SLOTS, frame_no);
        captures.extend(frame_captures);
    }

    assert!(
        captures.is_empty(),
        "with {SLOTS} swapchain slots the client's own repaints caused {} backdrop capture(s), \
         though the same sequence into a single buffer causes none. The effect's damage window is \
         picking up frames of damage that are only being replayed to heal an older \
         buffer.\ncaptures: {captures:?}",
        captures.len(),
    );
}

/// A blur region set on a **subsurface** must blur that subsurface's own backdrop.
///
/// `docs/fork/client-blur.md` §5 gap 4. The protocol lets a client call `get_background_effect` on
/// any `wl_surface`, and a GTK/Qt client with blurred chrome on a subsurface does exactly that. We
/// used to accept the request, cache the region, and never look at it: effects resolved for the
/// toplevel's surface only, with a separate loop for popups, and the surface tree was never walked.
///
/// The effect has to land *directly beneath the subsurface that asked for it*, not under the whole
/// window — that is what makes its backdrop "everything below me, my own parent surface included",
/// which is what such chrome wants. So this asserts placement, not merely presence.
#[test]
fn a_subsurface_blur_region_blurs_that_subsurface() {
    use crate::render_helpers::vulkan::VulkanRenderer;

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
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    let window = f.client(id).create_window();
    let parent = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&parent);
    window.attach_solid_buffer(0, u32::MAX, 0, u32::MAX / 2);
    window.set_size(400, 300);
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    // Blurred chrome on a subsurface, inset from the parent so its region cannot be mistaken for
    // the toplevel's own. *Two* of them, at different offsets: the effect's absolute origin is not
    // checkable against anything the test already knows, but the distance between two of them is,
    // and it is exactly what a mis-scaled or double-counted offset gets wrong. One subsurface hid a
    // bug where the surface's own `view.offset` was folded in twice — the effect landed a whole
    // subsurface-offset past the subsurface, and every self-relative assertion still passed.
    let (child, _sub) =
        f.client(id)
            .create_subsurface(&parent, 40, 40, 200, 120, [0, 0, u32::MAX, u32::MAX / 2]);
    f.client(id).set_blur_region(&child, (0, 0, 200, 120));
    let (child2, _sub2) =
        f.client(id)
            .create_subsurface(&parent, 140, 90, 200, 120, [0, 0, u32::MAX, u32::MAX / 2]);
    f.client(id).set_blur_region(&child2, (0, 0, 200, 120));
    // A synchronized subsurface only takes effect when the parent commits.
    f.client(id).window(&parent).commit();
    f.double_roundtrip(id);
    f.settle();
    f.double_roundtrip(id);

    // Half one: the protocol seam works. Walk the compositor's surface tree for the window and
    // find a cached blur region on something that is not the toplevel itself.
    let mapped = f.synoik().layout.windows().next().unwrap().1;
    let root = mapped.toplevel().wl_surface().clone();
    let mut on_a_child = false;
    smithay::wayland::compositor::with_surface_tree_downward(
        &root,
        (),
        |_, _, _| smithay::wayland::compositor::TraversalAction::DoChildren(()),
        |surface, states, _| {
            if *surface != root && get_cached_blur_region(states).is_some_and(|r| !r.is_empty()) {
                on_a_child = true;
            }
        },
        |_, _, _| true,
    );
    assert!(
        on_a_child,
        "the subsurface's blur region never reached the compositor's cache — that would be a \
         protocol-seam bug, not the render-path gap this test is about",
    );

    // Half two: an effect resolves for each, at the subsurface's own rect rather than the window's.
    let samples = sample_effect(&mut f);
    let mut subs: Vec<_> = samples
        .iter()
        .filter(|s| s.subregion_bbox.is_some())
        .collect();
    assert_eq!(
        subs.len(),
        2,
        "expected one effect per blurred subsurface, got {}: {samples:?}",
        subs.len(),
    );
    subs.sort_by(|a, b| a.effect_geometry.loc.x.total_cmp(&b.effect_geometry.loc.x));

    // The two subsurfaces are 100x50 apart, so their effects must be too. A doubled offset puts
    // them 200x100 apart; a dropped one puts them on top of each other.
    let delta = subs[1].effect_geometry.loc - subs[0].effect_geometry.loc;
    assert!(
        (delta.x - 100.).abs() < 1e-6 && (delta.y - 50.).abs() < 1e-6,
        "the two subsurfaces are 100x50 apart but their effects are {delta:?} apart — the \
         subsurface offset is being scaled or counted the wrong number of times",
    );

    let sub = subs[0];

    assert!(
        sub.effect_geometry.size.w >= 200. && sub.effect_geometry.size.h >= 120.,
        "the effect covers {:?}, smaller than the 200x120 subsurface — it was sized from something \
         other than the subsurface's own view",
        sub.effect_geometry.size,
    );
    let bbox = sub.subregion_bbox.unwrap();
    assert!(
        rect_approx_eq(
            bbox,
            smithay::utils::Rectangle::new(sub.effect_geometry.loc, (200., 120.).into()),
        ),
        "the blur region landed at {bbox:?}, not on the subsurface at {:?}",
        sub.effect_geometry.loc,
    );
    // The client's blur region is what turns the effect on at all — without it the subsurface
    // would draw no backdrop.
    assert!(
        sub.blur,
        "the subsurface effect resolved to blur={}, so it would draw no backdrop at all",
        sub.blur,
    );
}

/// Flipping `org.gnome.desktop.interface color-scheme` must repaint every blurred surface, with no
/// window damage and nothing else changing.
///
/// This is the whole reason the appearance is resolved into `Options` — which
/// `update_render_elements` compares to decide whether to `damage_all()` — instead of being read at
/// draw time. Read at draw time the new tint would be correct and *invisible*: `draw` only runs
/// where the tracker says the output is damaged, and on a static desktop with a settled window that
/// is nowhere. The window would keep its old tint until something unrelated happened to damage it.
///
/// The idle control is not optional, for the same reason it is not optional in
/// `moving_the_effect_recaptures_even_with_a_static_backdrop`: if a settled frame re-captures
/// anyway, the assertion below passes whatever the flip does.
#[test]
fn a_color_scheme_flip_redraws_a_blurred_surface() {
    use smithay::backend::renderer::damage::OutputDamageTracker;

    use crate::render_helpers::vulkan::VulkanRenderer;

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
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    // Translucent: an opaque buffer occludes the effect and the tracker culls it before the
    // framebuffer-effect scan, which would make this test pass for the wrong reason.
    let window = f.client(id).window(&surface);
    window.attach_solid_buffer(0, u32::MAX, 0, u32::MAX / 2);
    window.set_size(400, 300);
    window.ack_last_and_commit();
    f.double_roundtrip(id);

    f.client(id).set_blur_region(&surface, (0, 0, 400, 300));
    f.double_roundtrip(id);
    f.settle();
    f.double_roundtrip(id);

    let output = f.synoik_output(1);
    let mut tracker = OutputDamageTracker::from_output(&output);

    let mut geo = None;
    for _ in 0..3 {
        let (_, sample) = render_frame(&mut f, &mut tracker);
        if let Some(s) = sample.first() {
            geo = Some(s.effect_geometry);
        }
    }
    let geo = geo
        .expect("the effect must render before the flip")
        .to_physical_precise_round(f.synoik_output(1).current_scale().fractional_scale());

    // Control: settled, nothing happening, no re-capture.
    let (idle, _) = render_frame(&mut f, &mut tracker);
    assert!(
        !idle.iter().any(|c| c.dst == geo),
        "control failed: the effect re-captured on an idle frame, so this test cannot tell \
         whether the color-scheme flip is what causes the redraw.\ncaptures: {idle:?}",
    );

    // The flip. The client is not told, nothing is committed, no geometry moves.
    assert!(
        !f.synoik_state()
            .synoik
            .gnome_settings
            .quick_toggles
            .dark_style,
        "the fixture should start light, or this flips nothing",
    );
    f.synoik_state()
        .synoik
        .gnome_settings
        .quick_toggles
        .dark_style = true;

    let (captures, _) = render_frame(&mut f, &mut tracker);
    assert!(
        captures.iter().any(|c| c.dst == geo),
        "a color-scheme flip must redraw the blurred surface — its tint is appearance-dependent, \
         and nothing else will ever damage a settled window.\nwanted dst {geo:?}, \
         captures: {captures:?}",
    );
}
