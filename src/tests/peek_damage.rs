// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! What a **settled** workspace peek costs when nothing beneath it is changing.
//!
//! The live seat says: with the peek down a frame draws 0.92x the output; with it up, 5.37x (p95
//! 10.5x), 3.6x the GPU time, and 29% of vblanks go by with nothing to present — which reads,
//! from a client's side, as vkcube slowing down. The seat's own explanation for the extra
//! coverage was that the peek turns the workspace cull off (`monitor.rs`, `strip_is_peek`), so
//! every workspace's elements are updated instead of just the active one.
//!
//! That explains the *element count*. It does not explain the *cost*, and the difference is the
//! whole point of this file: on the seat where this was seen, the peeked workspace held a static
//! terminal and the strip was nowhere near the animating window. Nothing beneath the strip was
//! changing. A frame that repaints a screen where nothing changed is a damage defect, not a price
//! of translucency — and the peek being on screen is not a licence to charge for it.
//!
//! Two defects hide in one symptom, and they are separable:
//!
//! 1. **A redraw is queued at all.** Headless only renders when something damages the output — "a
//!    redraw leaves the output `Idle` and only new damage brings it back"
//!    ([`crate::backend::headless`]). So the frame count of a still second *is* the answer to "is
//!    something asking for a repaint", with no instrument of its own.
//! 2. **What a redraw costs once one is due.** Two quantities, and conflating them is the trap this
//!    file was nearly built on: **damage** is the region the screen is asked to repaint,
//!    **overdraw** is how many fragments get shaded inside it. The seat's 5.37x is overdraw. They
//!    can move independently — 80 extra thumbnail elements shade the same damaged region over and
//!    over — so both are reported, per frame, side by side.
//!
//! Both are read off what the compositor actually did: the damage comes from the tracker
//! `render_element_states` consulted (`Headless::damage_log`), and the fragments from a real
//! damaged render of the frame's own element list through
//! [`Headless::frame_sink`](crate::backend::headless::Headless). A probe that runs a tracker of
//! its own is asserting about its own copy, and the fork's damage bugs have twice lived in
//! exactly that gap.
//!
//! Counters, not wall clock: they are exact, reproducible, and they are the quantity that is
//! wrong. [`perf_probe`](super::perf_probe) is where the timings live.
//!
//! Run the instrument: `cargo test --workspace peek_damage -- --nocapture --ignored`
//!
//! The **control comes first and is not optional**. "A settled frame repaints nothing" has to
//! hold with the peek *down*, in this harness, before it means anything with the peek up:
//! headless renders nothing persistent and every capture path re-renders with full damage, so a
//! probe wired to the wrong seam produces a full repaint every frame and can never fail-to-pass.
//! [`the_control_a_settled_scene_repaints_nothing`] is that check, and it is a real test rather
//! than a printed row for the same reason.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use smithay::utils::{Logical, Physical, Point, Rectangle, Size};
use synoik_config::Action;
use wayland_client::protocol::wl_surface::WlSurface;

use super::client::ClientId;
use super::fixture::Fixture;
use super::gnome::{draw_into, pointer_motion_to, summon_peek, WarmTarget};
use crate::backend::headless::FrameDamage;
use crate::render_helpers::vulkan::VulkanRenderer;

/// The internal panel's shape, i.e. where the live reading was taken.
const OUT: (u16, u16) = (2048, 1330);

/// How long a "nothing is happening" window lasts. Long enough that a per-vblank repaint would be
/// unmistakable (~60 frames) and short enough to stay in the suite's budget.
const STILL: Duration = Duration::from_secs(1);

/// The window on the *active* workspace: the one that stands in for vkcube, repainting a corner of
/// itself while nothing else in the scene moves.
struct Poke {
    client: ClientId,
    surface: WlSurface,
}

