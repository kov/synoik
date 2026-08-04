use std::fmt::Write as _;
use std::time::Duration;

use insta::assert_snapshot;
use smithay::utils::{Point, Size};
use synoik_config::animations::{Curve, EasingParams, Kind};
use synoik_config::Config;
use synoik_ipc::SizeChange;
use wayland_client::protocol::wl_surface::WlSurface;

use super::client::ClientId;
use super::*;
use crate::synoik::Synoik;

fn format_tiles(synoik: &Synoik) -> String {
    let mut buf = String::new();
    let ws = synoik.layout.active_workspace().unwrap();
    let mut tiles: Vec<_> = ws.tiles_with_render_positions().collect();

    // We sort by id since that gives us a consistent order (from first opened to last), but we
    // don't print the id since it's nondeterministic (the id is a global counter across all
    // running tests in the same binary).
    tiles.sort_by_key(|(tile, _, _)| tile.window().id().get());
    for (tile, pos, _visible) in tiles {
        let Size { w, h, .. } = tile.animated_tile_size();
        let Point { x, y, .. } = pos;
        writeln!(&mut buf, "{w:>3.0} × {h:>3.0} at x:{x:>3.0} y:{y:>3.0}").unwrap();
    }
    buf
}

fn create_window(f: &mut Fixture, id: ClientId, w: u16, h: u16) -> WlSurface {
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    let window = f.client(id).window(&surface);
    window.attach_new_buffer();
    window.set_size(w, h);
    window.ack_last_and_commit();
    f.roundtrip(id);

    surface
}

fn set_time(synoik: &mut Synoik, time: Duration) {
    // This is a bit involved because we're dealing with an AdjustableClock that maintains its own
    // internal current_time.

    // First, reset current_time to zero by matching unadjusted time to it (at rate 0.0), then
    // setting unadjusted time to zero at rate 1.0 (causing current_time to also go to zero).
    let now = synoik.clock.now();
    synoik.clock.set_unadjusted(now);
    let _ = synoik.clock.now();
    synoik.clock.set_unadjusted(Duration::ZERO);
    synoik.clock.set_rate(1.0);
    let _ = synoik.clock.now();

    // Now, set the desired time at rate 1.0.
    synoik.clock.set_unadjusted(time);
    let _ = synoik.clock.now();

    // Freeze the clock so that clear() inside the synoik loop callback followed by some get()
    // doesn't replace it with the monotonic time.
    synoik.clock.set_rate(0.0);
}

// Sets up a fixture with linear animations, a renderer, and an output.
fn set_up() -> Fixture {
    const LINEAR: Kind = Kind::Easing(EasingParams {
        duration_ms: 1000,
        curve: Curve::Linear,
    });

    let mut config = scrolling(Config::default());
    config.layout.gaps = 0.0;
    config.animations.window_resize.anim.kind = LINEAR;
    config.animations.window_movement.0.kind = LINEAR;

    let mut f = Fixture::with_config(config);
    f.synoik_state().backend.headless().add_renderer().unwrap();
    f.add_output(1, (1920, 1080));

    f
}

fn set_up_two_in_column() -> (Fixture, ClientId, WlSurface, WlSurface) {
    let mut f = set_up();

    let id = f.add_client();

    let surface1 = create_window(&mut f, id, 100, 100);
    let surface2 = create_window(&mut f, id, 200, 200);
    f.double_roundtrip(id);

    let _ = f.client(id).window(&surface1).recent_configures();
    let _ = f.client(id).window(&surface2).recent_configures();

    // Consume into one column.
    f.synoik().layout.focus_left();
    f.synoik().layout.consume_into_column();
    f.double_roundtrip(id);

    // Commit for the column consume.
    let window = f.client(id).window(&surface1);
    window.ack_last_and_commit();

    let window = f.client(id).window(&surface2);
    window.ack_last_and_commit();

    f.double_roundtrip(id);

    set_time(f.synoik(), Duration::ZERO);
    f.synoik_complete_animations();

    (f, id, surface1, surface2)
}

