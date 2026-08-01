//! `ThumbnailSwitcher` (`js/ui/altTab.js:910-999`) — the app switcher's window sub-list.
//!
//! A second `.switcher-list` that drops below the app row when the selected app has more than one
//! window: one 256px live preview per window, with its title underneath. It is what makes
//! Super-Tab able to reach a *specific* window rather than only the app's most recent one.
//!
//! Two things separate it from the row above it:
//! - **it is not square.** `super._init(false)` (`:913`) leaves the item container non-homogeneous,
//!   so each preview keeps its own width instead of every item taking the widest. The Tab popups
//!   both pass `true`.
//! - **it is not centered on the monitor.** `AppSwitcherPopup.vfunc_allocate` (`:79-115`) centers
//!   it under the *selected app's icon* and then slides it back inside the primary monitor, so it
//!   points at the app it belongs to.

use std::time::Duration;

use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::ui::switcher::{ITEM_PADDING, ITEM_SPACING, LIST_PADDING, POPUP_SPACING};

/// `THUMBNAIL_DEFAULT_SIZE` (`altTab.js:15`), which `.thumbnail`'s `width: 256px`
/// (`_switcher-popup.scss:53-56`) repeats — the SCSS comment says so.
pub const THUMBNAIL_SIZE: f64 = 256.;

/// `THUMBNAIL_POPUP_TIME` (`:16`) — how long the selection rests on a multi-window app before its
/// windows appear on their own (`_select`, `:349-356`).
///
/// The delay is the whole reason the sub-list is not in the way: tabbing *through* a multi-window
/// app never opens it, because the next Tab re-arms the timer from scratch.
pub const POPUP_TIME: Duration = Duration::from_millis(500);

/// `THUMBNAIL_FADE_TIME` (`:17`).
///
/// **Not implemented.** GNOME fades the sub-list in and out over this (`:359-408`); ours appears
/// and disappears at once. Recorded so the number does not have to be re-derived, and because the
/// fade is the only part of `_createThumbnails`/`_destroyThumbnails` we skip.
pub const FADE_TIME: Duration = Duration::from_millis(100);

/// `.thumbnail-box` `padding: 2px` (`_switcher-popup.scss:49-52`).
pub const BOX_PADDING: f64 = 2.;
/// `.thumbnail-box` `spacing: $base_padding` (`:51`) — between the preview and its title.
pub const BOX_SPACING: f64 = 6.;
/// `.thumbnail` `border-radius: $base_border_radius` (`:55`), which is 8 (`_common.scss:39`) —
/// half [`ITEM_RADIUS`](super::ITEM_RADIUS), which is the doubled `%tile` value.
pub const THUMB_RADIUS: f64 = 8.;

/// Where the sub-list and everything in it sits, in output-logical coordinates.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ThumbLayout {
    /// The `.switcher-list` box.
    pub panel: Rectangle<f64, Logical>,
    /// One `.item-box` per window, left to right.
    pub items: Vec<Rectangle<f64, Logical>>,
    /// The `.thumbnail` bin inside each item — where the live preview is fitted.
    pub thumbs: Vec<Rectangle<f64, Logical>>,
    /// The title band under each preview.
    pub labels: Vec<Rectangle<f64, Logical>>,
}

/// The height of one preview bin — `ThumbnailSwitcher.addClones` (`altTab.js:946-976`).
///
/// `available` is the room below the sub-list's top edge (`primary.y + primary.height -
/// bottomPadding - childBox.y1`, `:110`), with `.switcher-popup`'s bottom padding being 0
/// (`_switcher-popup.scss:9`).
///
/// The arithmetic is transcribed rather than tidied, including the part that reads oddly:
/// `totalPadding` sums the item's and the list's **horizontal and vertical** padding (`:949-950`)
/// and then the bin adds the vertical halves of both back (`:957`). On a screen with room to
/// spare all of it is moot — the result clamps to [`THUMBNAIL_SIZE`] — so it only bites on a
/// short monitor, which is exactly where guessing at a "cleaner" formula would diverge silently.
pub fn thumb_height(available: f64, label_h: f64) -> f64 {
    let total_padding = ITEM_PADDING * 4. + LIST_PADDING * 4.;
    let avail = (available - label_h - total_padding - BOX_SPACING).min(THUMBNAIL_SIZE);
    // GNOME has no floor here; a monitor too short for any preview would ask for a negative
    // height, which our geometry cannot express.
    (avail + ITEM_PADDING * 2. + LIST_PADDING * 2. - BOX_SPACING).clamp(0., THUMBNAIL_SIZE)
}

