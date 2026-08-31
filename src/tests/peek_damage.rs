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
//! **It is not a damage defect.** Damage barely moves: 0.60x the output with the peek down,
//! 0.70x with it up, on the scene that reproduces the cost. What moves is the **blur**, and it
//! moves alone — per-site attribution on a wallpaper+blur scene, full-window client repaint:
//!
//! | | peek down | peek up | delta |
//! |---|---|---|---|
//! | scene | 2.86x | 3.18x | +0.32x |
//! | **blur** | **0.02x** | **2.06x** | **+2.04x** |
//! | total overdraw | 2.89x | 5.25x | +2.36x |
//! | draws | 6 | 60 | +54 |
//!
//! 5.25x against the seat's 5.37x. With the peek down a blurred window shades essentially no blur
//! fragments; with it up, a full-output gaussian chain runs — and it runs to serve thumbnails that
//! are postage stamps. That is `perf_probe`'s standing finding reached by a new road: a blurred
//! window's chain costs the **output**, "whether it is drawn full-size or as a postage stamp
//! overview thumbnail", and the peek's cull-off (`strip_is_peek` in `monitor.rs`) is what puts
//! every workspace's blurred windows on screen as thumbnails.
//!
//! The cost is **flat in workspace count** — 5.19x at two workspaces, 5.30x at eight, ~58 draws
//! throughout. So it is one chain per frame, not one per blurred window, and adding workspaces to
//! a reproduction buys nothing.
//!
//! Two things the probe reports as *absent*, which is the other half of the reading:
//!
//! 1. **Nothing redraws on its own.** A settled scene with the peek up renders zero frames.
//!    Headless renders only on damage — "a redraw leaves the output `Idle` and only new damage
//!    brings it back" ([`crate::backend::headless`]) — so the frame count of a still second is the
//!    whole answer, with no instrument of its own. The seat's continuous frames come from clients
//!    committing, not from the peek asking.
//! 2. **The blur is invisible without blurred content.** A bare scene, or one with only a
//!    wallpaper, prices the peek at 1.1x. Every early reading in this file was taken on flat shm
//!    rectangles and said the peek was nearly free.
//!
//! Damage and overdraw are reported side by side throughout, because conflating them is the trap
//! this was nearly built on: the seat's 5.37x is overdraw, and extra thumbnail elements can shade
//! the same damaged region many times over without damaging more of it.
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
    /// The window's size, so a repaint attaches a buffer of the size the window actually has.
    size: (i32, i32),
}

/// A scene with `windows` windows spread one per workspace, settled, with a renderer attached.
///
/// One per workspace because the strip is what is under test: a peek puts every workspace on
/// screen as a thumbnail, and a strip of empty thumbnails is a scene the defect cannot happen in.
/// `None` when the machine has no Vulkan device — the element pass needs one
/// (`render_element_states` returns early without it), so there is no frame to read.
fn build(windows: usize) -> Option<(Fixture, Poke)> {
    build_scene(windows, Scene::default())
}

/// What live ingredients a probe scene carries beyond bare windows, and the reason the bare scene
/// is not the answer: this file's first reading was taken on flat shm rectangles and put the
/// peek-*down* control at 0.04x the output against the seat's 0.92x. A comparison whose control
/// end is 20x adrift cannot price the peek.
#[derive(Clone, Copy, Default)]
struct Scene {
    /// Decode and upload the real `org.gnome.desktop.background` picture, so the backdrop is a
    /// sampled 4K texture rather than a solid fill.
    wallpaper: bool,
    /// 85% opacity, no opaque border background, `background-effect blur`.
    ///
    /// The pre-registered guess for the seat's 5.37x. A blurred window's chain runs at *output*
    /// resolution regardless of where it is drawn — `perf_probe`'s headline finding, "whether it
    /// is drawn full-size or as a postage stamp overview thumbnail" — and the peek's cull-off is
    /// exactly what puts every workspace's windows on screen as thumbnails. If that is the
    /// mechanism, peek-up pays a full-resolution blur chain per blurred window that peek-down
    /// never renders at all, and the premium is per blurred window rather than per workspace.
    blur: bool,
}

