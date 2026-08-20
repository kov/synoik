// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! GNOME Activities overview window-picker layout ("exposé").
//!
//! A port of gnome-shell's `UnalignedLayoutStrategy` (`js/ui/workspace.js`):
//! windows are packed into rows by their on-workspace position (rows by
//! vertical center, within a row by horizontal center, so previews stay near
//! their real windows), small windows are enlarged up to 1.5×, and the whole
//! grid is scaled to fit the area, capped at 95% of natural size. The number
//! of rows is chosen by scoring scale (weight 1) against area usage
//! (weight 0.1).
//!
//! This module is pure geometry: rects in, slot rects out (indexed like the
//! input), so the corpus can pin the math directly.

use smithay::utils::{Logical, Point, Rectangle, Size};

/// `WINDOW_PREVIEW_MAXIMUM_SCALE` (js/ui/workspace.js).
const MAXIMUM_SCALE: f64 = 0.95;

/// `.window-picker { spacing: $base_padding }` (`_window-picker.scss:5-8`), which is what
/// `WorkspaceLayout`'s `spacing` property actually resolves to — the 20px in the JS is only the
/// property default, overridden by the theme before any layout runs.
const CSS_SPACING: f64 = 6.;

/// `WINDOW_ACTIVE_SIZE_INC` (`windowPreview.js:20`): how far a preview grows in each direction
/// while it is the active/hovered one.
const ACTIVE_SIZE_INC: f64 = 5.;

/// `_adjustSpacingAndPadding` (`workspace.js:478-511`) adds the preview chrome's worst-case
/// overhang to the row and column spacing, so a close button or app icon never lands on the
/// neighbouring preview. GNOME takes the max over all four sides
/// (`chromeHeights`/`chromeWidths`, `windowPreview.js:277-308`):
///
/// * top / right — half the close button, which straddles the preview's top-right corner
/// * bottom — the part of the app icon hanging below the preview
/// * left — nothing but the active-size increase
///
/// plus `ACTIVE_SIZE_INC` on every side. The bottom (icon) side wins for our chrome.
fn chrome_oversize() -> f64 {
    use crate::ui::window_preview::{CLOSE_SIZE, ICON_OVERLAP, ICON_SIZE};

    let top = CLOSE_SIZE / 2.;
    let bottom = (1. - ICON_OVERLAP) * ICON_SIZE;
    let left = 0.;
    let right = CLOSE_SIZE / 2.;

    top.max(bottom).max(left).max(right) + ACTIVE_SIZE_INC
}

/// The effective inter-preview spacing: the theme's value plus the chrome overhang.
fn spacing() -> f64 {
    CSS_SPACING + chrome_oversize()
}
/// `LAYOUT_SCALE_WEIGHT` / `LAYOUT_SPACE_WEIGHT` (js/ui/workspace.js).
const SCALE_WEIGHT: f64 = 1.;
const SPACE_WEIGHT: f64 = 0.1;

/// `_computeWindowScale`: height matters most for row alignment, and small
/// windows get bumped up to 1.5× so a calculator remains clickable.
fn window_scale(window: Rectangle<f64, Logical>, monitor_height: f64) -> f64 {
    let ratio = window.size.h / monitor_height;
    // lerp(1.5, 1, ratio)
    1.5 + (1. - 1.5) * ratio
}

struct Row {
    /// Sum of unscaled (window-scale only) widths; no spacing.
    full_width: f64,
    /// Tallest window at window-scale; no spacing.
    full_height: f64,
    /// `full_width * scale` plus inter-window spacing.
    width: f64,
    /// `full_height * scale`.
    height: f64,
    x: f64,
    y: f64,
    additional_scale: f64,
    /// Indices into the input slice.
    windows: Vec<usize>,
}

