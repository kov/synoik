// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! How smoothly an animated edge actually moves, measured in rendered pixels.
//!
//! Geometry is not the question here. The layout's own curve is smooth to nine decimal places —
//! sampled per frame at `org.synoik.animations speed 0.3`, a thumbnail's width moves by
//! `0.19, 0.49, 0.70, 0.84, 0.92, 0.96, 0.97, 0.96, 0.93, 0.88 ...` logical px. What reaches the
//! screen is that curve snapped to whole physical pixels, which lands as `1, 2, 1, 2, 2, 1 ...`,
//! and the alternation is what reads as shimmer at slow speeds.
//!
//! So this module renders through the real Vulkan path and measures the result in two places,
//! because fractional placement moves them independently:
//!
//! - **The silhouette**, the thumbnail's own outer edge. Every pipeline is
//!   `SampleCountFlags::TYPE_1` and the geometry clip in `clipped_texture.frag` is a hard step, so
//!   this edge is wherever the rasteriser decides and nowhere in between. A fractional origin alone
//!   will not smooth it: that changes which pixel the edge lands on, not how sharply it gets there.
//!   Moving it needs coverage in the shader.
//! - **The interior**, a feature of the client's own texture. Sampled `LINEAR`, it can slide
//!   continuously under that hard edge as soon as the destination rect can hold a fraction, with no
//!   shader work at all. If the shimmer goes when this one goes, the silhouette work is not needed.
//!
//! **Each is judged against unrounded geometry, not against its own smoothness.** How finely a
//! series resolves proves nothing: a feature sitting at fraction `p` of a rect is at
//! `loc + p·w`, and since `p` is wherever the window happened to land in its workspace — an
//! arbitrary real — even wholly integer `loc` and `w` yield arbitrary fractional positions that
//! look subpixel. What integer placement cannot hide is the *residual*: measured position minus
//! where the layout's own f64 rect says it should be. Rounding makes that residual sweep and snap
//! back once per pixel of travel, so it is judged peak-to-peak — a constant offset is only the
//! calibration, while a sawtooth is the defect.
//!
//! Both probes are predicted the same way, as `loc + p·w` with `p` measured at rest. Neither sits
//! exactly where it is aimed — the silhouette reads a few pixels inside the thumbnail's drawn
//! rect — and an offset like that is in *thumbnail* units, so it grows and shrinks as the
//! thumbnail scales. Treated as a constant it would leave a drift in the residual of its own,
//! about half a pixel here, which is the same size as the defect being looked for.
//!
//! **This cannot see damage.** [`capture`] renders the whole element list every frame, so a change
//! that moves an element without reporting damage still shows up here, correctly placed, while on
//! screen nothing would repaint at all. A green result in this module says a frame *drawn* is
//! drawn right; it says nothing about whether the frame gets drawn.

use smithay::backend::allocator::Fourcc;
use smithay::output::Output;
use smithay::utils::{Physical, Scale, Size, Transform};

use super::fixture::Fixture;
use crate::render_helpers::vulkan::VulkanRenderer;
use crate::render_helpers::{render_to_vec, RenderCtx, RenderTarget};
use crate::synoik::OutputRenderElements;

const OUT_W: u16 = 1280;
const OUT_H: u16 = 720;

/// Rec. 709 luma, the same weighting used to read the screencasts this came from.
fn luma(px: &[u8], w: i32, x: i32, y: i32) -> f64 {
    let i = ((y * w + x) * 4) as usize;
    0.2126 * f64::from(px[i]) + 0.7152 * f64::from(px[i + 1]) + 0.0722 * f64::from(px[i + 2])
}

/// One row of luminance.
fn row(px: &[u8], w: i32, y: i32) -> Vec<f64> {
    (0..w).map(|x| luma(px, w, x, y)).collect()
}

/// Where `lums` first crosses `threshold`, to a fraction of a pixel.
///
/// Linear interpolation across the crossing, not the index of the first pixel past it: the whole
/// point is to resolve motion smaller than a pixel, and an integer answer could not show it. A
/// measurement that can only return integers would report perfectly quantised motion no matter
/// what the renderer did.
fn crossing(lums: &[f64], threshold: f64) -> Option<f64> {
    for i in 0..lums.len().saturating_sub(1) {
        let (a, b) = (lums[i], lums[i + 1]);
        if (a < threshold) != (b < threshold) {
            let t = if (b - a).abs() < f64::EPSILON {
                0.5
            } else {
                (threshold - a) / (b - a)
            };
            return Some(i as f64 + t);
        }
    }
    None
}