fn build_scene(windows: usize, scene: Scene) -> Option<(Fixture, Poke)> {
    build_scene_sized(windows, scene, WIN)
}

/// [`build_scene`] with the window size named, so an arm can put the windows clear of the dash.
fn build_scene_sized(windows: usize, scene: Scene, win: (i32, i32)) -> Option<(Fixture, Poke)> {
    use synoik_config::{BackgroundEffectRule, Config, WindowRule};

    if let Err(e) = VulkanRenderer::new() {
        eprintln!("skipping: no Vulkan device ({e})");
        return None;
    }
    synoik_vk::stats::set_enabled(true);

    let mut config = Config::default();
    if scene.blur {
        config.window_rules.push(WindowRule {
            opacity: Some(0.85),
            draw_border_with_background: Some(false),
            background_effect: BackgroundEffectRule {
                blur: Some(true),
                ..Default::default()
            },
            ..Default::default()
        });
    }
    let mut f = Fixture::with_config(config);
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
        window.attach_shm_buffer(win.0, win.1, 200, 100, 50, 255);
        window.set_size(win.0 as u16, win.1 as u16);
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
                size: win,
            });
        }
    }

    if scene.wallpaper {
        // The real picture, decoded synchronously (no worker in the harness) and staged straight
        // into device memory, exactly as the session does it. Slow in a debug build — it happens
        // once per fixture, outside every measured frame.
        let settings = crate::gnome::BackgroundSettings {
            picture: Some(std::path::PathBuf::from(WALLPAPER)),
            options: crate::gnome::BackgroundOptions::default(),
        };
        let gpu = f
            .synoik_state()
            .backend
            .with_vulkan_renderer(|r| r.gpu().clone());
        f.synoik().wallpaper.update(&settings, gpu.as_ref());
    }

    f.settle();
    Some((f, poke.expect("at least one window")))
}