#[test]
fn egl_height_resize_animates_next_y() {
    let (mut f, id, surface1, surface2) = set_up_two_in_column();

    // Issue a resize.
    f.synoik()
        .layout
        .set_window_height(None, SizeChange::AdjustFixed(-50));
    f.double_roundtrip(id);

    // The top window shrinks in response, the bottom remains as is.
    let window = f.client(id).window(&surface1);
    window.set_size(100, 50);
    window.ack_last_and_commit();
    let window = f.client(id).window(&surface2);
    window.ack_last_and_commit();

    // This starts the resize animation for the top window and the Y move for the bottom.
    f.roundtrip(id);

    // No time had passed yet, so we're at the initial state.
    assert_snapshot!(format_tiles(f.synoik()), @r"
    100 × 100 at x:  0 y:  0
    200 × 200 at x:  0 y:100
    ");

    // Advance the time halfway.
    set_time(f.synoik(), Duration::from_millis(500));
    f.synoik().advance_animations();

    // Top window is half-resized at 75 px tall, bottom window is at y=75 matching it.
    assert_snapshot!(format_tiles(f.synoik()), @r"
    100 ×  75 at x:  0 y:  0
    200 × 200 at x:  0 y: 75
    ");

    // Advance the time to completion.
    set_time(f.synoik(), Duration::from_millis(1000));
    f.synoik().advance_animations();

    // Final state at 50 px.
    assert_snapshot!(format_tiles(f.synoik()), @r"
    100 ×  50 at x:  0 y:  0
    200 × 200 at x:  0 y: 50
    ");
}

#[test]
fn egl_clientside_height_change_doesnt_animate() {
    let (mut f, id, surface1, _surface2) = set_up_two_in_column();

    // The initial state.
    assert_snapshot!(format_tiles(f.synoik()), @r"
    100 × 100 at x:  0 y:  0
    200 × 200 at x:  0 y:100
    ");

    // The top window shrinks by itself, without a synoik-issued resize.
    let window = f.client(id).window(&surface1);
    window.set_size(100, 50);
    window.commit();

    // This does not start any animations.
    f.roundtrip(id);

    // No time had passed yet, but we are at the final state right away.
    assert_snapshot!(format_tiles(f.synoik()), @r"
    100 ×  50 at x:  0 y:  0
    200 × 200 at x:  0 y: 50
    ");
}

/// Headless has no VBlank, so nothing outside the backend ever asks for the *next* frame of an
/// animation: a redraw leaves the output idle, and only fresh damage brings it back. Without the
/// backend's own frame timer an animation therefore renders exactly one frame and sits there —
/// invisible to every test that steps the clock by hand, and the reason a headless screencast
/// delivered a single frame and then nothing (`tools/headless-cast`).
///
/// Driving the real event loop is the point: `frame_callback_sequence` advances once per headless
/// render, so more than one of them means the loop re-armed itself.
#[test]
fn the_headless_redraw_loop_keeps_an_animation_going() {
    let (mut f, id, surface1, surface2) = set_up_two_in_column();

    // Start the resize animation (1000 ms, and the clock is frozen, so it stays unfinished).
    f.synoik()
        .layout
        .set_window_height(None, SizeChange::AdjustFixed(-50));
    f.double_roundtrip(id);
    let window = f.client(id).window(&surface1);
    window.set_size(100, 50);
    window.ack_last_and_commit();
    let window = f.client(id).window(&surface2);
    window.ack_last_and_commit();
    f.roundtrip(id);

    let output = f.synoik_output(1);
    let before = f.synoik().output_state[&output].frame_callback_sequence;
    f.synoik().queue_redraw(&output);

    let kept_going = f.dispatch_until(Duration::from_millis(500), |state| {
        state.synoik.output_state[&output]
            .frame_callback_sequence
            .wrapping_sub(before)
            >= 3
    });
    assert!(
        kept_going,
        "the headless backend must ask for the next frame while animations are unfinished"
    );
}
