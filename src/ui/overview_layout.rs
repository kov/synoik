//! The overview's control layout — gnome-shell's `ControlsManagerLayout`
//! (`js/ui/overviewControls.js`), as pure geometry.
//!
//! Every piece of overview chrome gets an allocated box, computed top-down
//! from the work area: the search entry at the top, the dash bottom-anchored,
//! the workspace thumbnails in the band just below the entry, and the window
//! picker filling whatever is left. The search results overlay the whole
//! middle strip (gnome-shell allocates its `searchController` the full space
//! between entry and dash, *without* subtracting the thumbnails, and
//! cross-fades it over them — `overviewControls.js:242-245`).
//!
//! This module is pure geometry: sizes in, boxes out, so the corpus can pin
//! the arithmetic directly. The measured heights it takes as inputs (search
//! entry, dash, thumbnails) are St theme-node lookups in gnome-shell; here
//! each owning widget publishes its own preferred height.
//!
//! Divergences from gnome-shell, deliberate:
//! - gnome-shell measures the thumbnails against the *work area* porthole
//!   (`workspaceThumbnail.js:1204-1219,1248-1255`); our workspace miniature is the whole view (it
//!   includes the strip under the top panel), so [`crate::layout::thumbnails`] measures against the
//!   view. At 1920×1080 that is 54px where gnome-shell has 52. gnome-shell additionally clamps that
//!   measurement at `height × maxThumbnailScale` (`overviewControls.js:190-192`); ours is already
//!   the cap by construction, so there is nothing left to clamp.
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
/// `THUMBNAILS_SPACING_ADJUSTMENT_TOP` (`overviewControls.js:24`): the
/// thumbnails sit closer to the search entry than a full spacing.
const THUMBNAILS_SPACING_ADJUSTMENT_TOP: f64 = 0.6;
/// `THUMBNAILS_SPACING_ADJUSTMENT_BOTTOM` (`overviewControls.js:25`): and the
/// rest of the spacing goes below them.
const THUMBNAILS_SPACING_ADJUSTMENT_BOTTOM: f64 = 0.4;
/// `SMALL_WORKSPACE_RATIO` (`overviewControls.js:21`): in the app-grid state the
/// window picker shrinks to this fraction of the work-area height, a thin strip
/// under the search entry, and the app grid fills the space below it.
const SMALL_WORKSPACE_RATIO: f64 = 0.15;

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
    /// The search entry bin — full width, the entry itself is centered in it
    /// and inset by its own margins (`overviewControls.js:164-169`).
    pub search_entry: Rectangle<f64, Logical>,
    /// The workspace thumbnails strip band (`overviewControls.js:184-196`).
    pub thumbnails: Rectangle<f64, Logical>,
    /// The dash, bottom-anchored to the work area (`overviewControls.js:172-182`).
    pub dash: Rectangle<f64, Logical>,
    /// The search results strip: everything between the entry and the dash,
    /// overlapping thumbnails and picker (`overviewControls.js:242-245`).
    pub search_results: Rectangle<f64, Logical>,
    /// The window picker — the state-interpolated workspaces box
    /// (`_computeWorkspacesBoxForState`, `overviewControls.js:80-110`). At
    /// `WINDOW_PICKER` it fills the band; at `APP_GRID` it shrinks to the small
    /// top strip.
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

