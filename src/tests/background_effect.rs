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
                xray: None,
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
                xray: None,
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
    f.synoik_complete_animations();
    f.double_roundtrip(id);

    // Settled: the effect covers the tile exactly.
    for s in sample_effect(&mut f) {
        assert_eq!(s.effect_geometry, s.tile_geometry);
        assert!(s
            .subregion_bbox
            .is_some_and(|r| rect_approx_eq(r, s.tile_geometry)));
    }

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

    // The client answers with a bigger buffer and a matching region, in one commit — which is what
    // a real client does (verified on the wire against ghost). The resize animation is still
    // running, so the tile is between the two sizes here.
    let window = f.client(id).window(&surface);
    window.attach_solid_buffer(0, u32::MAX, 0, u32::MAX);
    window.set_size(800, 600);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    f.client(id)
        .update_blur_region(&effect, &surface, (0, 0, 800, 600));
    f.double_roundtrip(id);

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
    f.synoik_complete_animations();
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
    f.synoik_complete_animations();
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