/// The picker's *assignment*: which window goes in which row and column, and the scale the
/// row-count search settled on. Packing it into concrete rects is a separate step
/// ([`pack_grid`]), because a container resize must re-fit without reconsidering the
/// assignment — gnome-shell's `_layout` / `_windowSlots` split (`js/ui/workspace.js:668-681`).
struct GridLayout {
    rows: Vec<Row>,
    max_columns: usize,
    grid_width: f64,
    grid_height: f64,
    scale: f64,
    /// The monitor height the row widths and heights were summed at. Packing consults
    /// `window_scale` again, and must do so with the same one; gnome-shell likewise freezes
    /// the monitor into the strategy at `_createBestLayout` time.
    monitor_height: f64,
    /// How many windows the row index lists span. A grid only means anything against the
    /// slice it was built from: a shorter one indexes out of bounds, a longer one leaves
    /// windows at a default slot. A caller holding a grid across changes must therefore key
    /// its rows by window identity and recompute when they no longer line up — an equal
    /// count is not the same as the same windows, so the assert below is a backstop, not
    /// the check.
    window_count: usize,
}

/// `computeLayout`: distribute the windows into `num_rows` rows, aiming for
/// even row widths, preserving the vertical then horizontal on-workspace
/// order.
fn compute_layout(
    windows: &[Rectangle<f64, Logical>],
    order: &[usize],
    monitor_height: f64,
    num_rows: usize,
) -> GridLayout {
    let total_width: f64 = windows
        .iter()
        .map(|w| w.size.w * window_scale(*w, monitor_height))
        .sum();
    let ideal_row_width = total_width / num_rows as f64;

    let mut rows = Vec::with_capacity(num_rows);
    let mut idx = 0;
    for i in 0..num_rows {
        let mut row = Row {
            full_width: 0.,
            full_height: 0.,
            width: 0.,
            height: 0.,
            x: 0.,
            y: 0.,
            additional_scale: 1.,
            windows: Vec::new(),
        };

        while idx < order.len() {
            let window = windows[order[idx]];
            let s = window_scale(window, monitor_height);
            let width = window.size.w * s;
            let height = window.size.h * s;
            // Faithful quirk: the row height is bumped before deciding
            // whether the window actually stays in this row.
            row.full_height = f64::max(row.full_height, height);

            // `_keepSameRow`: fits outright, or gets the row width ratio
            // closer to the ideal; the last row takes everything left.
            let keep = row.full_width + width <= ideal_row_width || {
                let old_ratio = row.full_width / ideal_row_width;
                let new_ratio = (row.full_width + width) / ideal_row_width;
                (1. - new_ratio).abs() < (1. - old_ratio).abs()
            };
            if keep || i == num_rows - 1 {
                row.windows.push(order[idx]);
                row.full_width += width;
                idx += 1;
            } else {
                break;
            }
        }

        rows.push(row);
    }

    let mut grid_height = 0.;
    for row in rows.iter_mut() {
        // Sort windows horizontally to minimize travel distance.
        row.windows
            .sort_by(|&a, &b| center(windows[a]).x.total_cmp(&center(windows[b]).x));
        grid_height += row.full_height;
    }

    let mut max_row = 0;
    for i in 1..rows.len() {
        if rows[i].full_width > rows[max_row].full_width {
            max_row = i;
        }
    }

    GridLayout {
        max_columns: rows[max_row].windows.len(),
        grid_width: rows[max_row].full_width,
        grid_height,
        rows,
        scale: 1.,
        monitor_height,
        window_count: windows.len(),
    }
}

/// The scale that fits `extent` into `available`, never above `cap` —
/// `Math.min(cap, available / extent)`, but defined when there is no extent to
/// scale. Every "scale to fit" in this file goes through here; `cap` is 1 where
/// the step may only shrink, `MAXIMUM_SCALE` where it may also enlarge.
///
/// A grid dimension of zero is reachable: a row whose windows all scale to zero
/// height sums to `-0.`, and `656. / -0.` is `-f64::INFINITY`, which `min` then
/// picks over the cap. That negative-infinite scale spread through the slot sizes
/// into `expose_pickup_size` and came out the far end as a NaN tile position, an
/// assertion in `Layout::verify_invariants` away from being a window rendered
/// nowhere. GNOME divides the same way at every one of these sites
/// (`workspace.js:284-285`, `:320`, `:329`) and has the same hole; it just never
/// hands the grid a zero-extent row.
///
/// An axis with no extent constrains nothing, so it yields `cap` — the identity
/// for the `min` each caller folds it into.
fn scale_to_fit(available: f64, extent: f64, cap: f64) -> f64 {
    if extent <= 0. || !extent.is_finite() {
        return cap;
    }
    f64::min(cap, available / extent)
}