/// [`crossing`], restricted to `lums[from..to]` and reported in whole-row coordinates.
///
/// Scanning the whole row finds whatever comes first, which is rarely the feature being measured —
/// the first version of this searched from x=0 and locked onto the contents of a *static*
/// neighbouring thumbnail, then reported a perfectly motionless edge for 77 consecutive frames.
fn crossing_in(lums: &[f64], from: usize, to: usize, threshold: f64) -> Option<f64> {
    let to = to.min(lums.len());
    if from >= to {
        return None;
    }
    crossing(&lums[from..to], threshold).map(|x| x + from as f64)
}

/// The lowest and highest luminance in `lums[from..to]`, for a threshold local to the feature
/// rather than to the whole screen.
fn span(lums: &[f64], from: usize, to: usize) -> Option<(f64, f64)> {
    let to = to.min(lums.len());
    if from >= to {
        return None;
    }
    let win = &lums[from..to];
    let lo = win.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = win.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    (hi - lo > 8.).then_some((lo, hi))
}

/// Where the sharpest luminance step in `lums[from..to]` sits, to a fraction of a pixel.
///
/// The interior probe cannot be aimed at a fixed fraction of the thumbnail: what it is looking
/// for is a feature of the *client's* texture, and where that lands inside the thumbnail depends
/// on where the window was placed in its workspace. So find the steepest adjacent pair in the
/// window — which is the step edge, every other transition in the scene being gentler — then
/// interpolate across the whole ramp around it.
///
/// Interpolating across the steepest *pair* alone would be worthless: the half-way level of a
/// single pair is by construction half-way between its two ends, so the answer would always come
/// out as that pair's index plus exactly 0.5, quantising the measurement to the grid it exists to
/// resolve. The levels have to come from the flat ground either side of the ramp instead.
fn steepest_crossing(lums: &[f64], from: usize, to: usize) -> Option<f64> {
    let to = to.min(lums.len());
    if to <= from + 1 {
        return None;
    }
    let (i, rise) =
        (from..to - 1)
            .map(|i| (i, lums[i + 1] - lums[i]))
            .fold((from, 0f64), |best, (i, d)| {
                if d.abs() > best.1.abs() {
                    (i, d)
                } else {
                    best
                }
            });
    if rise.abs() < 8. {
        return None;
    }
    // Wide enough to clear the ramp — a few texels in the client, so a couple of pixels once
    // minified — and no wider, so the levels are this feature's and not the scene's.
    let (a, b) = (i.saturating_sub(4).max(from), (i + 5).min(to));
    span(lums, a, b).and_then(|(lo, hi)| crossing_in(lums, a, b, (lo + hi) / 2.))
}

/// Composite the live scene through the owned Vulkan renderer and read it back, as
/// `(pixels, width, height)`.
fn capture(f: &mut Fixture, output: &Output) -> (Vec<u8>, i32, i32) {
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
                target: RenderTarget::Output,
                appearance: Some(synoik.appearance()),
            };
            let elements: Vec<OutputRenderElements> = synoik.render_to_vec(ctx, output, false);

            // Elements come front-to-back; `render_to_vec` draws in iteration order, so reverse.
            let pixels = render_to_vec(
                vk,
                size,
                scale,
                Transform::Normal,
                Fourcc::Abgr8888,
                elements.into_iter().rev(),
            )?;
            Ok((pixels, size.w, size.h))
        })
        .expect("the fixture was built with a Vulkan renderer")
        .expect("composite the scene through Vulkan")
}

