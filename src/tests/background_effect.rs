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