fn center(rect: Rectangle<f64, Logical>) -> Point<f64, Logical> {
    Point::from((rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.))
}

/// `computeScaleAndSpace`.
fn compute_scale_and_space(layout: &mut GridLayout, area: Rectangle<f64, Logical>) -> (f64, f64) {
    // Gaps-between-N: subtract in f64, never in the `usize` the count is held in. A row
    // can legitimately hold no windows — degenerate window sizes make `ideal_row_width`
    // NaN, which makes every `_keepSameRow` comparison false, so every window falls
    // through to the last row (`i == num_rows - 1`) and `max_row` stays at the empty row
    // 0, because `NaN > 0.` is false too. GNOME reaches the same state and takes
    // `(0 - 1) * spacing` in stride (`workspace.js:280`); only the port underflowed.
    // Every `len() - 1` spacing term in this file is the same shape — see
    // `_compute_row_sizes` and `compute_window_slots` below.
    let hspacing = (layout.max_columns as f64 - 1.) * spacing();
    let vspacing = (layout.rows.len() as f64 - 1.) * spacing();

    // Folding `MAXIMUM_SCALE` in per axis rather than once around the pair: `min` is
    // associative, so the result is unchanged, and it makes the cap the value an axis
    // with no extent contributes.
    let horizontal_scale = scale_to_fit(area.size.w - hspacing, layout.grid_width, MAXIMUM_SCALE);
    let vertical_scale = scale_to_fit(area.size.h - vspacing, layout.grid_height, MAXIMUM_SCALE);
    let scale = f64::min(horizontal_scale, vertical_scale);

    let scaled_width = layout.grid_width * scale + hspacing;
    let scaled_height = layout.grid_height * scale + vspacing;
    let space = (scaled_width * scaled_height) / (area.size.w * area.size.h);

    layout.scale = scale;
    (scale, space)
}

/// `_isBetterScaleAndSpace`.
fn is_better_scale_and_space(old_scale: f64, old_space: f64, scale: f64, space: f64) -> bool {
    let space_power = (space - old_space) * SPACE_WEIGHT;
    let scale_power = (scale - old_scale) * SCALE_WEIGHT;

    if scale > old_scale && space > old_space {
        true
    } else if scale > old_scale {
        scale_power > space_power
    } else if space > old_space {
        space_power > scale_power
    } else {
        false
    }
}

/// Compute the picker slot for each window: `windows` are the current rects
/// (workspace coordinates), `area` is the space the picker may use, and
/// `monitor_height` feeds the small-window enlargement. Returns one slot per
/// input window, same order.
///
/// The two halves are also callable apart ([`compute_grid`], [`pack_grid`]), for a caller
/// that wants to hold the assignment still and only re-pack it.
pub fn compute_slots(
    monitor_height: f64,
    area: Rectangle<f64, Logical>,
    windows: &[Rectangle<f64, Logical>],
) -> Vec<Rectangle<f64, Logical>> {
    let Some(mut grid) = compute_grid(monitor_height, area, windows) else {
        return Vec::new();
    };
    pack_grid(&mut grid, area, windows)
}