/// The overview open on two workspaces, each holding a fullscreen window — the scene whose strip
/// was measured on the seat.
///
/// `renderer` builds the Vulkan renderer and so returns `None` (having said why) on a machine
/// without a device; geometry-only callers pass `false` and always get a fixture.
fn build_overview(scale: f64, renderer: bool) -> Option<(Fixture, Output)> {
    if renderer {
        if let Err(e) = VulkanRenderer::new() {
            eprintln!("skipping subpixel measurement: no Vulkan device ({e})");
            return None;
        }
    }

    let mut f = Fixture::new();
    if renderer {
        f.synoik_state()
            .backend
            .headless()
            .add_renderer()
            .expect("build the Vulkan renderer");
    }
    f.add_output(1, (OUT_W, OUT_H));
    if scale != 1. {
        f.resize_output(1, None, Some(scale));
    }
    let output = f.synoik_output(1);

    f.synoik_state().synoik.gnome_settings.animation_speed = 0.3;
    f.synoik_state().refresh_animation_clock();

    let id = f.add_client();
    for _ in 0..2 {
        let window = f.client(id).create_window();
        let surface = window.surface.clone();
        // Fullscreen, so the client's texture maps onto the whole workspace and therefore onto
        // the whole thumbnail. A smaller window puts its own borders inside the thumbnail, and
        // those are boundaries between two rounded element rects rather than sampled texture —
        // they move like a silhouette, and being far the steepest thing in the row they capture
        // the interior probe. That is what an earlier version of this measured: a feature at 0.91
        // of the thumbnail's width, which is the window's right border, not its content.
        window.set_fullscreen(None);
        window.commit();
        f.roundtrip(id);

        let window = f.client(id).window(&surface);
        window.attach_shm_step_edge(i32::from(OUT_W), i32::from(OUT_H));
        window.set_size(OUT_W, OUT_H);
        // The initial configure has to be acked before a buffer counts.
        window.ack_last_and_commit();
        f.double_roundtrip(id);

        f.synoik_state()
            .do_action(synoik_config::Action::FocusWorkspaceDown, false);
    }
    f.settle();

    f.synoik_state()
        .do_action(synoik_config::Action::ToggleOverview, false);
    f.settle();

    Some((f, output))
}

/// The scene the measurements below run against.
fn overview_fixture() -> Option<(Fixture, Output)> {
    build_overview(1., true)
}

/// Nothing at rest may sit between pixels.
///
/// This is the precondition for placing anything on a fraction. Today the rounding is what keeps
/// resting edges crisp: geometry lands wherever it lands and the renderer snaps it. Take the snap
/// away without putting the rest positions on the grid deliberately and every still thumbnail
/// goes permanently soft — and arming the fraction only while something moves is worse, because
/// then each switch ends with a visible pop as the edges jump back onto the grid.
///
/// Both scales matter and they fail for different reasons. At a fractional output scale a whole
/// logical pixel is not a whole physical one, so a rect rounded in logical units is off the grid
/// by construction. At scale 1 that cannot happen, so anything off the grid there is a size
/// computed and then never re-snapped — which is what the inactive shrink does.
///
/// A thumbnail is drawn shrunk by `WORKSPACE_INACTIVE_SCALE` and re-centred in its slot, and that
/// product is not a whole pixel. `thumbnail_drawn_rect` therefore snaps the two *ends* of the
/// shrink to the grid and interpolates between them: a thumbnail at rest lands on whole pixels,
/// while a moving one stays free to sit between them instead of having its motion quantised.
#[test]
fn a_resting_thumbnail_sits_on_whole_physical_pixels() {
    for scale in [1., 1.5] {
        let (mut f, output) = build_overview(scale, false).expect("a fixture without a renderer");
        let rects = f
            .synoik()
            .layout
            .monitor_for_output(&output)
            .unwrap()
            .thumbnail_drawn_rects();
        assert!(rects.len() > 1, "the strip must have thumbnails to judge");

        let off: Vec<String> = rects
            .iter()
            .enumerate()
            .filter_map(|(i, r)| {
                let edges = [
                    ("x", r.loc.x),
                    ("y", r.loc.y),
                    ("w", r.size.w),
                    ("h", r.size.h),
                ];
                let bad: Vec<String> = edges
                    .iter()
                    .filter_map(|(name, v)| {
                        let physical = v * scale;
                        // Float dust: a physical value reached by dividing by 1.5 and adding
                        // back up does not land on the integer exactly.
                        let off = (physical - physical.round()).abs();
                        (off > 1e-6).then(|| format!("{name} {v} ({physical:.4}px)"))
                    })
                    .collect();
                (!bad.is_empty()).then(|| format!("thumbnail {i}: {}", bad.join(", ")))
            })
            .collect();

        assert!(
            off.is_empty(),
            "at scale {scale} these resting edges are between physical pixels:\n  {}",
            off.join("\n  ")
        );
    }
}

/// One frame: what each probe measured, and the unrounded geometry to judge it against.
///
/// Positions are physical pixels across the captured row.
struct Frame {
    silhouette: Option<f64>,
    interior: Option<f64>,
    /// The thumbnail's left edge and width as the layout has them, before any rounding.
    loc: f64,
    w: f64,
}

