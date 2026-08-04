// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The overview's control layout — gnome-shell's `ControlsManagerLayout`
//! (`js/ui/overviewControls.js`), as pure geometry.
//!
//! Every piece of overview chrome gets an allocated box, computed top-down
//! from the work area: the search entry at the top, the dash bottom-anchored,
//! the small workspace row across the top, and the window picker filling
//! whatever is left. The search results overlay the whole
//! middle strip (gnome-shell allocates its `searchController` the full space
//! between entry and dash, *without* subtracting the thumbnails, and
//! cross-fades it over them — `overviewControls.js:242-245`).
//!
//! **Divergence (approved 2026-08-03).** gnome-shell has two different rows: a thumbnail
//! strip in the window-picker state, and the picker itself shrunk into
//! `_computeWorkspacesBoxForState(APP_GRID)` in the app-grid state. Ours is **one** row —
//! [`ControlsLayout::workspace_row`] — allocated the same box in both states and drawn the
//! same way in both, so the show-apps transition never moves it. The picker fades away over
//! it instead of travelling into it. See `docs/fork/dynamic-workspaces-divergence.md`.
//!
//! This module is pure geometry: sizes in, boxes out, so the corpus can pin
//! the arithmetic directly. The measured heights it takes as inputs (search
//! entry, dash, thumbnails) are St theme-node lookups in gnome-shell; here
//! each owning widget publishes its own preferred height.
//!
//! Divergences from gnome-shell, deliberate:
//! - gnome-shell sizes the thumbnails from a `maxThumbnailScale` of the work-area porthole
//!   (`workspaceThumbnail.js:1204-1219,1248-1255`, clamped again at `overviewControls.js:190-192`),
//!   shrinking them further until the row fits. Ours are [`small_workspace_height`] — the app-grid
//!   row's workspace, whatever the count — and the row scrolls instead.
//! - the row's top sits at the search puck's vertical midline rather than below the entry's row, so
//!   it *overlaps* the floating entry (which is itself a divergence — see
//!   [`ControlsLayout::search_entry`]). GNOME's app-grid row clears the entry because the entry
//!   takes a full-width row there; ours does not, and tucking the row under the reserved height
//!   instead left it sitting oddly low on the canvas.
//! - gnome-shell lays out *secondary* monitors with `SecondaryMonitorDisplay`
//!   (`workspacesView.js:589-720`) — thumbnails and padding only, no dash or search — so their
//!   picker box, and now their zoom, differ from the primary's. We draw the full chrome on every
//!   output (as the dash and search already did), so every monitor gets the primary layout.
//!
//! The `state` axis ports gnome-shell's `ControlsState` (HIDDEN/WINDOW_PICKER/APP_GRID): the
//! workspaces and app-grid boxes are computed per integer state and interpolated by a fractional
//! `state`, exactly as `ControlsManagerLayout` blends its cached per-state boxes. Only the geometry
//! is ported here; driving the state (the show-apps toggle, the state adjustment/animation, and the
//! app-grid *view*) is a following slice.

use smithay::utils::{Logical, Rectangle, Size};

/// `DASH_MAX_HEIGHT_RATIO` (`overviewControls.js:22`): the dash never takes
/// more than this fraction of the work area.
const DASH_MAX_HEIGHT_RATIO: f64 = 0.16;
/// `VERTICAL_SPACING_RATIO` (`overviewControls.js:23`): all vertical spacing in
/// the overview is this fraction of the work-area height.
const VERTICAL_SPACING_RATIO: f64 = 0.02;
/// `THUMBNAILS_SPACING_ADJUSTMENT_BOTTOM` (`overviewControls.js:25`): how much of one
/// spacing goes below the workspace row, between it and the window picker.
///
/// **Divergence (approved 2026-07-29).** gnome-shell's is `0.4`, splitting one spacing
/// 60/40 around the strip. Ours is bigger: at the app-grid row's size the row is one of
/// real workspaces rather than a thin ribbon, and 40% of a spacing left it crowding the
/// picker below it.
const ROW_SPACING_ADJUSTMENT_BOTTOM: f64 = 1.2;
/// `SMALL_WORKSPACE_RATIO` (`overviewControls.js:21`): in the app-grid state the
/// window picker shrinks to this fraction of the work-area height, a thin strip
/// under the search entry, and the app grid fills the space below it.
const SMALL_WORKSPACE_RATIO: f64 = 0.15;