/// Decide the assignment: which window lands in which row, in what order, and at what
/// scale. `None` when there is nothing to lay out.
///
/// This half reads window positions and sizes both, and scores candidate row counts against
/// `area`. It is the half that must not run per frame: it is the only half that *orders*
/// anything, and both its sorts are stable with no tie-break, so any jitter in the input
/// rects re-orders the grid.
fn compute_grid(
    monitor_height: f64,
    area: Rectangle<f64, Logical>,
    windows: &[Rectangle<f64, Logical>],
) -> Option<GridLayout> {
    if windows.is_empty() {
        return None;
    }

    // Sort windows vertically to minimize travel distance; this decides the
    // row each window lands in.
    let mut order: Vec<usize> = (0..windows.len()).collect();
    order.sort_by(|&a, &b| center(windows[a]).y.total_cmp(&center(windows[b]).y));

    // `_createBestLayout`: try increasing row counts while the column count
    // still changes and the scale/space score keeps improving.
    let mut best: Option<GridLayout> = None;
    let mut last_num_columns = usize::MAX;
    let mut last_scale = 0.;
    let mut last_space = 0.;
    for num_rows in 1.. {
        let num_columns = windows.len().div_ceil(num_rows);
        if num_columns == last_num_columns {
            break;
        }

        let mut layout = compute_layout(windows, &order, monitor_height, num_rows);
        let (scale, space) = compute_scale_and_space(&mut layout, area);

        if best.is_some() && !is_better_scale_and_space(last_scale, last_space, scale, space) {
            break;
        }

        best = Some(layout);
        last_num_columns = num_columns;
        last_scale = scale;
        last_space = space;
    }
    // The `num_rows = 1` pass always lands: its column count cannot equal the `usize::MAX`
    // sentinel, and the score test is gated on a candidate already being held.
    Some(best.unwrap())
}