/// A scene with `windows` windows spread one per workspace, settled, with a renderer attached.
///
/// One per workspace because the strip is what is under test: a peek puts every workspace on
/// screen as a thumbnail, and a strip of empty thumbnails is a scene the defect cannot happen in.
/// `None` when the machine has no Vulkan device — the element pass needs one
/// (`render_element_states` returns early without it), so there is no frame to read.
fn build(windows: usize) -> Option<(Fixture, Poke)> {
    if let Err(e) = VulkanRenderer::new() {
        eprintln!("skipping: no Vulkan device ({e})");
        return None;
    }
    synoik_vk::stats::set_enabled(true);

    let mut f = Fixture::new();
    f.synoik_state()
        .backend
        .headless()
        .add_renderer()
        .expect("build the Vulkan renderer");
    f.add_output(1, OUT);

    let mut poke = None;
    for i in 0..windows {
        let id = f.add_client();
        let window = f.client(id).create_window();
        let surface = window.surface.clone();
        window.commit();
        f.roundtrip(id);

        // A real shm buffer, not a solid: a solid never samples a texture, and a thumbnail is that
        // same texture minified — which is the shape the strip draws in.
        let window = f.client(id).window(&surface);
        window.attach_shm_buffer(WIN.0, WIN.1, 200, 100, 50, 255);
        window.set_size(WIN.0 as u16, WIN.1 as u16);
        window.ack_last_and_commit();
        f.double_roundtrip(id);

        // Park each on its own workspace, except the last: it stays on the active one, so the
        // scene under the strip is a real window rather than bare wallpaper.
        if i + 1 < windows {
            f.synoik_state()
                .do_action(Action::MoveWindowToWorkspaceDown(true), false);
            f.synoik_complete_animations();
        } else {
            poke = Some(Poke {
                client: id,
                surface,
            });
        }
    }

    f.settle();
    Some((f, poke.expect("at least one window")))
}

/// A window big enough that its texture must be minified hard to fit a thumbnail.
const WIN: (i32, i32) = (1600, 1000);

/// What a window of frames cost.
#[derive(Default)]
struct Cost {
    /// Frames the compositor decided to render. Defect 1: on a settled scene this should be 0.
    frames: usize,
    /// Of those, how many asked the screen to repaint anything at all.
    damaging: usize,
    /// Damage summed over the window, as a multiple of the output. Counts the tracker's own rects,
    /// since each one is a repaint that was asked for.
    damage: f64,
    /// Fragments shaded rendering those frames, as a multiple of the output — the frame log's
    /// `overdraw`, and the number the live reading is in.
    overdraw: f64,
    /// Draw calls issued.
    draws: u64,
}

impl Cost {
    fn per_frame(&self) -> (f64, f64, f64) {
        let n = self.frames.max(1) as f64;
        (self.damage / n, self.overdraw / n, self.draws as f64 / n)
    }
}

/// Runs `body` with both instruments recording, and reports what the frames in between cost.
///
/// The sink renders each frame's own element list into a warm target through a persistent damage
/// tracker, which is what makes the fragment count mean anything: a full-output render every frame
/// would report the same overdraw whatever the damage was, and the whole question is whether the
/// peek is charging for pixels that did not change.
fn measure(f: &mut Fixture, body: impl FnOnce(&mut Fixture)) -> Cost {
    let output = f.synoik_output(1);
    let name = output.name();
    let size: Size<i32, Physical> = output.current_mode().expect("the output has a mode").size;

    let counted = Rc::new(RefCell::new((0u64, 0u64)));
    {
        let counted = counted.clone();
        let mut warm = WarmTarget::new(&output, 1);
        f.synoik_state().backend.headless().frame_sink =
            Some(Box::new(move |vk, _output, elements| {
                let (d0, s0) = (synoik_vk::stats::draws(), synoik_vk::stats::shaded());
                draw_into(vk, &mut warm, size, elements, false);
                let mut c = counted.borrow_mut();
                c.0 += synoik_vk::stats::draws() - d0;
                c.1 += synoik_vk::stats::shaded() - s0;
            }));
    }
    // Warm the pipelines and let the tracker take a first, necessarily-full frame, then start the
    // ledgers: the first render into a fresh target is a full repaint by construction and would
    // otherwise be counted as the peek's doing.
    f.synoik().queue_redraw_all();
    f.dispatch();
    *counted.borrow_mut() = (0, 0);
    f.synoik_state().backend.headless().damage_log = Some(Vec::new());

    body(f);

    let log: Vec<FrameDamage> = f
        .synoik_state()
        .backend
        .headless()
        .damage_log
        .take()
        .expect("recording was started just above");
    f.synoik_state().backend.headless().frame_sink = None;
    let (draws, shaded) = *counted.borrow();

    let out_px = f64::from(OUT.0) * f64::from(OUT.1);
    let mine: Vec<&FrameDamage> = log.iter().filter(|d| d.output == name).collect();
    Cost {
        frames: mine.len(),
        damaging: mine.iter().filter(|d| d.area() > 0).count(),
        damage: mine.iter().map(|d| d.area() as f64).sum::<f64>().max(0.) / out_px,
        overdraw: shaded as f64 / out_px,
        draws,
    }
}

