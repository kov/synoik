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
//!   view. At 1920×1080 that is 54px where gnome-shell has 52.
//! - The app grid's `ControlsState::AppGrid` boxes are not computed yet; this only produces the
//!   window-picker state. `SMALL_WORKSPACE_RATIO` lands with the app grid.

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
    /// The window picker — gnome-shell's `ControlsState.WINDOW_PICKER`
    /// workspaces box (`overviewControls.js:91-107`).
    pub workspaces: Rectangle<f64, Logical>,
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

    let picker_y = start_y + search_h + spacing_top + thumbs_h + spacing_bottom;
    let picker_h = height - dash_h - spacing - search_h - spacing_top - thumbs_h - spacing_bottom;
    let workspaces = rect(0., picker_y, width, picker_h.max(0.));

    // Note: the thumbnails height is deliberately *not* subtracted here.
    let results_h = height - search_h - spacing - dash_h - spacing;
    let search_results = rect(0., start_y + search_h + spacing, width, results_h.max(0.));

    ControlsLayout {
        search_entry,
        thumbnails,
        dash,
        search_results,
        workspaces,
    }
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> Rectangle<f64, Logical> {
    Rectangle::new((x, y).into(), (w, h).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The heights the fork's own widgets publish, so the expectations below
    /// are hand-derived rather than observed.
    const SEARCH_H: f64 = 58.; // margin-top 12 + entry 40 + margin-bottom 6
    const DASH_H: f64 = 112.; // pill 100 + edge offset 12
    const THUMBS_H: f64 = 54.; // 1080 × MAX_THUMBNAIL_SCALE

    fn layout_1080(expand: f64) -> ControlsLayout {
        layout(
            Size::from((1920., 1080.)),
            35.,
            SEARCH_H,
            DASH_H,
            THUMBS_H,
            expand,
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
        let l = layout(Size::from((1024., 600.)), 35., SEARCH_H, DASH_H, 30., 1.);
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
        );
        assert_eq!(l.dash.size.h, 167.);
    }

    /// Every height derives from the work-area height, never the view height:
    /// at a second resolution the spacing follows the strut, not the panel.
    #[test]
    fn spacing_follows_the_work_area_not_the_view() {
        let l = layout(Size::from((2560., 1440.)), 35., SEARCH_H, DASH_H, 72., 1.);

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
        let l = layout(Size::from((1920., 1080.)), 0., SEARCH_H, DASH_H, 54., 1.);

        assert_eq!(l.search_entry.loc.y, 0.);
        assert_eq!(l.dash.loc.y, 1080. - 112.);
        // spacing = round(1080 × 0.02) = 22, top 13, bottom 9.
        assert_eq!(l.workspaces, rect(0., 134., 1920., 812.));
    }
}
