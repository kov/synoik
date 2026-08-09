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

struct GridLayout {
    rows: Vec<Row>,
    max_columns: usize,
    grid_width: f64,
    grid_height: f64,
    scale: f64,
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
    }
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

    let horizontal_scale = (area.size.w - hspacing) / layout.grid_width;
    let vertical_scale = (area.size.h - vspacing) / layout.grid_height;
    let scale = f64::min(f64::min(horizontal_scale, vertical_scale), MAXIMUM_SCALE);

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
pub fn compute_slots(
    monitor_height: f64,
    area: Rectangle<f64, Logical>,
    windows: &[Rectangle<f64, Logical>],
) -> Vec<Rectangle<f64, Logical>> {
    if windows.is_empty() {
        return Vec::new();
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
    let mut layout = best.unwrap();

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
    let additional_vertical_scale = f64::min(
        1.,
        (area.size.h - vertical_spacing) / height_without_spacing,
    );

    // Keep track of how much smaller the grid becomes due to scaling so it
    // can be centered again.
    let mut compensation = 0.;
    let mut y = 0.;
    for row in rows.iter_mut() {
        let horizontal_spacing = (row.windows.len() as f64 - 1.) * spacing();
        let width_without_spacing = row.width - horizontal_spacing;
        let additional_horizontal_scale = f64::min(
            1.,
            (area.size.w - horizontal_spacing) / width_without_spacing,
        );

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