/// Hold the scene still for [`STILL`], running `nudge` each frame.
///
/// No client commits in here at all: whatever this records is the compositor's own doing. `nudge`
/// exists for the arms whose whole subject is input arriving while nothing else changes.
fn still(f: &mut Fixture, mut nudge: impl FnMut(&mut Fixture)) -> Cost {
    const FRAME: Duration = Duration::from_micros(16_667);

    measure(f, |f| {
        f.freeze_clock();
        let mut elapsed = Duration::ZERO;
        while elapsed < STILL {
            f.advance_clock(FRAME);
            nudge(f);
            f.dispatch();
            f.refresh();
            elapsed += FRAME;
        }
    })
}

/// Repaint `side`x`side` pixels of the active workspace's window, `times` over.
///
/// This is the live report in miniature. kov's scene was one animating window on the active
/// workspace, a static terminal beside it, and the strip far away — so the honest cost of a frame
/// is that window's damage and nothing else, peek or no peek. The strip is not moving either.
fn poke(f: &mut Fixture, p: &Poke, side: i32, times: usize) -> Cost {
    poke_nudging(f, p, side, times, |_| {})
}

/// [`poke`], with `nudge` run each frame before the compositor gets its turn.
///
/// Pointer motion has to be measured *here* rather than on a still scene, and the reason is a
/// finding in its own right: moving the pointer queues no redraw at all (pinned by
/// [`the_control_pointer_motion_queues_no_redraw`]). On a live seat that is invisible, because
/// clients are committing and frames are coming anyway — so what motion actually does is make the
/// frames that already exist more expensive. A still scene cannot show that; a scene with a client
/// repainting can.
fn poke_nudging(
    f: &mut Fixture,
    p: &Poke,
    side: i32,
    times: usize,
    mut nudge: impl FnMut(&mut Fixture),
) -> Cost {
    const FRAME: Duration = Duration::from_micros(16_667);

    measure(f, |f| {
        f.freeze_clock();
        for _ in 0..times {
            nudge(f);
            {
                // A real client's repaint: a fresh buffer, with only the corner reported changed.
                // Damage with no attach is not a repaint — the tracker reads the element's commit
                // counter, which a bufferless commit does not move, so such frames report no
                // damage at all and the measurement is of nothing.
                let window = f.client(p.client).window(&p.surface);
                window.attach_shm_buffer_damaging(
                    WIN.0,
                    WIN.1,
                    [200, 100, 50, 255],
                    (0, 0, side, side),
                );
                window.commit();
            }
            f.roundtrip(p.client);
            f.advance_clock(FRAME);
            f.dispatch();
            f.refresh();
        }
    })
}

/// Every thumbnail's rect on the peeked strip, in the strip's own order.
fn thumbnails(f: &mut Fixture) -> Vec<Rectangle<f64, Logical>> {
    let output = f.synoik_output(1);
    let ids: Vec<_> = f
        .synoik()
        .layout
        .workspaces()
        .filter(|(mon, _, _)| mon.is_some_and(|m| m.output() == &output))
        .map(|(_, _, ws)| ws.id())
        .collect();
    let monitor = f
        .synoik()
        .layout
        .monitor_for_output(&output)
        .expect("the output has a monitor");
    ids.into_iter()
        .filter_map(|id| monitor.thumbnail_rect_for(id))
        .collect()
}

fn center(rect: Rectangle<f64, Logical>) -> Point<f64, Logical> {
    Point::from((rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.))
}