/// Measure both probes in the scene as it stands.
fn probe(f: &mut Fixture, output: &Output) -> Option<Frame> {
    // Thumbnail 1 is one of the pair that trades size across the switch. Thumbnail 0 sits
    // perfectly still, so aiming there measures a static edge and calls the renderer flawless.
    let thumb = {
        let mon = f.synoik().layout.monitor_for_output(output).unwrap();
        mon.thumbnail_drawn_rects().get(1).copied()
    }?;

    let (px, w, h) = capture(f, output);
    let scale = output.current_scale().fractional_scale();
    let y = ((thumb.loc.y + thumb.size.h / 2.) * scale).round() as i32;
    if y < 0 || y >= h {
        return None;
    }
    let lums = row(&px, w, y);

    let (loc, width) = (thumb.loc.x * scale, thumb.size.w * scale);

    // The silhouette: the backdrop-to-thumbnail step, searched around where the layout says the
    // left edge is. Wide enough to hold the edge for the whole animation, narrow enough that it
    // cannot reach the neighbouring thumbnail across the gap.
    let (a, b) = (
        (loc.round() as i32 - 16).max(0) as usize,
        (loc.round() as i32 + 16).max(0) as usize,
    );
    let silhouette =
        span(&lums, a, b).and_then(|(lo, hi)| crossing_in(&lums, a, b, (lo + hi) / 2.));

    // The interior: the client's own step edge, kept well clear of both silhouette edges so
    // neither can be mistaken for it.
    let x0 = (loc + width * 0.08).round().max(0.) as usize;
    let x1 = (loc + width * 0.92).round().max(0.) as usize;
    let interior = steepest_crossing(&lums, x0, x1);

    Some(Frame {
        silhouette,
        interior,
        loc,
        w: width,
    })
}

/// Which of the two probes a measurement came from.
#[derive(Clone, Copy)]
enum Probe {
    Silhouette,
    Interior,
}

impl Probe {
    fn of(self, frame: &Frame) -> Option<f64> {
        match self {
            Probe::Silhouette => frame.silhouette,
            Probe::Interior => frame.interior,
        }
    }
}

/// Where each probe sits as a fraction of the thumbnail's width.
///
/// Taken at rest, before the switch starts. Calibrating off the animation's first frame would
/// fold that frame's rounding error into every prediction made from it; at rest there is no such
/// error to fold, because a resting thumbnail sits on whole physical pixels — which is not a
/// happy accident but an invariant, pinned by
/// [`a_resting_thumbnail_sits_on_whole_physical_pixels`].
fn calibrate(f: &mut Fixture, output: &Output) -> Option<(f64, f64)> {
    let frame = probe(f, output)?;
    let at = |v: Option<f64>| v.map(|v| (v - frame.loc) / frame.w);
    Some((at(frame.silhouette)?, at(frame.interior)?))
}

/// Both probes, per frame, across a workspace switch.
fn sample_edges(f: &mut Fixture, output: &Output) -> Vec<Frame> {
    f.synoik_state()
        .do_action(synoik_config::Action::FocusWorkspaceUp, false);

    f.sample_every_frame(400, |f| probe(f, output))
        .into_iter()
        .flatten()
        .collect()
}

/// How far each frame's measurement sits from where unrounded geometry puts it.
fn residuals(frames: &[Frame], which: Probe, p: f64) -> Vec<f64> {
    frames
        .iter()
        .filter_map(|f| Some(which.of(f)? - (f.loc + p * f.w)))
        .collect()
}

/// The swing in a residual series. A constant offset is calibration; a swing is rounding.
fn pk_pk(residuals: &[f64]) -> f64 {
    let lo = residuals.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = residuals.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    hi - lo
}

/// Steps between consecutive readings.
fn steps(series: &[Option<f64>]) -> Vec<f64> {
    let present: Vec<f64> = series.iter().filter_map(|v| *v).collect();
    present.windows(2).map(|p| p[1] - p[0]).collect()
}

/// A readout of one probe: how far it travelled, and how its residual behaved.
fn report(label: &str, frames: &[Frame], which: Probe, p: f64) -> String {
    let series: Vec<Option<f64>> = frames.iter().map(|f| which.of(f)).collect();
    let r = residuals(frames, which, p);
    let round = |v: &f64| (v * 1000.).round() / 1000.;
    format!(
        "{label}: travelled {:.3}px over {} frames, sitting at {p:.4} of the thumbnail's width, \
         residual swing {:.3}px.\n  steps: {:?}\n  residuals: {:?}",
        steps(&series).iter().sum::<f64>(),
        r.len(),
        pk_pk(&r),
        steps(&series).iter().map(round).collect::<Vec<_>>(),
        r.iter().map(round).collect::<Vec<_>>(),
    )
}