/// How tall one workspace is in the row: [`SMALL_WORKSPACE_RATIO`] of the work area.
///
/// Published because the row is the same size in both states, and the layout module is not
/// the only thing that needs it — the workspace zoom the row is drawn at derives from it.
pub fn small_workspace_height(view_size: Size<f64, Logical>, start_y: f64) -> f64 {
    ((view_size.h - start_y) * SMALL_WORKSPACE_RATIO).round()
}

/// **Divergence (approved 2026-07-28).** How far the floating search entry is inset from the
/// right edge of the work area — `.search-entry`'s own `margin-top` (`$base_padding*2`),
/// reused as the edge gap so the pill sits the same distance from both edges it touches.
///
/// **Adaptive chrome, rule 1 — ramped**: a fixed logical constant, so it multiplies by
/// [`chrome_ramp`].
const SEARCH_ENTRY_EDGE_MARGIN: f64 = 12.;

/// The reference canvas: the smallest logical size GNOME's fixed chrome constants still
/// read correctly on. Above it nothing changes; below it [`chrome_ramp`] shrinks.
const REFERENCE_CANVAS: (f64, f64) = (1280., 800.);

/// The floor: chrome may shrink to half GNOME's constants and no further, so hit targets
/// and rings stay usable on an absurd canvas.
const CHROME_RAMP_FLOOR: f64 = 0.5;

/// **Divergence (approved 2026-07-26, `docs/fork/adaptive-overview-chrome.md`).** How far
/// the overview's chrome shrinks on this canvas: `1.0` on anything at least as big as
/// [`REFERENCE_CANVAS`], down to [`CHROME_RAMP_FLOOR`].
///
/// gnome-shell is *not* adaptive — the dash icon is a flat 64 (`dash.js:321`), the
/// workspace background radius a flat 30 (`workspace.js:30`), the picker gap clamps a flat
/// 24..80 (`workspacesView.js:22-23`) — and that reads fine only because it assumes a
/// canvas of roughly 1280x800 or more. On a 1024x665 one it produces a dash wider than
/// half the screen over an app grid whose own icons have laddered down to 32, and a
/// near-circular corner on a preview a couple of hundred px tall.
///
/// Two rules decide how a piece uses this, and mixing them per-widget is the thing to
/// avoid: chrome whose box is a fixed logical constant multiplies that constant by the
/// ramp, while chrome whose box already scales with the canvas derives its radii and
/// spacing from *its own box* instead (so its shape is scale-invariant, which a ramped
/// constant would not be). The top panel and every text size are exempt — the panel is the
/// one fixed landmark, and text size is a readability constant whose knob is
/// `text-scaling-factor`, not the canvas.
pub fn chrome_ramp(view_size: Size<f64, Logical>) -> f64 {
    let w = view_size.w / REFERENCE_CANVAS.0;
    let h = view_size.h / REFERENCE_CANVAS.1;
    w.min(h).clamp(CHROME_RAMP_FLOOR, 1.)
}

/// gnome-shell's `ControlsState` (`overviewControls.js:32-36`) as a continuous
/// axis: `HIDDEN` 0, `WINDOW_PICKER` 1, `APP_GRID` 2. [`layout`] takes a fractional
/// value and interpolates the state-dependent boxes (workspaces + app grid) between
/// the two bracketing integer states, exactly as `ControlsManagerLayout` blends its
/// cached per-state boxes by the state-adjustment progress.
pub mod state {
    pub const HIDDEN: f64 = 0.;
    pub const WINDOW_PICKER: f64 = 1.;
    pub const APP_GRID: f64 = 2.;
}