/// **The control.** A settled scene with the peek down must render nothing at all.
///
/// This is the anti-vacuity guard for everything else in this file. If the harness renders every
/// tick regardless — which is what a probe reading the wrong seam, or compositing on its own
/// schedule, would see — then "the peek repaints the screen" is a claim the instrument would make
/// about a still desktop too, and none of the peek rows would mean anything.
/// **The second control.** Moving the pointer reaches the compositor, and queues no redraw.
///
/// The motion arms came back at exactly zero frames on a still scene, and a counter that hits
/// precisely zero has been a blind instrument every previous time in this fork — so it is pinned
/// here rather than believed. Both halves matter: the pointer really moves (or the arms inject
/// nothing), and no frame follows (which is why motion is measured on top of a client repaint, not
/// on a still scene — see [`poke_nudging`]).
///
/// Not a defect. Nothing on screen has changed yet: the cursor is not in the element list this
/// backend assembles, and the strip's hover chrome is recomputed when a frame is next built. On a
/// seat, frames are always coming, so motion never has to ask for one. It only makes the frames
/// that were coming anyway cost more, and that is the thing worth measuring.
#[test]
fn the_control_pointer_motion_queues_no_redraw() {
    let Some((mut f, _)) = build(4) else { return };

    let before = f.synoik().seat.get_pointer().unwrap().current_location();
    let moved = still(&mut f, |f| {
        f.pointer_motion(3., 0.);
    });
    let after = f.synoik().seat.get_pointer().unwrap().current_location();

    assert_ne!(
        before, after,
        "the pointer never moved, so this probe's motion arms are injecting nothing"
    );
    assert_eq!(
        moved.frames, 0,
        "pointer motion now queues redraws ({} of them). That is a change in what this backend          costs while the mouse moves, and the motion arms in this file — which ride on a client's          frames precisely because motion produced none — are measuring something else now",
        moved.frames,
    );
}

#[test]
fn the_control_a_settled_scene_repaints_nothing() {
    let Some((mut f, _)) = build(4) else { return };

    let quiet = still(&mut f, |_| {});
    assert_eq!(
        quiet.frames, 0,
        "a settled scene with nothing animating and no client committing rendered {} frames \
         ({:.2}x the output damaged): headless only renders on damage, so either something is \
         damaging a still output or this probe is not reading the frames the compositor chose",
        quiet.frames, quiet.damage,
    );
}