/// The gsrs session's wallpaper (`org.gnome.desktop.background picture-uri`).
const WALLPAPER: &str = "/usr/share/backgrounds/f34/default/f34-01-day.png";

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
    /// Fragments shaded per [`DrawSite`], as multiples of the output — which of scene, blur, text
    /// and offscreen the frame actually went to. A total that moves without a site that moved is
    /// an attribution gap, not a finding.
    by_site: [f64; synoik_vk::stats::DrawSite::ALL.len()],
    /// The tracker's own rects for the first few damaging frames — what the screen was actually
    /// asked to repaint, not how much of it. A blur that recaptures pushes its whole geometry in
    /// here, so this is where "who damaged what" is legible.
    sample_rects: Vec<Vec<smithay::utils::Rectangle<i32, Physical>>>,
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
/// Frames still owed an element dump. The scene the tracker actually saw — id, geometry and
/// whether it is a framebuffer effect — is the only thing that says *which* element a blur
/// recapture was triggered by, and no aggregate can be read back into it.
static DUMP_FRAMES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn measure(f: &mut Fixture, body: impl FnOnce(&mut Fixture)) -> Cost {
    let output = f.synoik_output(1);
    let name = output.name();
    let size: Size<i32, Physical> = output.current_mode().expect("the output has a mode").size;

    const SITES: usize = synoik_vk::stats::DrawSite::ALL.len();
    let counted = Rc::new(RefCell::new((0u64, 0u64, [0u64; SITES])));
    {
        let counted = counted.clone();
        let mut warm = WarmTarget::new(&output, 1);
        f.synoik_state().backend.headless().frame_sink =
            Some(Box::new(move |vk, _output, elements| {
                use std::sync::atomic::Ordering;
                if DUMP_FRAMES.load(Ordering::Relaxed) > 0 {
                    DUMP_FRAMES.fetch_sub(1, Ordering::Relaxed);
                    println!("        -- scene, front to back --");
                    for (i, e) in elements.iter().enumerate() {
                        use smithay::backend::renderer::element::Element as _;
                        let g = e.geometry(smithay::utils::Scale::from(1.));
                        let dup = elements[..i]
                            .iter()
                            .position(|p| p.id() == e.id())
                            .map(|k| format!("dup-of-[{k}] "))
                            .unwrap_or_default();
                        println!(
                            "        [{i:2}] {:>4}x{:<4}@{:>5},{:<5} fx={} c={:?} {dup}{}",
                            g.size.w,
                            g.size.h,
                            g.loc.x,
                            g.loc.y,
                            u8::from(e.is_framebuffer_effect()),
                            e.current_commit(),
                            {
                                let d = format!("{e:?}");
                                d.chars().take(110).collect::<String>()
                            },
                        );
                    }
                }
                let (d0, s0) = (synoik_vk::stats::draws(), synoik_vk::stats::shaded());
                let site0 = synoik_vk::stats::shaded_by_site();
                draw_into(vk, &mut warm, size, elements, false);
                let site1 = synoik_vk::stats::shaded_by_site();
                let mut c = counted.borrow_mut();
                c.0 += synoik_vk::stats::draws() - d0;
                c.1 += synoik_vk::stats::shaded() - s0;
                for i in 0..SITES {
                    c.2[i] += site1[i] - site0[i];
                }
            }));
    }
    // Warm the pipelines and let the tracker take a first, necessarily-full frame, then start the
    // ledgers: the first render into a fresh target is a full repaint by construction and would
    // otherwise be counted as the peek's doing.
    f.synoik().queue_redraw_all();
    f.dispatch();
    *counted.borrow_mut() = (0, 0, [0; SITES]);
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
    let (draws, shaded, by_site) = *counted.borrow();

    let out_px = f64::from(OUT.0) * f64::from(OUT.1);
    let mine: Vec<&FrameDamage> = log.iter().filter(|d| d.output == name).collect();
    Cost {
        frames: mine.len(),
        damaging: mine.iter().filter(|d| d.area() > 0).count(),
        damage: mine.iter().map(|d| d.area() as f64).sum::<f64>().max(0.) / out_px,
        overdraw: shaded as f64 / out_px,
        draws,
        by_site: by_site.map(|n| n as f64 / out_px),
        sample_rects: mine
            .iter()
            .filter(|d| d.area() > 0)
            .take(3)
            .map(|d| d.damage.clone().unwrap_or_default())
            .collect(),
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
/// Pointer motion has to be measured *here* rather than on a still scene: moving the pointer
/// queues no redraw at all headless (pinned by [`the_control_pointer_motion_queues_no_redraw`]),
/// so a still scene renders nothing to measure. This measures what motion does to the cost of a
/// frame that was happening anyway — which is half the answer. A seat's software cursor also makes
/// motion *add* frames, and that half is not visible from here.
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
                    p.size.0,
                    p.size.1,
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
/// A headless artifact, not a behavior to generalize from: the cursor is not in the element list
/// this backend assembles, while a seat runs a **software cursor**, so there every motion event
/// damages the cursor rect and queues a redraw. Motion on a seat therefore adds *frames* — up to
/// the pointer's rate — each paying the peek's inflated per-frame cost. The arms here measure only
/// the cost half of `frames x cost`; the frame half has to be answered on a seat.
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

    // Calibration, and the content ladder in one table. The seat's peek-*down* frame draws 0.92x
    // the output; a row here whose peek-down column is far under that is not measuring the same
    // frame, whatever its peek-up column says. So each row reports both ends, and the ingredient
    // that brings peek-down to ~0.9x is the scene this file should be asking its questions on.
    //
    // vkcube repaints its whole window every frame, so the repaint is full-window here rather
    // than the 64x64 corner below — a corner is the *other* question (what a small change costs),
    // and mixing them is what put the control 20x adrift.
    println!("\n== calibration: full-window repaint, x30, 4 workspaces ==");
    println!("   (the live seat reads 0.92x overdraw peek down, 5.37x peek up — p95 10.5x)");
    for (label, scene) in [
        ("bare", Scene::default()),
        (
            "+ wallpaper",
            Scene {
                wallpaper: true,
                ..Default::default()
            },
        ),
        (
            "+ wallpaper + blur",
            Scene {
                wallpaper: true,
                blur: true,
            },
        ),
    ] {
        let (Some((mut down, at_down)), Some((mut up, at_up))) =
            (build_scene(4, scene), build_scene(4, scene))
        else {
            break;
        };
        if label.contains("blur") {
            println!("        ==== PEEK DOWN ====");
            DUMP_FRAMES.store(2, std::sync::atomic::Ordering::Relaxed);
        }
        let d = poke(&mut down, &at_down, WIN.0, 30);
        summon_peek(&mut up);
        let _ = crate::render_helpers::background_effect::trace::take_settled();
        let _ = crate::render_helpers::background_effect::trace::take_captures();
        if label.contains("blur") {
            {
                let output = up.synoik_output(1);
                let mon = up
                    .synoik()
                    .layout
                    .monitor_for_output(&output)
                    .expect("a monitor");
                for (i, (ws, geo)) in mon.workspaces_with_render_geo_idx().enumerate() {
                    println!(
                        "        ws[{i}] idx={} geo={:?} windows={}",
                        ws.0,
                        geo,
                        ws.1.windows().count()
                    );
                }
            }
            println!("        ==== PEEK UP ====");
            DUMP_FRAMES.store(2, std::sync::atomic::Ordering::Relaxed);
        }
        let u = poke(&mut up, &at_up, WIN.0, 30);
        let settled = crate::render_helpers::background_effect::trace::take_settled();
        let caps = crate::render_helpers::background_effect::trace::take_captures();
        let ((dd, od, ddr), (du, ou, dur)) = (d.per_frame(), u.per_frame());
        println!(
            "  {label:<20} down {od:5.2}x overdraw ({dd:5.3}x dmg, {ddr:3.0} draws)   \
             up {ou:5.2}x ({du:5.3}x dmg, {dur:3.0} draws)   premium {:4.1}x",
            ou / od.max(1e-9),
        );
        {
            let mut counts: Vec<((u32, u32), usize)> = Vec::new();
            for dims in &settled {
                match counts.iter_mut().find(|(d, _)| d == dims) {
                    Some((_, n)) => *n += 1,
                    None => counts.push((*dims, 1)),
                }
            }
            counts.sort_by_key(|((w, h), _)| std::cmp::Reverse(u64::from(*w) * u64::from(*h)));
            let per_frame = u.frames.max(1);
            let sum: f64 = settled
                .iter()
                .map(|(w, h)| f64::from(*w) * f64::from(*h))
                .sum::<f64>()
                / (f64::from(OUT.0) * f64::from(OUT.1) * per_frame as f64);
            let shown: Vec<String> = counts
                .iter()
                .take(6)
                .map(|((w, h), n)| format!("{w}x{h} x{:.1}", *n as f64 / per_frame as f64))
                .collect();
            println!(
                "      intermediates/frame: {sum:5.3}x out total  [{}]",
                shown.join(", "),
            );
            let mut pairs: Vec<(String, usize)> = Vec::new();
            let mut ids: Vec<smithay::backend::renderer::element::Id> = Vec::new();
            for (c, (w, h)) in caps.iter().zip(&settled) {
                let slot = match ids.iter().position(|i| *i == c.id) {
                    Some(i) => i,
                    None => {
                        ids.push(c.id.clone());
                        ids.len() - 1
                    }
                };
                let k = format!(
                    "elem #{slot}  into {:4}x{:<4}  drawn {:4}x{:<4} -> blurs {w}x{h}",
                    c.target.w, c.target.h, c.dst.size.w, c.dst.size.h
                );
                match pairs.iter_mut().find(|(p, _)| *p == k) {
                    Some((_, n)) => *n += 1,
                    None => pairs.push((k, 1)),
                }
            }
            for (arm, c) in [("down", &d), ("up", &u)] {
                for (i, rects) in c.sample_rects.iter().enumerate() {
                    let shown: Vec<String> = rects
                        .iter()
                        .map(|r| format!("{}x{}@{},{}", r.size.w, r.size.h, r.loc.x, r.loc.y))
                        .collect();
                    println!("        damage {arm} f{i}: {}", shown.join(" "));
                }
            }
            pairs.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
            for (k, n) in pairs.iter().take(6) {
                println!("        {k}   x{:.2}/frame", *n as f64 / per_frame as f64);
            }
        }
        let n = |c: &Cost| c.frames.max(1) as f64;
        for (i, site) in synoik_vk::stats::DrawSite::ALL.iter().enumerate() {
            let (a, b) = (d.by_site[i] / n(&d), u.by_site[i] / n(&u));
            if a > 0.005 || b > 0.005 {
                println!(
                    "      {:<10} down {a:5.2}x   up {b:5.2}x   {:+5.2}x",
                    site.label(),
                    b - a,
                );
            }
        }
    }

    // **The cascade.** A blur that recaptures pushes its whole geometry onto the end of the
    // tracker's damage list, and every blur processed after it sees that push — so one seeded
    // recapture at the front of the scene forces a full-resolution recapture of every blurred
    // window behind it. The peek supplies the seed by poking the dash out: the dash's backdrop
    // overlaps the bottom sliver of a window, so that window's own repaint now lands *below* a
    // blur, which is the one thing that forces one.
    //
    // The arm that says so varies exactly one thing — the window's height, so the same scene
    // either reaches under the dash or clears it. If the cascade is the mechanism, the shorter
    // windows cost the dash's own small chain and nothing else.
    println!("\n== does the dash seed the blur cascade? ==");
    let blur = Scene {
        wallpaper: true,
        blur: true,
    };
    for (label, win) in [
        ("windows under the dash", (WIN.0, 1000)),
        ("windows clear of it", (WIN.0, 860)),
    ] {
        let Some((mut f, at)) = build_scene_sized(4, blur, win) else {
            break;
        };
        summon_peek(&mut f);
        f.synoik_complete_animations();
        f.dispatch();
        let _ = crate::render_helpers::background_effect::trace::take_settled();
        let c = poke(&mut f, &at, win.0, 30);
        let settled = crate::render_helpers::background_effect::trace::take_settled();
        let n = c.frames.max(1) as f64;
        let blur_site = c.by_site[synoik_vk::stats::DrawSite::Blur as usize] / n;
        let big = settled.iter().filter(|(w, _)| *w > 1000).count() as f64 / n;
        println!(
            "  {label:<24} {}x{} -> blur {blur_site:5.2}x/frame,              {big:4.2} full-size chains/frame, {:5.2}x total",
            win.0,
            win.1,
            c.per_frame().1,
        );
    }

    // Which effects capture, and at what resolution. The chain's cost is its *intermediate's*
    // area, and the intermediate comes from the surface's own geometry and scale rather than from
    // where the element is drawn (`framebuffer_effect.rs`: "not dst.size"). So a window shown as a
    // postage-stamp thumbnail still blurs at its full on-screen size, and this is the table that
    // says so in numbers rather than by reading the code.
    println!("\n== who captures, and how big is their blur ==");
    let blur = Scene {
        wallpaper: true,
        blur: true,
    };
    for (label, up) in [("peek down", false), ("peek up", true)] {
        let Some((mut f, at)) = build_scene(4, blur) else {
            break;
        };
        if up {
            summon_peek(&mut f);
        }
        let _ = crate::render_helpers::background_effect::trace::take_captures();
        let _ = crate::render_helpers::background_effect::trace::take_settled();
        poke(&mut f, &at, WIN.0, 2);
        let caps = crate::render_helpers::background_effect::trace::take_captures();
        let settled = crate::render_helpers::background_effect::trace::take_settled();
        let out_px = f64::from(OUT.0) * f64::from(OUT.1);
        println!("  {label}: {} captures over 2 frames", caps.len());
        for (i, c) in caps.iter().enumerate().take(12) {
            let (sw, sh) = settled.get(i).copied().unwrap_or((0, 0));
            let inter = f64::from(sw) * f64::from(sh);
            println!(
                "     drawn {:4}x{:<4} ({:5.3}x out)   asked {:4}x{:<4}   blurs at {:4}x{:<4} \
                 ({:5.3}x out)   {:5.1}x more pixels than it shows",
                c.dst.size.w,
                c.dst.size.h,
                f64::from(c.dst.size.w) * f64::from(c.dst.size.h) / out_px,
                c.intermediate.w,
                c.intermediate.h,
                sw,
                sh,
                inter / out_px,
                inter / (f64::from(c.dst.size.w) * f64::from(c.dst.size.h)).max(1.),
            );
        }
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

    // How the peek's premium scales, on the scene that reproduces it. Each extra workspace is one
    // more *blurred* window the peek brings on screen as a thumbnail — and a blurred window's
    // chain runs at output resolution wherever it is drawn. So the prediction is a straight line
    // in workspace count, at roughly the cost of a full-screen blur apiece. Flat would falsify it.
    println!("\n== the peek's premium vs the strip's contents ==");
    for n in [2usize, 4, 6, 8] {
        let (Some((mut down, at_down)), Some((mut up, at_up))) =
            (build_scene(n, blur), build_scene(n, blur))
        else {
            break;
        };
        let d = poke(&mut down, &at_down, WIN.0, 30);
        summon_peek(&mut up);
        let u = poke(&mut up, &at_up, WIN.0, 30);
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

/// Pins the blur cascade shut. A framebuffer effect that recaptures pushes its own geometry onto
/// the end of the tracker's damage list; those pushes land past every recorded per-element index,
/// so without the watermark in the tracker's recapture test, every blur *behind* the seed sees them
/// and recaptures at full resolution. One seed re-blurs the screen.
///
/// The seed here is the dash: bringing the peek up pokes it out, and a repainting window tall
/// enough to reach under its backdrop is the one thing that forces a recapture. The two arms differ
/// in window height and nothing else, so a difference between them is the cascade and not the
/// strip's own contents. Pre-fix the tall arm ran 1.80 full-size chains per frame against the short
/// arm's 0.00; the bound is loose because the gap is not.
#[test]
fn a_blur_recapture_does_not_cascade_to_the_blurs_behind_it() {
    let scene = Scene {
        wallpaper: true,
        blur: true,
    };
    let mut chains = Vec::new();
    for win in [(WIN.0, 1000), (WIN.0, 860)] {
        // No renderer, no measurement: the arms are only comparable against each other.
        let Some((mut f, at)) = build_scene_sized(4, scene, win) else {
            return;
        };
        summon_peek(&mut f);
        f.synoik_complete_animations();
        f.dispatch();
        let _ = crate::render_helpers::background_effect::trace::take_settled();
        let c = poke(&mut f, &at, win.0, 30);
        let settled = crate::render_helpers::background_effect::trace::take_settled();
        let n = c.frames.max(1) as f64;
        chains.push(settled.iter().filter(|(w, _)| *w > 1000).count() as f64 / n);
    }
    let (under_dash, clear_of_it) = (chains[0], chains[1]);
    assert!(
        under_dash < 0.5,
        "a window reaching under the dash's blur seeds {under_dash:.2} full-size blur chains per \
         frame (a window clear of it: {clear_of_it:.2}) — the recapture is cascading to the blurs \
         behind the seed again"
    );
}