/// Lays out the overview chrome.
///
/// `start_y` is the top of the work area (the top panel's strut); gnome-shell
/// shifts its whole box down by it (`box.y1 += startY`, `overviewControls.js:157`)
/// so every height below is the *work-area* height, not the view height.
///
/// `expand_fraction` is `ThumbnailsBox.expandFraction` (0 while the strip is
/// hidden, 1 while shown, eased between): it scales both the thumbnails' own
/// height and the spacing below them, so the picker grows into the band
/// smoothly instead of popping.
pub fn layout(
    view_size: Size<f64, Logical>,
    start_y: f64,
    search_entry_height: f64,
    dash_preferred_height: f64,
    thumbnails_preferred_height: f64,
    expand_fraction: f64,
    state: f64,
) -> ControlsLayout {
    let width = view_size.w;
    let height = view_size.h - start_y;
    let spacing = (height * VERTICAL_SPACING_RATIO).round();

    let search_h = search_entry_height;
    let search_entry = rect(0., start_y, width, search_h);

    // The dash is capped at a fraction of the work area, then bottom-anchored.
    let max_dash_h = (height * DASH_MAX_HEIGHT_RATIO).round();
    let dash_h = dash_preferred_height.min(max_dash_h);
    let dash = rect(0., start_y + height - dash_h, width, dash_h);

    let thumbs_h = thumbnails_preferred_height * expand_fraction;
    let spacing_top = (spacing * THUMBNAILS_SPACING_ADJUSTMENT_TOP).round();
    let spacing_bottom = (spacing * THUMBNAILS_SPACING_ADJUSTMENT_BOTTOM).round() * expand_fraction;
    let thumbnails = rect(0., start_y + search_h + spacing_top, width, thumbs_h);

    // The workspaces (picker) box per integer `ControlsState`
    // (`_computeWorkspacesBoxForState`, `overviewControls.js:80-110`).
    let workspaces_for = |s: f64| -> Rectangle<f64, Logical> {
        if s == state::APP_GRID {
            // A thin strip under the entry; the app grid fills the rest.
            rect(
                0.,
                start_y + search_h + spacing,
                width,
                (height * SMALL_WORKSPACE_RATIO).round(),
            )
        } else if s == state::WINDOW_PICKER {
            let y = start_y + search_h + spacing_top + thumbs_h + spacing_bottom;
            let h = height - dash_h - spacing - search_h - spacing_top - thumbs_h - spacing_bottom;
            rect(0., y, width, h.max(0.))
        } else {
            // HIDDEN: the whole work area (the live desktop behind the overview).
            rect(0., start_y, width, height)
        }
    };

    // The app-grid box per state (`_getAppDisplayBoxForState`,
    // `overviewControls.js:112-138`): its height uses the APP_GRID picker height,
    // and it is parked at the work-area bottom (`box.y2`) until the APP_GRID state
    // slides it up.
    let app_grid_ws_h = workspaces_for(state::APP_GRID).size.h;
    let app_h = (height - search_h - spacing - app_grid_ws_h - spacing - dash_h - spacing).max(0.);
    let app_display_for = |s: f64| -> Rectangle<f64, Logical> {
        let y = if s == state::APP_GRID {
            start_y + search_h + spacing + app_grid_ws_h + spacing
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

    // Note: the thumbnails height is deliberately *not* subtracted here.
    let results_h = height - search_h - spacing - dash_h - spacing;
    let search_results = rect(0., start_y + search_h + spacing, width, results_h.max(0.));

    ControlsLayout {
        search_entry,
        thumbnails,
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
    const SEARCH_H: f64 = 58.; // margin-top 12 + entry 40 + margin-bottom 6
    const DASH_H: f64 = 112.; // pill 100 + edge offset 12
    const THUMBS_H: f64 = 54.; // 1080 × MAX_THUMBNAIL_SCALE

    fn layout_1080(expand: f64) -> ControlsLayout {
        layout_1080_state(expand, state::WINDOW_PICKER)
    }

    fn layout_1080_state(expand: f64, state: f64) -> ControlsLayout {
        layout(
            Size::from((1920., 1080.)),
            35.,
            SEARCH_H,
            DASH_H,
            THUMBS_H,
            expand,
            state,
        )
    }

    /// 1920×1080 with the 35px panel strut: work area 1045 tall, so
    /// `spacing = round(1045 × 0.02) = 21`, `round(21 × 0.6) = 13` above the
    /// thumbnails and `round(21 × 0.4) = 8` below.
    #[test]
    fn expanded_thumbnails_boxes() {
        let l = layout_1080(1.);

        assert_eq!(l.search_entry, rect(0., 35., 1920., 58.));
        // 35 + 58 + 13
        assert_eq!(l.thumbnails, rect(0., 106., 1920., 54.));
        // Bottom-anchored: 35 + 1045 − 112. Unchanged from the pre-allocator
        // hardcoded anchor (1080 − 12 − 100), which is the point.
        assert_eq!(l.dash, rect(0., 968., 1920., 112.));
        // 106 + 54 + 8, and 1045 − 112 − 21 − 58 − 13 − 54 − 8.
        assert_eq!(l.workspaces, rect(0., 168., 1920., 779.));
        // 35 + 58 + 21, and 1045 − 58 − 21 − 112 − 21. Spans the thumbnails
        // and the picker both — gnome-shell cross-fades, it does not carve.
        assert_eq!(l.search_results, rect(0., 114., 1920., 833.));
    }

    /// With the strip collapsed the picker takes the whole band back: no
    /// thumbnails height and no spacing below them.
    #[test]
    fn collapsed_thumbnails_give_the_picker_the_band() {
        let l = layout_1080(0.);

        assert_eq!(l.thumbnails.size.h, 0.);
        // 35 + 58 + 13, and 1045 − 112 − 21 − 58 − 13.
        assert_eq!(l.workspaces, rect(0., 106., 1920., 841.));
        // Entry, dash and results strip do not depend on the thumbnails.
        assert_eq!(l.search_entry, layout_1080(1.).search_entry);
        assert_eq!(l.dash, layout_1080(1.).dash);
        assert_eq!(l.search_results, layout_1080(1.).search_results);
    }

    /// `expandFraction` scales *both* the thumbnails height and the spacing
    /// below them, so the picker box moves continuously (this is what keeps
    /// the zoom from popping when a third workspace appears mid-overview).
    #[test]
    fn expand_fraction_interpolates_both_terms() {
        let l = layout_1080(0.5);

        assert_eq!(l.thumbnails.size.h, 27.);
        // 106 + 27 + 4
        assert_eq!(l.workspaces.loc.y, 137.);
        assert_eq!(l.workspaces.size.h, 810.);

        let (lo, hi) = (layout_1080(0.), layout_1080(1.));
        assert_eq!(
            l.workspaces.loc.y,
            (lo.workspaces.loc.y + hi.workspaces.loc.y) / 2.
        );
        assert_eq!(
            l.workspaces.size.h,
            (lo.workspaces.size.h + hi.workspaces.size.h) / 2.
        );
    }

    /// The dash cap is a fraction of the *work area*, and it bites before the
    /// dash would eat the picker on a short screen.
    #[test]
    fn dash_is_capped_at_a_fraction_of_the_work_area() {
        // round(565 × 0.16) = 90 < the 112 the dash would like.
        let l = layout(
            Size::from((1024., 600.)),
            35.,
            SEARCH_H,
            DASH_H,
            30.,
            1.,
            state::WINDOW_PICKER,
        );
        assert_eq!(l.dash.size.h, 90.);
        assert_eq!(l.dash.loc.y, 600. - 90.);

        // An oversized dash on a tall screen is capped too, not merely clamped
        // to its own preference.
        let l = layout(
            Size::from((1920., 1080.)),
            35.,
            SEARCH_H,
            400.,
            THUMBS_H,
            1.,
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
            SEARCH_H,
            DASH_H,
            72.,
            1.,
            state::WINDOW_PICKER,
        );

        // work area 1405 ⇒ spacing = round(28.1) = 28, top 17, bottom 11.
        assert_eq!(l.search_entry, rect(0., 35., 2560., 58.));
        assert_eq!(l.thumbnails, rect(0., 110., 2560., 72.));
        assert_eq!(l.dash, rect(0., 1440. - 112., 2560., 112.));
        // 110 + 72 + 11, and 1405 − 112 − 28 − 58 − 17 − 72 − 11.
        assert_eq!(l.workspaces, rect(0., 193., 2560., 1107.));
        assert_eq!(l.search_results, rect(0., 121., 2560., 1179.));
    }

    /// Without a panel strut everything shifts up by exactly the strut.
    #[test]
    fn no_strut_starts_at_the_view_top() {
        let l = layout(
            Size::from((1920., 1080.)),
            0.,
            SEARCH_H,
            DASH_H,
            54.,
            1.,
            state::WINDOW_PICKER,
        );

        assert_eq!(l.search_entry.loc.y, 0.);
        assert_eq!(l.dash.loc.y, 1080. - 112.);
        // spacing = round(1080 × 0.02) = 22, top 13, bottom 9.
        assert_eq!(l.workspaces, rect(0., 134., 1920., 812.));
    }

    /// In the app-grid state the picker shrinks to the small top strip
    /// (`round(1045 × 0.15) = 157` at y = 35 + 58 + 21) and the app grid fills the
    /// band below it, up to the dash.
    #[test]
    fn app_grid_state_shrinks_the_picker_and_fills_below() {
        let l = layout_1080_state(1., state::APP_GRID);

        assert_eq!(l.workspaces, rect(0., 114., 1920., 157.));
        // 35 + 58 + 21 + 157 + 21, and 1045 − 58 − 21 − 157 − 21 − 112 − 21.
        assert_eq!(l.app_display, rect(0., 292., 1920., 655.));
    }

    /// In the window-picker state the app grid is parked at the work-area bottom
    /// (off-screen below), keeping its app-grid height; the picker is unchanged.
    #[test]
    fn window_picker_state_parks_the_app_grid_below() {
        let l = layout_1080(1.);

        // Parked at box.y2 = start_y + work-area height = 35 + 1045.
        assert_eq!(l.app_display, rect(0., 1080., 1920., 655.));
        // The picker is exactly the pre-app-grid window-picker box.
        assert_eq!(l.workspaces, rect(0., 168., 1920., 779.));
    }

    /// A fractional state interpolates both the picker and the app grid between the
    /// two bracketing states — the smooth show-apps transition.
    #[test]
    fn state_interpolates_workspaces_and_app_grid() {
        let mid = layout_1080_state(1., 1.5);
        let picker = layout_1080_state(1., state::WINDOW_PICKER);
        let grid = layout_1080_state(1., state::APP_GRID);

        for (m, p, g) in [
            (mid.workspaces, picker.workspaces, grid.workspaces),
            (mid.app_display, picker.app_display, grid.app_display),
        ] {
            assert_eq!(m.loc.y, (p.loc.y + g.loc.y) / 2.);
            assert_eq!(m.size.h, (p.size.h + g.size.h) / 2.);
        }
    }
}