/// The allocated box of every overview control, in view (output) coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ControlsLayout {
    /// The search entry bin — the entry is centered in it and inset by its own
    /// margins (`overviewControls.js:164-169`).
    ///
    /// **Divergence (approved 2026-07-28):** gnome-shell gives the bin the full width and
    /// its own row at the top of the work area, pushing everything below it down. Ours is
    /// exactly pill-wide and *floats* at the top right, over whatever is behind it, so the
    /// strip and the picker start at the top of the work area instead.
    pub search_entry: Rectangle<f64, Logical>,
    /// The small workspace row: gnome-shell's thumbnails band
    /// (`overviewControls.js:184-196`) and its app-grid workspaces box
    /// (`_computeWorkspacesBoxForState(APP_GRID)`, `:80-110`), which we allocate as **one**
    /// box because they are one row here.
    ///
    /// Full width, and **state-independent**: the show-apps transition must not move it,
    /// or the row the user is pointing at slides out from under them for no reason.
    pub workspace_row: Rectangle<f64, Logical>,
    /// The dash, bottom-anchored to the work area (`overviewControls.js:172-182`).
    pub dash: Rectangle<f64, Logical>,
    /// The search results strip: everything between the entry and the dash,
    /// overlapping thumbnails and picker (`overviewControls.js:242-245`).
    pub search_results: Rectangle<f64, Logical>,
    /// The window picker — the workspaces box (`_computeWorkspacesBoxForState`,
    /// `overviewControls.js:80-110`) on its `HIDDEN` → `WINDOW_PICKER` leg.
    ///
    /// **Divergence (approved 2026-08-03):** gnome-shell's `APP_GRID` box shrinks this into
    /// the small top strip. Ours does not move on that leg at all — [`Self::workspace_row`]
    /// is already drawn there, and the picker fades away over it instead of travelling into
    /// a row that exists either way.
    pub workspaces: Rectangle<f64, Logical>,
    /// How far this canvas shrinks GNOME's fixed chrome constants ([`chrome_ramp`]) — 1
    /// on anything at or above the reference canvas. Carried here so the divergence is
    /// readable off the same layout model as the boxes it produced.
    pub chrome_ramp: f64,
    /// The app grid — the state-interpolated app-display box
    /// (`_getAppDisplayBoxForState`, `overviewControls.js:112-138`). Parked at the
    /// work-area bottom (off-screen below) in `HIDDEN`/`WINDOW_PICKER`, it slides up
    /// to fill the space under the shrunken picker in `APP_GRID`.
    pub app_display: Rectangle<f64, Logical>,
}

/// What each owning widget publishes about itself, which is all [`layout`] knows about
/// them — the St theme-node lookups gnome-shell's `ControlsManagerLayout` performs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Measured {
    /// `searchEntryBin`'s preferred height: the pill plus its margins.
    pub search_entry_height: f64,
    /// The pill's own width. Needed because the entry floats right rather than
    /// centering in a full-width bin (see [`ControlsLayout::search_entry`]).
    pub search_entry_width: f64,
    /// How far the entry *control's* vertical middle sits below the top of its bin — the
    /// bin is taller than the control by the theme's margins, and the row is anchored to
    /// the control, not to the bin.
    pub search_entry_mid_y: f64,
    /// The dash's preferred height, before the work-area cap.
    pub dash_preferred_height: f64,
}