/// The instrument's own guard: both probes must actually move, and the silhouette must never move
/// backwards. Without this the measurements below could be reading a still image and reporting
/// perfect results — an earlier version of this file did exactly that for 77 consecutive frames.
#[test]
fn the_strip_edge_moves_and_never_reverses() {
    let Some((mut f, output)) = overview_fixture() else {
        return;
    };
    let (sil_p, int_p) = calibrate(&mut f, &output).expect("both probes must find their feature");
    let frames = sample_edges(&mut f, &output);

    let silhouette: Vec<Option<f64>> = frames.iter().map(|f| f.silhouette).collect();
    let moved = steps(&silhouette);
    assert!(
        moved.len() > 8,
        "the switch must produce a measurable run of frames, got {}",
        moved.len()
    );

    let travel: f64 = moved.iter().sum();
    assert!(
        travel.abs() > 4.,
        "the strip must actually travel during the switch. A flat series here means the \
         measurement is locked onto something static (the band's own edge, or the screen's) \
         rather than the thumbnail's.\n{}",
        report("silhouette", &frames, Probe::Silhouette, sil_p)
    );

    // The interior probe has its own way of reading as perfect: aimed at the thumbnail's centre
    // it sits on the one point that hardly moves while the thumbnail swells about it, and
    // reports a motionless feature no matter what the renderer does.
    let interior: Vec<Option<f64>> = frames.iter().map(|f| f.interior).collect();
    let interior_travel: f64 = steps(&interior).iter().sum();
    assert!(
        interior_travel.abs() > 1.,
        "the interior probe must travel too, or it is sitting on the thumbnail's centre and \
         measuring nothing.\n{}",
        report("interior", &frames, Probe::Interior, int_p)
    );

    let sign = travel.signum();
    let worst = moved
        .iter()
        .map(|d| -(d * sign))
        .fold(f64::NEG_INFINITY, f64::max);
    assert!(
        worst <= 1.,
        "the rendered edge moved backwards by {worst}px.\n{}",
        report("silhouette", &frames, Probe::Silhouette, sil_p)
    );
}

/// The target for the interior: content sampled `LINEAR` out of a texture should follow the
/// layout's real curve once the destination rect can hold a fraction, with no shader work at all.
///
/// Ignored until fractional placement lands. This is the cheaper half of the work and the one
/// that decides the rest: if it turns this green and the shimmer goes with it, the silhouette
/// coverage work is not needed.
#[test]
#[ignore = "fractional placement is not built yet; this is its acceptance criterion"]
fn an_interior_feature_slides_between_pixels() {
    let Some((mut f, output)) = overview_fixture() else {
        return;
    };
    let (_, p) = calibrate(&mut f, &output).expect("calibrate the probes");
    let frames = sample_edges(&mut f, &output);
    assert!(frames.len() > 8, "need a run of frames to judge");

    let swing = pk_pk(&residuals(&frames, Probe::Interior, p));
    assert!(
        swing < 0.25,
        "the interior drifted {swing:.3}px from where unrounded geometry puts it, which is the \
         destination rect being rounded under it.\n{}",
        report("interior", &frames, Probe::Interior, p)
    );
}

/// The target for the silhouette: an edge crossing the screen at a fraction of a pixel per frame
/// should sit where geometry puts it, not stall on a pixel and then jump.
///
/// Ignored until edge coverage lands. A fractional origin alone will not move this one — the clip
/// is a hard step — so this stays red until the shader can express partial coverage. Written now
/// so it cannot be quietly weakened to match whatever gets built.
#[test]
#[ignore = "edge coverage is not built yet; this is its acceptance criterion"]
fn an_animated_edge_moves_by_fractions_of_a_pixel() {
    let Some((mut f, output)) = overview_fixture() else {
        return;
    };
    let (p, _) = calibrate(&mut f, &output).expect("calibrate the probes");
    let frames = sample_edges(&mut f, &output);
    assert!(frames.len() > 8, "need a run of frames to judge smoothness");

    let swing = pk_pk(&residuals(&frames, Probe::Silhouette, p));
    assert!(
        swing < 0.25,
        "the rendered edge drifted {swing:.3}px from where unrounded geometry puts it: it is \
         pinned to whole pixels while the geometry sweeps between them.\n{}",
        report("silhouette", &frames, Probe::Silhouette, p)
    );
}