/// Pack an assignment into concrete slot rects, one per input window, in input order.
///
/// This half reads window **sizes** only, never positions, so it cannot re-order anything —
/// which is what makes it safe to run every frame over a held-still [`GridLayout`]. Packing
/// into an `area` other than the one the grid was searched at is allowed and intended: the
/// grid shifts and re-fits, and nothing moves between cells. Growth is then shrink-only
/// until the next assignment, because `additional_scale` caps at 1 — gnome-shell tolerates
/// the same staleness.
fn pack_grid(
    layout: &mut GridLayout,
    area: Rectangle<f64, Logical>,
    windows: &[Rectangle<f64, Logical>],
) -> Vec<Rectangle<f64, Logical>> {
    // The row index lists address this exact slice. A length mismatch is already a caller
    // bug by then; this only makes it loud in the build that can still be debugged.
    debug_assert_eq!(layout.window_count, windows.len());
    let monitor_height = layout.monitor_height;

    // `_computeRowSizes`.
    let scale = layout.scale;
    for row in &mut layout.rows {
        // `(row.windows.length - 1) * this._columnSpacing` (`workspace.js:188`) — an empty
        // row gives one negative spacing there, and must here too.
        row.width = row.full_width * scale + (row.windows.len() as f64 - 1.) * spacing();
        row.height = row.full_height * scale;
    }

    // `computeWindowSlots`.
    let rows = &mut layout.rows;
    let height_without_spacing: f64 = rows.iter().map(|row| row.height).sum();
    let vertical_spacing = (rows.len() as f64 - 1.) * spacing();
    let additional_vertical_scale =
        scale_to_fit(area.size.h - vertical_spacing, height_without_spacing, 1.);

    // Keep track of how much smaller the grid becomes due to scaling so it
    // can be centered again.
    let mut compensation = 0.;
    let mut y = 0.;
    for row in rows.iter_mut() {
        let horizontal_spacing = (row.windows.len() as f64 - 1.) * spacing();
        let width_without_spacing = row.width - horizontal_spacing;
        let additional_horizontal_scale =
            scale_to_fit(area.size.w - horizontal_spacing, width_without_spacing, 1.);

        if additional_horizontal_scale < additional_vertical_scale {
            row.additional_scale = additional_horizontal_scale;
            // Only consider the scaling in addition to the vertical scaling
            // for centering.
            compensation += (additional_vertical_scale - additional_horizontal_scale) * row.height;
        } else {
            row.additional_scale = additional_vertical_scale;
        }

        row.x = area.loc.x
            + f64::max(
                area.size.w - (width_without_spacing * row.additional_scale + horizontal_spacing),
                0.,
            ) / 2.;
        row.y = area.loc.y
            + f64::max(
                area.size.h - (height_without_spacing + vertical_spacing),
                0.,
            ) / 2.
            + y;
        y += row.height * row.additional_scale + spacing();
    }

    compensation /= 2.;

    let single_row = rows.len() == 1;
    let mut slots = vec![Rectangle::default(); windows.len()];
    for row in rows.iter() {
        let row_y = row.y + compensation;
        let row_height = row.height * row.additional_scale;

        let mut x = row.x;
        for &idx in &row.windows {
            let window = windows[idx];
            let mut s = scale * window_scale(window, monitor_height) * row.additional_scale;
            let cell_width = window.size.w * s;
            let cell_height = window.size.h * s;

            s = f64::min(s, MAXIMUM_SCALE);
            let clone_width = window.size.w * s;
            let clone_height = window.size.h * s;

            let clone_x = x + (cell_width - clone_width) / 2.;
            // A single row centers windows vertically inside the row;
            // multiple rows align them to the bottom edge of their row.
            let clone_y = if single_row {
                row_y + (row_height - clone_height) / 2.
            } else {
                row_y + row_height - cell_height
            };

            // Align with the pixel grid to prevent blurry windows at scale 1.
            slots[idx] = Rectangle::new(
                Point::from((clone_x.floor(), clone_y.floor())),
                Size::from((clone_width, clone_height)),
            );
            x += cell_width + spacing();
        }
    }

    slots
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> Rectangle<f64, Logical> {
        Rectangle::new(Point::from((x, y)), Size::from((w, h)))
    }

    fn area() -> Rectangle<f64, Logical> {
        rect(0., 0., 1920., 1080.)
    }

    /// A pinned input: a name, the monitor height, the area, and the window rects.
    type GoldenCase = (
        &'static str,
        f64,
        Rectangle<f64, Logical>,
        Vec<Rectangle<f64, Logical>>,
    );

    /// Inputs the golden table below pins. Each is a documented degenerate or a branch of
    /// the layout that a later change could silently move.
    fn golden_cases() -> Vec<GoldenCase> {
        vec![
            ("single", 1080., area(), vec![rect(100., 100., 800., 600.)]),
            (
                "exact centre tie",
                1080.,
                area(),
                vec![rect(400., 300., 700., 500.), rect(400., 300., 700., 500.)],
            ),
            (
                "single row, mixed aspects",
                1080.,
                area(),
                vec![
                    rect(0., 0., 800., 600.),
                    rect(900., 50., 640., 480.),
                    rect(1400., 20., 300., 300.),
                ],
            ),
            (
                "multi row",
                1080.,
                area(),
                vec![
                    rect(0., 0., 800., 600.),
                    rect(900., 50., 640., 480.),
                    rect(100., 500., 1200., 400.),
                    rect(1400., 600., 300., 300.),
                    rect(600., 300., 1920., 1080.),
                    rect(200., 800., 500., 900.),
                    rect(1500., 900., 960., 540.),
                    rect(700., 100., 1280., 720.),
                ],
            ),
            (
                "zero height",
                1080.,
                area(),
                vec![rect(0., 0., 800., 0.), rect(900., 0., 640., 0.)],
            ),
            (
                "fully degenerate",
                1080.,
                area(),
                vec![rect(0., 0., 0., 0.), rect(10., 10., 0., 0.)],
            ),
            (
                "negative window scale",
                200.,
                area(),
                vec![rect(0., 0., 400., 900.), rect(500., 0., 300., 200.)],
            ),
            (
                "offset area",
                1080.,
                rect(64., 48., 1600., 900.),
                vec![rect(0., 0., 800., 600.), rect(900., 50., 640., 480.)],
            ),
            (
                "tiny area",
                1080.,
                rect(0., 0., 200., 120.),
                vec![
                    rect(0., 0., 800., 600.),
                    rect(900., 50., 640., 480.),
                    rect(100., 500., 1200., 400.),
                ],
            ),
            (
                // A window wide enough to fail `_keepSameRow` from an empty row leaves the
                // row it broke out of empty, which is the negative-spacing term below.
                "empty middle row",
                1080.,
                rect(0., 0., 900., 700.),
                vec![
                    rect(0., 0., 60., 40.),
                    rect(0., 400., 1800., 200.),
                    rect(0., 800., 60., 40.),
                ],
            ),
            ("empty", 1080., area(), vec![]),
        ]
    }

    /// Slot values captured from this implementation, pinned field-by-field on their bit
    /// patterns.
    ///
    /// The layout is a port whose every constant and quirk is answerable to gnome-shell, so
    /// "it still computes what it computed" is the property worth holding: a refactor that
    /// moves a slot by an ulp has changed the picker. Every value here is the result of
    /// exactly-specified IEEE arithmetic, so the bits are reproducible; comparing them
    /// rather than the values is what makes a sign-of-zero or a single ulp fail.
    ///
    /// The degenerate cases are the finite ones — a zero extent, an empty row's negative
    /// spacing, a window tall enough to drive `window_scale` negative. Inputs that make the
    /// arithmetic itself `NaN` (a zero monitor height) are deliberately **not** pinned here:
    /// `NaN` bit patterns are not portable, so a golden row holding one would pin the
    /// platform rather than the layout.
    #[rustfmt::skip]
    const GOLDEN: &[&[(f64, f64, f64, f64)]] = &[
        &[(580.0, 255.0, 760.0, 570.0)],
        &[(190.0, 302.0, 665.0, 475.0), (1064.0, 302.0, 665.0, 475.0)],
        &[(32.0, 255.0, 760.0, 570.0), (896.0, 312.0, 608.0, 456.0), (1605.0, 397.0, 285.0, 285.0)],
        &[
            (350.0, 37.0, 344.36691613893686, 258.27518710420264),
            (725.0, 79.0, 288.01596622529263, 216.0119746689695),
            (241.0, 521.0, 555.6829783151027, 185.22765943836757),
            (827.0, 562.0, 143.81232009211286, 143.81232009211286),
            (1001.0, 326.0, 676.2113989637305, 380.36891191709844),
            (638.0, 736.0, 190.77144502014968, 343.3886010362694),
            (859.0, 842.0, 422.63212435233163, 237.73056994818654),
            (1043.0, 0.0, 525.9421991940127, 295.84248704663213),
        ],
        &[(579.0, 524.0, 760.0, 0.0), (656.0, 555.0, 608.0, 0.0)],
        &[(944.0, 524.0, 0.0, 0.0), (975.0, 524.0, 0.0, 0.0)],
        &[(944.0, 860.0, -284.99999999999994, -641.2499999999999), (690.0, 445.0, 285.0, 190.0)],
        &[(111.0, 213.0, 760.0, 570.0), (1002.0, 270.0, 608.0, 456.0)],
        &[
            (20.0, 0.0, 69.7270588235294, 52.29529411764705),
            (120.0, 8.0, 58.31717647058822, 43.73788235294116),
            (43.0, 82.0, 112.5141176470588, 37.50470588235294),
        ],
        &[
            (435.0, 253.0, 29.48474576271186, 19.65649717514124),
            (59.0, 426.0, 840.3152542372881, 93.3683615819209),
            (0.0, 500.0, 29.48474576271186, 19.65649717514124),
        ],
        &[],
    ];

    #[test]
    fn slots_match_the_golden_table() {
        let cases = golden_cases();
        // Without this a case added past the end of the table is silently never checked.
        assert_eq!(cases.len(), GOLDEN.len(), "every case needs a golden row");
        for ((name, mh, a, windows), golden) in cases.into_iter().zip(GOLDEN) {
            let slots = compute_slots(mh, a, &windows);
            assert_eq!(slots.len(), golden.len(), "{name}: slot count");
            for (i, (slot, want)) in slots.iter().zip(*golden).enumerate() {
                let got = (slot.loc.x, slot.loc.y, slot.size.w, slot.size.h);
                let bits = |t: &(f64, f64, f64, f64)| {
                    [t.0.to_bits(), t.1.to_bits(), t.2.to_bits(), t.3.to_bits()]
                };
                assert_eq!(
                    bits(&got),
                    bits(want),
                    "{name}: slot {i} is {got:?}, want {want:?}"
                );
            }
        }
    }

    /// The two halves compose back into [`compute_slots`] — which is trivially true while
    /// `compute_slots` *is* that composition, and stops being trivial the moment a caller
    /// holds a grid still across frames. It pins the property that makes holding one legal.
    #[test]
    fn packing_a_grid_reproduces_compute_slots() {
        for (name, mh, a, windows) in golden_cases() {
            let Some(mut grid) = compute_grid(mh, a, &windows) else {
                assert!(
                    windows.is_empty(),
                    "{name}: no grid for a non-empty workspace"
                );
                continue;
            };
            assert_eq!(grid.monitor_height, mh, "{name}");
            assert_eq!(grid.window_count, windows.len(), "{name}");

            // Re-packed, because retention packs the same grid again on every later frame,
            // and packed into a *different* area in between, because that is the whole
            // reason a grid is held: the row scratch fields must be written afresh each
            // time, never folded into what a previous area left behind.
            let once = pack_grid(&mut grid, a, &windows);
            let elsewhere = Rectangle::new(
                Point::from((a.loc.x + 37., a.loc.y - 11.)),
                Size::from((a.size.w * 0.6 + 5., a.size.h * 1.3 + 5.)),
            );
            let _ = pack_grid(&mut grid, elsewhere, &windows);
            let twice = pack_grid(&mut grid, a, &windows);
            let bits = |s: &Rectangle<f64, Logical>| {
                [
                    s.loc.x.to_bits(),
                    s.loc.y.to_bits(),
                    s.size.w.to_bits(),
                    s.size.h.to_bits(),
                ]
            };
            let want: Vec<_> = compute_slots(mh, a, &windows).iter().map(bits).collect();
            assert_eq!(
                once.iter().map(bits).collect::<Vec<_>>(),
                want,
                "{name}: first pack"
            );
            assert_eq!(
                twice.iter().map(bits).collect::<Vec<_>>(),
                want,
                "{name}: re-pack"
            );
        }
    }

    /// Hand-computed through the gnome-shell algorithm: window scale
    /// lerp(1.5, 1, 600/1080) = 1.2̅, grid 977.7̅ × 733.3̅, fit scale capped at
    /// 0.95, clone 760 × 570 centered in its cell and row.
    #[test]
    fn single_window_slot_matches_gnome_math() {
        let slots = compute_slots(1080., area(), &[rect(100., 100., 800., 600.)]);
        assert_eq!(slots.len(), 1);
        let s = slots[0];
        assert_eq!((s.loc.x, s.loc.y), (580., 255.));
        assert!((s.size.w - 760.).abs() < 1e-6 && (s.size.h - 570.).abs() < 1e-6);
    }

    /// Slots preserve the on-workspace spatial order: the left window's slot
    /// stays left of the right window's slot.
    #[test]
    fn slots_preserve_spatial_order() {
        let windows = [rect(1000., 200., 700., 500.), rect(50., 180., 700., 500.)];
        let slots = compute_slots(1080., area(), &windows);
        assert!(
            slots[1].loc.x < slots[0].loc.x,
            "left window must keep the left slot: {slots:?}"
        );
    }

    /// Slots fit within the area, never overlap, and never exceed 95% of the
    /// window's natural size (WINDOW_PREVIEW_MAXIMUM_SCALE).
    #[test]
    fn slots_fit_without_overlap() {
        let windows = [
            rect(0., 0., 800., 600.),
            rect(900., 50., 640., 480.),
            rect(100., 500., 1200., 400.),
            rect(1400., 600., 300., 300.),
            rect(600., 300., 1920., 1080.),
        ];
        let slots = compute_slots(1080., area(), &windows);
        for (i, (window, slot)) in windows.iter().zip(&slots).enumerate() {
            let area = area();
            assert!(
                slot.loc.x >= area.loc.x - 1.
                    && slot.loc.y >= area.loc.y - 1.
                    && slot.loc.x + slot.size.w <= area.loc.x + area.size.w + 1.
                    && slot.loc.y + slot.size.h <= area.loc.y + area.size.h + 1.,
                "slot {i} out of area: {slot:?}"
            );
            let scale = slot.size.w / window.size.w;
            assert!(scale <= MAXIMUM_SCALE + 1e-6, "slot {i} over max scale");
            for (j, other) in slots.iter().enumerate().skip(i + 1) {
                let disjoint = slot.loc.x + slot.size.w <= other.loc.x
                    || other.loc.x + other.size.w <= slot.loc.x
                    || slot.loc.y + slot.size.h <= other.loc.y
                    || other.loc.y + other.size.h <= slot.loc.y;
                assert!(disjoint, "slots {i} and {j} overlap: {slot:?} {other:?}");
            }
        }
    }
}