/// One item's outer `.item-box` size, for `n` previews of `thumb_h` with `label_h` titles.
fn item_size(thumb_h: f64, label_h: f64) -> Size<f64, Logical> {
    Size::from((
        THUMBNAIL_SIZE + BOX_PADDING * 2. + ITEM_PADDING * 2.,
        thumb_h + BOX_SPACING + label_h + BOX_PADDING * 2. + ITEM_PADDING * 2.,
    ))
}

/// Lay the sub-list out under the app row — `AppSwitcherPopup.vfunc_allocate` (`altTab.js:79-115`).
///
/// `anchor_x` is the centre of the selected app's icon and `top` is the app panel's bottom edge;
/// the sub-list is centred on the first and sits [`POPUP_SPACING`] below the second (`:106-107`).
/// `.switcher-popup`'s own padding is 0 on every side (`_switcher-popup.scss:9`), so the clamps
/// below are against the bare monitor.
///
/// DIVERGENCE (a hair): GNOME's overflow branch (`:99-102`) compares against `primary.width`
/// where every other line uses `primary.x + primary.width`, so on a monitor whose origin is not 0
/// it slides the list by the origin as well. We clamp to the monitor's real right edge.
pub fn layout(
    count: usize,
    thumb_h: f64,
    label_h: f64,
    anchor_x: f64,
    top: f64,
    monitor: Rectangle<f64, Logical>,
) -> ThumbLayout {
    if count == 0 {
        return ThumbLayout::default();
    }

    let n = count as f64;
    let item = item_size(thumb_h, label_h);
    let size = Size::<f64, Logical>::from((
        n * item.w + (n - 1.) * ITEM_SPACING + LIST_PADDING * 2.,
        item.h + LIST_PADDING * 2.,
    ));

    // Centred on the icon, then slid back inside the monitor — left edge wins if it cannot fit.
    let right = monitor.loc.x + monitor.size.w;
    let x = (anchor_x - size.w / 2.)
        .floor()
        .min(right - size.w)
        .max(monitor.loc.x);
    let loc = Point::from((x, top + POPUP_SPACING));

    let mut items = Vec::with_capacity(count);
    let mut thumbs = Vec::with_capacity(count);
    let mut labels = Vec::with_capacity(count);
    for i in 0..count {
        let item_loc = Point::from((
            loc.x + LIST_PADDING + i as f64 * (item.w + ITEM_SPACING),
            loc.y + LIST_PADDING,
        ));
        items.push(Rectangle::new(item_loc, item));

        // Inside the item box: `.thumbnail-box`'s padding, then the bin, then the title.
        let inner = Point::from((
            item_loc.x + ITEM_PADDING + BOX_PADDING,
            item_loc.y + ITEM_PADDING + BOX_PADDING,
        ));
        thumbs.push(Rectangle::new(inner, Size::from((THUMBNAIL_SIZE, thumb_h))));
        labels.push(Rectangle::new(
            Point::from((inner.x, inner.y + thumb_h + BOX_SPACING)),
            Size::from((THUMBNAIL_SIZE, label_h)),
        ));
    }

    ThumbLayout {
        panel: Rectangle::new(loc, size),
        items,
        thumbs,
        labels,
    }
}