/// The instrument. Prints what the arms that separate the two defects cost; `#[ignore]`d because
/// its job is to be read, and because promoting a row to an assertion needs a number this has
/// actually produced.
#[test]
#[ignore = "instrument; run explicitly"]
fn peek_damage_what_does_a_still_peek_repaint() {
    let row = |label: &str, c: &Cost| {
        let (damage, overdraw, draws) = c.per_frame();
        println!(
            "  {label:<34} {:4} frames  {:4} damaging   per frame: {damage:6.3}x damaged  \
             {overdraw:6.2}x overdraw  {draws:5.0} draws",
            c.frames, c.damaging,
        );
    };

    println!(
        "\n== defect 1: a still second, {}x{}, 4 workspaces ==",
        OUT.0, OUT.1
    );
    println!("   (headless renders only on damage, so `frames` is the whole answer)");

    // Control: no peek. Everything below is read against this row, not against zero in the
    // abstract — if this one is not 0 frames, stop and fix the harness.
    if let Some((mut f, _)) = build(4) {
        row("peek down, static", &still(&mut f, |_| {}));
    }
    if let Some((mut f, _)) = build(4) {
        summon_peek(&mut f);
        assert!(f.synoik().layout.is_peeking(), "precondition: peeking");
        row("peek up, static", &still(&mut f, |_| {}));
    }

    // Defect 2: one window repaints a 64x64 corner of itself and *nothing else in the scene
    // changes*. What the screen is asked to repaint should be that corner, strip up or down.
    println!("\n== defect 2: one 64x64 client repaint, x60 ==");
    println!(
        "   (the client's own damage is {:.4}x the output; its window covers {:.2}x)",
        (64. * 64.) / (f64::from(OUT.0) * f64::from(OUT.1)),
        f64::from(WIN.0) * f64::from(WIN.1) / (f64::from(OUT.0) * f64::from(OUT.1)),
    );
    let mut base = None;
    if let Some((mut f, at)) = build(4) {
        let c = poke(&mut f, &at, 64, 60);
        row("peek down, 64x64 repaint", &c);
        base = Some(c.per_frame());
    }
    if let Some((mut f, at)) = build(4) {
        summon_peek(&mut f);
        assert!(f.synoik().layout.is_peeking(), "precondition: peeking");
        let c = poke(&mut f, &at, 64, 60);
        row("peek up,   64x64 repaint", &c);
        if let Some((bd, bo, _)) = base {
            let (d, o, _) = c.per_frame();
            println!(
                "   the peek multiplies the same repaint by {:.1}x damaged, {:.1}x overdraw",
                d / bd.max(1e-9),
                o / bo.max(1e-9),
            );
        }
    }

    // How the peek's premium scales with the strip's contents. The live seat's strip holds six
    // workspaces of video call, Slack and live terminals; this one holds four flat rectangles, and
    // it is the first thing to suspect when a headless row lands far under a seat's. If the
    // premium is flat in workspace count, scene richness is not the missing variable and the gap
    // is somewhere else entirely.
    println!("\n== the peek's premium vs the strip's contents ==");
    for n in [2usize, 4, 6, 8] {
        let (Some((mut down, at_down)), Some((mut up, at_up))) = (build(n), build(n)) else {
            break;
        };
        let d = poke(&mut down, &at_down, 64, 30);
        summon_peek(&mut up);
        let u = poke(&mut up, &at_up, 64, 30);
        let ((dd, od, _), (du, ou, _)) = (d.per_frame(), u.per_frame());
        println!(
            "  {n} workspaces: {du:6.3}x damaged ({:4.1}x)  {ou:6.2}x overdraw ({:4.1}x)               {:3.0} draws (vs {:3.0})",
            du / dd.max(1e-9),
            ou / od.max(1e-9),
            u.per_frame().2,
            d.per_frame().2,
        );
    }

    // Defect 3: the same repaint, with the pointer moving over the strip. kov's report is that
    // this costs more again, and the host capture agrees (~110 fps holding still, ~101 fps
    // moving). Two sub-cases, because they have different suspects: a sweep inside one thumbnail
    // changes only where the cursor is, while crossing a boundary changes *which* thumbnail is
    // hovered — and a hover restack churns z-index, which is part of an element's identity and
    // re-damages everything below it (the overview's hover restack was priced at 0.98x the
    // output). If the boundary row is the expensive one, that is the mechanism named.
    println!("\n== defect 3: the same repaint, pointer moving over the strip ==");
    if let Some((mut f, at)) = build(4) {
        let mut step = 0f64;
        row(
            "peek down, pointer moving",
            &poke_nudging(&mut f, &at, 64, 60, |f| {
                step += 1.;
                pointer_motion_to(f, 600. + step % 8., 600.);
            }),
        );
    }
    if let Some((mut f, at)) = build(4) {
        summon_peek(&mut f);
        let thumbs = thumbnails(&mut f);
        assert!(thumbs.len() >= 2, "the strip must have thumbnails to sweep");
        let c = center(thumbs[0]);
        let span = thumbs[0].size.w / 4.;
        let mut step = 0f64;
        row(
            "peek up, within one thumbnail",
            &poke_nudging(&mut f, &at, 64, 60, |f| {
                step += 1.;
                pointer_motion_to(f, c.x + span * (step % 8. / 8. - 0.5), c.y);
            }),
        );
    }
    if let Some((mut f, at)) = build(4) {
        summon_peek(&mut f);
        let thumbs = thumbnails(&mut f);
        assert!(thumbs.len() >= 2, "the strip must have thumbnails to cross");
        let (a, b) = (center(thumbs[0]), center(thumbs[1]));
        let mut step = 0f64;
        row(
            "peek up, crossing thumbnails",
            &poke_nudging(&mut f, &at, 64, 60, |f| {
                step += 1.;
                let t = if (step as u32 / 8).is_multiple_of(2) {
                    0.
                } else {
                    1.
                };
                pointer_motion_to(f, a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t);
            }),
        );
    }
    println!();
}
