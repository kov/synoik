// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Dropping a compositor must drop everything it owns.
//!
//! Each test here builds a [`Fixture`], drives it into some state, drops it, and asserts that a
//! long-lived object went with it. They exist because the suite runs ~1800 compositors in one
//! process: anything a teardown misses is multiplied by that, and the first symptom is the whole
//! binary dying on `EMFILE` a thousand tests in, blaming whichever test happened to be running.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use synoik_config::Config;

use super::fixture::Fixture;
use super::scrolling;

/// Sets its flag when dropped, so a test can ask whether whatever owns it was itself dropped.
struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// The seat must not outlive the compositor, even with a window focused at teardown.
///
/// Focusing a surface used to leave a reference cycle behind it: smithay's keyboard held the
/// focused `WlSurface`, and the destruction hook it installed on that surface captured the seat
/// *strongly* (`wayland/seat/keyboard.rs`, `enter_internal`). `leave` breaks the cycle by removing
/// the hook, so this only bit a compositor dropped while something still had focus — which is
/// every test that maps a window. Each one orphaned a seat, and with it an xkb context, a compiled
/// keymap and the keymap's memfd. ~1300 tests in, the process ran out of file descriptors.
///
/// A drop sentinel rather than a descriptor count on purpose: descriptors are process-wide, and
/// this binary runs its tests in parallel.
#[test]
fn dropping_the_compositor_drops_the_seat_even_with_a_focused_window() {
    let dropped = Arc::new(AtomicBool::new(false));

    let mut f = Fixture::with_config(scrolling(Config::default()));
    f.add_output(1, (1280, 720));

    let id = f.add_client();
    f.roundtrip(id);

    let client = f.client(id);
    let window = client.create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    // Map it: mapping is what focuses the window, and focus is what installed the hook.
    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    assert!(
        f.synoik().layout.windows().next().is_some(),
        "the window should have mapped, or this test is not exercising focus at all"
    );

    // The sentinel lives in the seat's own user data, so it drops exactly when the seat does.
    let flag = DropFlag(dropped.clone());
    f.synoik().seat.user_data().insert_if_missing(|| flag);

    drop(f);

    assert!(
        dropped.load(Ordering::SeqCst),
        "the seat outlived the compositor, so something still holds it — every test that maps a \
         window leaks one, along with its keymap file descriptor"
    );
}