/// Lays out the overview chrome.
///
/// `start_y` is the top of the work area (the top panel's strut); gnome-shell
/// shifts its whole box down by it (`box.y1 += startY`, `overviewControls.js:157`)
/// so every height below is the *work-area* height, not the view height.
pub fn layout(
    view_size: Size<f64, Logical>,
    start_y: f64,
    measured: Measured,
    state: f64,
) -> ControlsLayout {
    let Measured {
        search_entry_height,
        search_entry_width,
        search_entry_mid_y,
        dash_preferred_height,
    } = measured;
    let width = view_size.w;
    let height = view_size.h - start_y;
    let spacing = (height * VERTICAL_SPACING_RATIO).round();

    // The entry floats at the top right instead of taking a row (divergence, see
    // `ControlsLayout::search_entry`). `search_h` therefore no longer displaces the strip or
    // the picker; it is still reserved by the two *full-width* content surfaces — the search
    // results and the app grid — which would otherwise run under the pill.
    let search_h = search_entry_height;
    let entry_margin = (SEARCH_ENTRY_EDGE_MARGIN * chrome_ramp(view_size)).round();
    let search_entry = rect(
        width - entry_margin - search_entry_width,
        start_y,
        search_entry_width,
        search_h,
    );

    // The dash is capped at a fraction of the work area, then bottom-anchored.
    let max_dash_h = (height * DASH_MAX_HEIGHT_RATIO).round();
    let dash_h = dash_preferred_height.min(max_dash_h);
    let dash = rect(0., start_y + height - dash_h, width, dash_h);

    // The one workspace row, in both states: full width, its top on the entry control's
    // midline. It deliberately overlaps the floating entry rather than clearing it — see
    // the module divergence list.
    let row_h = small_workspace_height(view_size, start_y);
    let workspace_row = rect(0., start_y + search_entry_mid_y, width, row_h);
    let row_bottom = workspace_row.loc.y + row_h;
    let spacing_bottom = (spacing * ROW_SPACING_ADJUSTMENT_BOTTOM).round();

    // The workspaces (picker) box per integer `ControlsState`
    // (`_computeWorkspacesBoxForState`, `overviewControls.js:80-110`). `APP_GRID` is
    // deliberately absent: the picker does not travel on that leg (see
    // `ControlsLayout::workspaces`), so it keeps its window-picker box there.
    let workspaces_for = |s: f64| -> Rectangle<f64, Logical> {
        if s == state::HIDDEN {
            // The whole work area (the live desktop behind the overview).
            rect(0., start_y, width, height)
        } else {
            // No `search_h` term: the entry floats, so the picker's band starts at the top
            // of the work area rather than below a row it no longer occupies — but the
            // workspace row is drawn across that top, so the picker starts under it.
            let y = row_bottom + spacing_bottom;
            let h = start_y + height - dash_h - spacing - y;
            rect(0., y, width, h.max(0.))
        }
    };

    // The app-grid box per state (`_getAppDisplayBoxForState`,
    // `overviewControls.js:112-138`): it fills from under the workspace row down to the
    // dash, and is parked at the work-area bottom (`box.y2`) until the APP_GRID state
    // slides it up.
    let app_h = (start_y + height - dash_h - spacing - (row_bottom + spacing)).max(0.);
    let app_display_for = |s: f64| -> Rectangle<f64, Logical> {
        let y = if s == state::APP_GRID {
            row_bottom + spacing
        } else {
            start_y + height // box.y2 — parked below the work area
        };
        rect(0., y, width, app_h)
    };

    // Interpolate the state-dependent boxes between the two bracketing states.
    //
    // Divergence from GNOME: `overviewControls.js` interpolates against the
    // *transition* bracket recorded when the state animation started (so a
    // direct HIDDEN->APP_GRID move blends those two endpoints). We bracket by
    // `floor(state)`/`ceil(state)` instead. The two agree for every adjacent
    // transition (HIDDEN<->WINDOW_PICKER, WINDOW_PICKER<->APP_GRID), which is
    // all our state machine drives; they would differ only for a direct
    // two-step move through the middle state, which we never animate.
    let s = state.clamp(state::HIDDEN, state::APP_GRID);
    let lo = s.floor();
    let hi = s.ceil();
    let t = s - lo;
    let workspaces = lerp_rect(workspaces_for(lo), workspaces_for(hi), t);
    let app_display = lerp_rect(app_display_for(lo), app_display_for(hi), t);

    // Note: the workspace row's height is deliberately *not* subtracted here.
    let results_h = height - search_h - spacing - dash_h - spacing;
    let search_results = rect(0., start_y + search_h + spacing, width, results_h.max(0.));

    ControlsLayout {
        search_entry,
        workspace_row,
        dash,
        search_results,
        chrome_ramp: chrome_ramp(view_size),
        workspaces,
        app_display,
    }
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> Rectangle<f64, Logical> {
    Rectangle::new((x, y).into(), (w, h).into())
}

/// Linearly interpolate two boxes (`ClutterActorBox.interpolate`,
/// `overviewControls.js:135,169`).
fn lerp_rect(
    a: Rectangle<f64, Logical>,
    b: Rectangle<f64, Logical>,
    t: f64,
) -> Rectangle<f64, Logical> {
    let l = |x: f64, y: f64| x + (y - x) * t;
    rect(
        l(a.loc.x, b.loc.x),
        l(a.loc.y, b.loc.y),
        l(a.size.w, b.size.w),
        l(a.size.h, b.size.h),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ramp: 1 on every canvas GNOME's own constants were written for, monotone
    /// below that, floored, and never above 1 — this divergence only ever *shrinks*.
    #[test]
    fn the_chrome_ramp_is_monotone_clamped_and_neutral_at_the_reference() {
        let r = |w: f64, h: f64| chrome_ramp(Size::from((w, h)));

        assert_eq!(r(1280., 800.), 1., "neutral at the reference canvas");
        assert_eq!(r(1920., 1080.), 1., "and on anything bigger");
        assert_eq!(r(3840., 2160.), 1.);

        // The axis that runs out first drives it, so a wide-but-short canvas ramps too.
        assert_eq!(r(1920., 600.), 0.75, "height binds");
        assert_eq!(r(640., 1080.), 0.5, "width binds, and the floor holds");
        assert_eq!(r(200., 200.), CHROME_RAMP_FLOOR, "floored, never zero");

        // Monotone in both axes.
        let mut prev = 0.;
        for h in [400., 500., 600., 700., 800., 900.] {
            let cur = r(1920., h);
            assert!(
                cur >= prev,
                "ramp must not go down: {h} gave {cur} after {prev}"
            );
            prev = cur;
        }

        // The canvas this divergence was written for (2048x1330 @ 2).
        assert_eq!(r(1024., 665.), 0.8);
    }

    /// The heights the fork's own widgets publish, so the expectations below
    /// are hand-derived rather than observed.
    const SEARCH_H: f64 = 74.; // margin-top 12 + puck 56 + margin-bottom 6
    const SEARCH_MID: f64 = 40.; // margin-top 12 + puck 56 / 2
    const SEARCH_W: f64 = 352.; // `.search-entry` 24em, unramped at the reference and above
    const DASH_H: f64 = 112.; // pill 100 + edge offset 12

    fn measured() -> Measured {
        Measured {
            search_entry_height: SEARCH_H,
            search_entry_width: SEARCH_W,
            search_entry_mid_y: SEARCH_MID,
            dash_preferred_height: DASH_H,
        }
    }

    fn layout_1080() -> ControlsLayout {
        layout_1080_state(state::WINDOW_PICKER)
    }

    fn layout_1080_state(state: f64) -> ControlsLayout {
        layout(Size::from((1920., 1080.)), 35., measured(), state)
    }

    /// The entry floats at the top right instead of taking a row, and the workspace row
    /// runs the full width *under* it, its top on the puck's midline.
    #[test]
    fn the_search_entry_floats_right_and_takes_no_row() {
        let l = layout_1080();

        // Right-anchored, exactly pill-wide, at the very top of the work area.
        assert_eq!(
            l.search_entry,
            rect(1920. - 12. - 352., 35., 352., SEARCH_H)
        );

        // Nothing below it is displaced by its height: the row starts at the control's
        // midline, where GNOME would have put it below a full 74px row.
        assert_eq!(l.workspace_row.loc.y, 35. + SEARCH_MID);
        assert!(l.workspace_row.loc.y < 35. + SEARCH_H);

        // Full width, both edges.
        assert_eq!(l.workspace_row.loc.x, 0.);
        assert_eq!(l.workspace_row.size.w, 1920.);

        // So the pill and the row overlap, in both axes — that is what "floating" means,
        // and the row is no longer inset to dodge it.
        assert!(l.search_entry.loc.x < l.workspace_row.loc.x + l.workspace_row.size.w);
        assert!(l.search_entry.loc.y + l.search_entry.size.h > l.workspace_row.loc.y);
    }

    /// The two *full-width content* surfaces still clear the pill — a grid of app icons or
    /// of search results running under it would be unreadable. The workspace row is the
    /// deliberate exception (it is scenery, and it is what the row's placement asks for).
    #[test]
    fn full_width_surfaces_still_reserve_the_entry_height() {
        let l = layout_1080_state(state::APP_GRID);
        assert!(l.search_results.loc.y >= 35. + SEARCH_H);
        assert!(l.app_display.loc.y >= 35. + SEARCH_H);
    }

    /// 1920×1080 with the 35px panel strut: work area 1045 tall, so
    /// `spacing = round(1045 × 0.02) = 21` and `round(21 × 1.2) = 25` below the row.
    #[test]
    fn the_overview_boxes_at_the_reference_canvas() {
        let l = layout_1080();

        assert_eq!(l.search_entry, rect(1556., 35., 352., SEARCH_H));
        // 35 + 40, full width, round(1045 × 0.15) tall.
        assert_eq!(l.workspace_row, rect(0., 75., 1920., 157.));
        // Bottom-anchored: 35 + 1045 − 112. Unchanged from the pre-allocator
        // hardcoded anchor (1080 − 12 − 100), which is the point.
        assert_eq!(l.dash, rect(0., 968., 1920., 112.));
        // 75 + 157 + 25, and 968 − 21 − 257.
        assert_eq!(l.workspaces, rect(0., 257., 1920., 690.));
        // 35 + 74 + 21, and 1045 − 74 − 21 − 112 − 21. Spans the row and the picker
        // both — gnome-shell cross-fades, it does not carve.
        assert_eq!(l.search_results, rect(0., 130., 1920., 817.));
    }

    /// The dash cap is a fraction of the *work area*, and it bites before the
    /// dash would eat the picker on a short screen.
    #[test]
    fn dash_is_capped_at_a_fraction_of_the_work_area() {
        // round(565 × 0.16) = 90 < the 112 the dash would like.
        let l = layout(
            Size::from((1024., 600.)),
            35.,
            measured(),
            state::WINDOW_PICKER,
        );
        assert_eq!(l.dash.size.h, 90.);
        assert_eq!(l.dash.loc.y, 600. - 90.);

        // An oversized dash on a tall screen is capped too, not merely clamped
        // to its own preference.
        let l = layout(
            Size::from((1920., 1080.)),
            35.,
            Measured {
                dash_preferred_height: 400.,
                ..measured()
            },
            state::WINDOW_PICKER,
        );
        assert_eq!(l.dash.size.h, 167.);
    }

    /// Every height derives from the work-area height, never the view height:
    /// at a second resolution the spacing follows the strut, not the panel.
    #[test]
    fn spacing_follows_the_work_area_not_the_view() {
        let l = layout(
            Size::from((2560., 1440.)),
            35.,
            measured(),
            state::WINDOW_PICKER,
        );

        // work area 1405 ⇒ spacing = round(28.1) = 28, below the row round(33.6) = 34.
        assert_eq!(l.search_entry, rect(2560. - 364., 35., 352., SEARCH_H));
        // round(1405 × 0.15) = 211 tall, at 35 + 40.
        assert_eq!(l.workspace_row, rect(0., 75., 2560., 211.));
        assert_eq!(l.dash, rect(0., 1440. - 112., 2560., 112.));
        // 75 + 211 + 34, and 1440 − 112 − 28 − 320.
        assert_eq!(l.workspaces, rect(0., 320., 2560., 980.));
        assert_eq!(l.search_results, rect(0., 137., 2560., 1163.));
    }

    /// Without a panel strut everything shifts up by exactly the strut.
    #[test]
    fn no_strut_starts_at_the_view_top() {
        let l = layout(
            Size::from((1920., 1080.)),
            0.,
            measured(),
            state::WINDOW_PICKER,
        );

        assert_eq!(l.search_entry.loc.y, 0.);
        assert_eq!(l.dash.loc.y, 1080. - 112.);
        // spacing = round(1080 × 0.02) = 22, below the row round(26.4) = 26.
        assert_eq!(l.workspace_row, rect(0., 40., 1920., 162.));
        assert_eq!(l.workspaces, rect(0., 228., 1920., 718.));
    }

    /// In the app-grid state the app grid fills from under the workspace row down to the
    /// dash. The row itself does not move, and neither does the picker box — the picker
    /// fades away rather than travelling into the row.
    #[test]
    fn the_app_grid_state_only_moves_the_app_grid() {
        let picker = layout_1080();
        let grid = layout_1080_state(state::APP_GRID);

        assert_eq!(grid.workspace_row, picker.workspace_row);
        assert_eq!(grid.workspaces, picker.workspaces);

        // 232 + 21, and 968 − 21 − 253.
        assert_eq!(grid.app_display, rect(0., 253., 1920., 694.));
        // Parked at box.y2 = start_y + work-area height = 35 + 1045, same height.
        assert_eq!(picker.app_display, rect(0., 1080., 1920., 694.));
    }

    /// A fractional state slides the app grid up, and moves nothing else: the row and the
    /// picker are both state-independent on that leg.
    #[test]
    fn state_interpolates_the_app_grid_alone() {
        let mid = layout_1080_state(1.5);
        let picker = layout_1080();
        let grid = layout_1080_state(state::APP_GRID);

        assert_eq!(mid.workspace_row, picker.workspace_row);
        assert_eq!(mid.workspaces, picker.workspaces);
        assert_eq!(
            mid.app_display.loc.y,
            (picker.app_display.loc.y + grid.app_display.loc.y) / 2.
        );
    }

    /// The `HIDDEN` leg is the one the picker box still travels on: from the whole work
    /// area (the live desktop) up to its window-picker band.
    #[test]
    fn the_hidden_leg_still_moves_the_picker() {
        let hidden = layout_1080_state(state::HIDDEN);
        let picker = layout_1080();
        let mid = layout_1080_state(0.5);

        assert_eq!(hidden.workspaces, rect(0., 35., 1920., 1045.));
        assert_eq!(mid.workspaces.loc.y, (35. + picker.workspaces.loc.y) / 2.);
    }
}