/// `_nextWindow` (`altTab.js:139-146`) — and the comment there is the whole subtlety: with no
/// window picked yet the "current" one is treated as 0, so the first step lands on the **second**
/// window, for the same reason the app row opens on item 1.
pub fn next_window(current: Option<usize>, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    (current.unwrap_or(0) + 1) % len
}

/// `_previousWindow` (`:148-155`), which assumes the second window in the unset case, so stepping
/// back from nothing lands on the first.
pub fn previous_window(current: Option<usize>, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    (current.unwrap_or(1) + len - 1) % len
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor() -> Rectangle<f64, Logical> {
        Rectangle::new(Point::from((0., 0.)), Size::from((1920., 1080.)))
    }

    /// A tall-enough screen gives full-size previews; a short one shrinks them.
    #[test]
    fn the_preview_size_clamps_to_the_default_and_shrinks_on_a_short_screen() {
        assert_eq!(thumb_height(1000., 18.), THUMBNAIL_SIZE);
        let short = thumb_height(300., 18.);
        assert!(
            short < THUMBNAIL_SIZE && short > 0.,
            "a short screen shrinks the preview: {short}"
        );
        assert_eq!(thumb_height(0., 18.), 0., "and never goes negative");
    }

    /// The sub-list points at the app it belongs to, and stays on screen.
    #[test]
    fn the_sublist_centers_on_the_icon_and_clamps_to_the_monitor() {
        let (thumb, label) = (THUMBNAIL_SIZE, 18.);

        let mid = layout(2, thumb, label, 960., 500., monitor());
        assert_eq!(
            mid.panel.loc.x + mid.panel.size.w / 2.,
            960.,
            "centred on the icon"
        );
        assert_eq!(
            mid.panel.loc.y, 524.,
            "one `.switcher-popup` spacing below the row"
        );

        // An app at the right edge slides the list back in rather than letting it overhang.
        let edge = layout(2, thumb, label, 1900., 500., monitor());
        assert_eq!(
            edge.panel.loc.x + edge.panel.size.w,
            1920.,
            "flush with the right edge: {:?}",
            edge.panel
        );

        // ...and one wider than the monitor starts at its left edge instead of off-screen.
        let huge = layout(20, thumb, label, 100., 500., monitor());
        assert_eq!(huge.panel.loc.x, 0.);
    }

    /// Every preview is the same 256px-wide bin, with its title directly under it.
    #[test]
    fn each_item_is_a_fixed_width_preview_over_its_title() {
        let l = layout(3, 200., 18., 960., 500., monitor());

        for i in 0..3 {
            assert_eq!(l.thumbs[i].size, Size::from((THUMBNAIL_SIZE, 200.)));
            assert_eq!(l.labels[i].loc.y, l.thumbs[i].loc.y + 200. + BOX_SPACING);
            assert_eq!(l.labels[i].loc.x, l.thumbs[i].loc.x);

            // The bin sits inside both paddings, and the item ends clear of the title.
            assert_eq!(
                l.thumbs[i].loc.x,
                l.items[i].loc.x + ITEM_PADDING + BOX_PADDING
            );
            assert!(
                l.labels[i].loc.y + l.labels[i].size.h
                    <= l.items[i].loc.y + l.items[i].size.h - ITEM_PADDING - BOX_PADDING + 0.001
            );
        }

        // Non-homogeneous or not, the items tile the row without drift.
        assert_eq!(
            l.items[2].loc.x + l.items[2].size.w,
            l.panel.loc.x + l.panel.size.w - LIST_PADDING
        );
    }

    /// The unset-selection cases: forward lands on the second window, back on the first.
    #[test]
    fn stepping_from_no_window_lands_on_the_second_going_forward() {
        assert_eq!(next_window(None, 3), 1);
        assert_eq!(previous_window(None, 3), 0);

        assert_eq!(next_window(Some(2), 3), 0, "wraps");
        assert_eq!(previous_window(Some(0), 3), 2, "wraps");
    }
}
