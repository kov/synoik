//! The overview workspace row — gnome-shell's `ThumbnailsBox`
//! (js/ui/workspaceThumbnail.js) fused with its app-grid workspaces box, as pure geometry.
//!
//! The row shows every workspace as a miniature across the full width of the band
//! [`crate::ui::overview_layout`] allocates it, and scrolls to follow the active one once
//! there are more workspaces than the band holds.
//!
//! **Divergence (approved 2026-08-03).** gnome-shell has two rows — a thumbnail strip in
//! the window-picker state and the shrunken picker in the app-grid state — that look and
//! behave differently. This is the one row both states draw, so the show-apps transition
//! never moves it and every affordance it carries (reorder, close-an-empty-desktop) is
//! there in both. See `docs/fork/dynamic-workspaces-divergence.md`.

use smithay::utils::{Logical, Point, Rectangle, Size};

use super::monitor::scroll_to_follow;

/// The active-workspace indicator's border width
/// (`.workspace-thumbnail-indicator` in the shell theme).
pub const INDICATOR_WIDTH: f64 = 3.;

/// How far a gap's drop zone cuts into the neighboring thumbnails
/// (`WORKSPACE_CUT_SIZE`).
pub const WORKSPACE_CUT_SIZE: f64 = 10.;

/// The width of the new-workspace drop placeholder (the theme's
/// `.placeholder`).
pub const PLACEHOLDER_WIDTH: f64 = 18.;

/// The close button's box on a thumbnail, logical px at ramp 1.
///
/// **Divergence (approved 2026-08-03).** gnome-shell has no such button — it reaps empty
/// workspaces instead (`docs/fork/dynamic-workspaces-divergence.md`). Smaller than the
/// window preview's `window_preview::CLOSE_SIZE` (32) because a thumbnail is a miniature,
/// and **inset** rather than half-overhanging the corner the way a preview's is: the row
/// clips everything it draws to its band, and an overhanging button would be sliced in half
/// along the band's top edge.
pub const CLOSE_SIZE: f64 = 24.;

/// How far the button is inset from the thumbnail's top-right corner, logical px at ramp 1.
pub const CLOSE_INSET: f64 = 6.;

/// The close button's box for the thumbnail *as drawn* at `thumb` (its slot after the
/// inactive-workspace shrink), ramped with the rest of the overview chrome.
pub fn close_rect(thumb: Rectangle<f64, Logical>, ramp: f64) -> Rectangle<f64, Logical> {
    let size = (CLOSE_SIZE * ramp).round();
    let inset = (CLOSE_INSET * ramp).round();
    Rectangle::new(
        Point::from((
            thumb.loc.x + thumb.size.w - inset - size,
            thumb.loc.y + inset,
        )),
        Size::from((size, size)),
    )
}

/// The laid-out strip.
#[derive(Debug)]
pub struct Strip {
    /// Scale from workspace to thumbnail coordinates.
    pub scale: f64,
    /// Per-workspace thumbnail rects, in view coordinates, workspace order. A
    /// scrolled row puts some of these partly or wholly outside [`Self::band`];
    /// they are clipped to it when drawn and are not hit-testable beyond it.
    pub thumbs: Vec<Rectangle<f64, Logical>>,
    /// The new-workspace drop placeholder, when a drag hovers a gap.
    pub placeholder: Option<Rectangle<f64, Logical>>,
    /// The band the strip was allocated: the row's viewport, and the clip.
    pub band: Rectangle<f64, Logical>,
}

/// Lays the row out inside its allocated `band`: `n` thumbnails one band tall, `gap` apart,
/// with the same `gap` before the first and after the last so the run never touches the
/// band's edges (`_getFirstFitAllWorkspaceBox`, `workspacesView.js:127-169`). A run that
/// fits is centered; one that overflows scrolls to follow `focus`, the fractional active
/// workspace index, so a workspace past the edge is reachable at all.
///
/// **Divergence (approved 2026-07-29).** gnome-shell narrows every box to
/// `availableWidth / n` once the width binds, so the whole row always fits — which past a
/// dozen workspaces is a run of specks. Ours stay aspect-locked whatever
/// the count, and the row scrolls instead.
///
/// A `placeholder` index makes room for the new-workspace drop placeholder before that
/// thumbnail (gnome-shell's drop placeholder), lengthening the run like an extra slot.
pub fn strip_geometry(
    view_size: Size<f64, Logical>,
    band: Rectangle<f64, Logical>,
    thumb_w: f64,
    gap: f64,
    n: usize,
    placeholder: Option<usize>,
    focus: f64,
) -> Strip {
    // A thumbnail is the band's full height: the band is allocated exactly one workspace
    // tall. Its width is the caller's, since that is what the row's zoom decides.
    let thumb = Size::from((thumb_w.round(), band.size.h.round()));
    let gap = gap.round();
    let y = band.loc.y.round();

    // Laid out from the row's own origin first, so the scroll can be computed from
    // where the focused thumbnail actually landed — the placeholder displaces it.
    let mut x = 0.;
    let mut placeholder_rect = None;
    let mut place = |i: usize, x: &mut f64| {
        if placeholder == Some(i) {
            placeholder_rect = Some(Rectangle::new(
                Point::from((*x, y)),
                Size::from((PLACEHOLDER_WIDTH, thumb.h)),
            ));
            *x += PLACEHOLDER_WIDTH + gap;
        }
    };
    let mut thumbs = Vec::with_capacity(n);
    for i in 0..n {
        place(i, &mut x);
        thumbs.push(Rectangle::new(Point::from((x, y)), thumb));
        x += thumb.w + gap;
    }
    place(n, &mut x);
    // `x` has one trailing gap on it, which is the space *after* the last thumbnail.
    let run = (x - gap).max(0.);

    // Where the active workspace sits along the row, interpolated between its neighbours
    // so the row tracks a workspace switch as it animates rather than jumping at the
    // halfway point.
    let idx = focus.clamp(0., (n - 1) as f64);
    let (lo, hi) = (idx.floor() as usize, idx.ceil() as usize);
    let t = idx.fract();
    let focus_x = thumbs[lo].loc.x + (thumbs[hi].loc.x - thumbs[lo].loc.x) * t + thumb.w / 2.;

    let x0 = (band.loc.x + gap + scroll_to_follow(band.size.w - gap * 2., run, focus_x)).round();
    for rect in &mut thumbs {
        rect.loc.x += x0;
    }
    if let Some(rect) = &mut placeholder_rect {
        rect.loc.x += x0;
    }

    Strip {
        // The exact scale the rounded thumbnail size implies, so contents
        // fill it precisely.
        scale: thumb.h / view_size.h,
        thumbs,
        placeholder: placeholder_rect,
        band,
    }
}

impl Strip {
    /// The strip's overall bounding rect.
    pub fn bounds(&self) -> Rectangle<f64, Logical> {
        let first = self.thumbs[0];
        let last = self.thumbs[self.thumbs.len() - 1];
        let mut x0 = first.loc.x;
        let mut x1 = last.loc.x + last.size.w;
        if let Some(rect) = self.placeholder {
            x0 = x0.min(rect.loc.x);
            x1 = x1.max(rect.loc.x + rect.size.w);
        }
        Rectangle::new(
            Point::from((x0, first.loc.y)),
            Size::from((x1 - x0, first.size.h)),
        )
    }

    /// The workspace whose thumbnail contains the position. A scrolled row is clipped
    /// to the band, so only what is drawn can be hit — the part of a thumbnail past the
    /// edge is not there to aim at, and the space beyond the band belongs to the
    /// floating search entry.
    pub fn thumb_under(&self, pos: Point<f64, Logical>) -> Option<usize> {
        if !self.band.contains(pos) {
            return None;
        }
        self.thumbs.iter().position(|rect| rect.contains(pos))
    }

    /// The drop target at the position: a workspace, or a gap insertion
    /// index. Gaps cut [`WORKSPACE_CUT_SIZE`] into the neighboring
    /// thumbnails (gnome-shell's `_getPlaceholderTarget`); only interior
    /// gaps insert.
    pub fn drop_target(&self, pos: Point<f64, Logical>) -> Option<DropTarget> {
        let bounds = self.bounds();
        if !bounds.contains(pos) || !self.band.contains(pos) {
            return None;
        }

        for (i, rect) in self.thumbs.iter().enumerate() {
            let cut_left = if i == 0 { 0. } else { WORKSPACE_CUT_SIZE };
            let cut_right = if i == self.thumbs.len() - 1 {
                0.
            } else {
                WORKSPACE_CUT_SIZE
            };
            if pos.x < rect.loc.x + cut_left {
                // In the gap (or cut) before this thumbnail.
                return Some(DropTarget::NewAt(i));
            }
            if pos.x < rect.loc.x + rect.size.w - cut_right {
                return Some(DropTarget::Workspace(i));
            }
        }

        // Within the cut after the last thumbnail's body.
        Some(DropTarget::NewAt(self.thumbs.len()))
    }
}

/// Where a drop on the strip lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropTarget {
    /// Onto the workspace at this index.
    Workspace(usize),
    /// Into the gap before the workspace at this index (a new workspace).
    NewAt(usize),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> Size<f64, Logical> {
        Size::from((1920., 1080.))
    }

    /// The band [`crate::ui::overview_layout`] allocates the row at the 1920x1080 /
    /// 35px-strut reference: full width, top on the search puck's midline (35 + 40), one
    /// small workspace tall (`round(1045 * SMALL_WORKSPACE_RATIO)`).
    const BAND_Y: f64 = 75.;
    const THUMB_H: f64 = 157.;
    /// The output's aspect at that height.
    const THUMB_W: f64 = 279.;
    /// The row's fit-all gap: `WORKSPACE_MIN_SPACING` at ramp 1.
    const GAP: f64 = 24.;

    fn band() -> Rectangle<f64, Logical> {
        Rectangle::new(Point::from((0., BAND_Y)), Size::from((1920., THUMB_H)))
    }

    /// The row with the first workspace active, which is where every test that is
    /// not about scrolling wants to be.
    fn strip(n: usize, placeholder: Option<usize>) -> Strip {
        strip_geometry(view(), band(), THUMB_W, GAP, n, placeholder, 0.)
    }

    /// Positions of a row of `n` at an arbitrary thumbnail width and gap — the sweep the
    /// row rule's own tests want, independent of the reference canvas.
    fn positions_at(thumb_w: f64, gap: f64, n: usize, focus: f64) -> Vec<f64> {
        strip_geometry(view(), band(), thumb_w, gap, n, None, focus)
            .thumbs
            .iter()
            .map(|r| r.loc.x)
            .collect()
    }

    fn positions(thumb_w: f64, gap: f64, n: usize) -> Vec<f64> {
        positions_at(thumb_w, gap, n, 0.)
    }

    /// The row is the app-grid row: workspaces at the small size, one gap apart, run
    /// centered in the full-width band.
    #[test]
    fn three_thumbnails_across_the_band() {
        let strip = strip(3, None);
        assert_eq!(strip.scale, THUMB_H / 1080.);

        let run = THUMB_W * 3. + GAP * 2.;
        let expected_x0 = ((1920. - run) / 2.).round();
        assert_eq!(
            strip.thumbs[0],
            Rectangle::new(
                Point::from((expected_x0, BAND_Y)),
                Size::from((THUMB_W, THUMB_H))
            )
        );
        assert_eq!(strip.thumbs[1].loc.x, expected_x0 + THUMB_W + GAP);
        assert_eq!(strip.thumbs[2].loc.x, expected_x0 + (THUMB_W + GAP) * 2.);
    }

    /// The run is centered in the view as a whole, independent of which workspace is
    /// active (`_getFirstFitAllWorkspaceBox`, `workspacesView.js:127-169`).
    #[test]
    fn the_run_is_centered() {
        for gap in [0., 32., 100.] {
            for n in 1..7usize {
                // A width small enough that n fit — the height-binds case, which is the
                // one the row is in for realistic counts.
                let thumb_w = ((1920. - gap * (n as f64 + 1.)) / n as f64 * 0.8).round();
                let xs = positions(thumb_w, gap, n);
                let first = xs[0];
                let last = xs[n - 1] + thumb_w;
                assert!(
                    ((first + last) / 2. - 1920. / 2.).abs() <= 1.,
                    "run of {n} not centered (gap={gap})"
                );
                assert!(first >= gap - 1., "run of {n} touches the left edge");
                assert!(
                    last <= 1920. - gap + 1.,
                    "run of {n} touches the right edge"
                );
            }
        }
    }

    /// The gap is also the margin before the first and after the last workspace
    /// (`workspacesView.js:135-137`), so a row that exactly fills the band still leaves
    /// one gap on each side.
    #[test]
    fn a_gap_is_kept_at_both_ends() {
        let (gap, n) = (32., 4usize);
        let thumb_w = ((1920. - gap * (n as f64 + 1.)) / n as f64).round();
        let xs = positions(thumb_w, gap, n);
        assert!((xs[0] - gap).abs() <= 1.);
        assert!((xs[n - 1] + thumb_w - (1920. - gap)).abs() <= 1.);
    }

    /// A run that *fits* ignores which workspace is active, exactly as gnome-shell's
    /// centering does — the scroll only engages on overflow.
    #[test]
    fn the_selection_does_not_move_a_row_that_fits() {
        let (gap, n) = (32., 5usize);
        let thumb_w = ((1920. - gap * (n as f64 + 1.)) / n as f64 * 0.8).round();
        let base = positions_at(thumb_w, gap, n, 0.);
        for active in 1..n {
            assert_eq!(base, positions_at(thumb_w, gap, n, active as f64));
        }
    }

    /// When the width binds instead of the height (many workspaces), we keep the
    /// aspect-locked width and let the row overflow rather than squashing the boxes — the
    /// recorded divergence. The overflowing row then scrolls to follow the active
    /// workspace, or the tail would be unreachable.
    #[test]
    fn an_overflowing_run_scrolls_to_the_active_workspace() {
        let (thumb_w, gap, n) = (THUMB_W, GAP, 10usize);
        let run = thumb_w * n as f64 + gap * (n - 1) as f64;
        assert!(run > 1920., "this case must actually overflow");

        // Every workspace, selected in turn, is fully on screen — and the size never
        // gives way to the count.
        for active in 0..n {
            let strip = strip_geometry(view(), band(), thumb_w, gap, n, None, active as f64);
            assert_eq!(strip.scale, THUMB_H / 1080.);
            let rect = strip.thumbs[active];
            assert!(
                rect.loc.x >= -1. && rect.loc.x + rect.size.w <= 1920. + 1.,
                "workspace {active} is off screen at {rect:?}"
            );
            // Rigid: the scroll moves the row, it never re-spaces it.
            for pair in strip.thumbs.windows(2) {
                assert_eq!(pair[1].loc.x - pair[0].loc.x, thumb_w + gap);
            }
        }

        // The ends stay flush: at the extremes the row has scrolled as far as it can and
        // no further, so no dead space opens past the first or last workspace.
        let first = positions_at(thumb_w, gap, n, 0.);
        assert!((first[0] - gap).abs() <= 1.);
        let last = positions_at(thumb_w, gap, n, (n - 1) as f64);
        assert!((last[n - 1] + thumb_w - (1920. - gap)).abs() <= 1.);

        // A fractional focus scrolls smoothly, so the row tracks a workspace switch as it
        // animates rather than jumping at the halfway point.
        let mid = positions_at(thumb_w, gap, n, 4.5);
        let (a, b) = (
            positions_at(thumb_w, gap, n, 4.)[0],
            positions_at(thumb_w, gap, n, 5.)[0],
        );
        assert!((mid[0] - (a + b) / 2.).abs() <= 1.);
    }

    /// Only what is drawn can be hit: a scrolled row is clipped to the band.
    #[test]
    fn a_scrolled_thumbnail_is_not_hit_outside_the_band() {
        let n = 10;
        let strip = strip_geometry(view(), band(), THUMB_W, GAP, n, None, 0.);
        let y = BAND_Y + 40.;

        let outside = strip
            .thumbs
            .iter()
            .position(|r| r.loc.x >= 1920.)
            .expect("the row must overflow the band");
        let rect = strip.thumbs[outside];
        let pos = Point::from((rect.loc.x + rect.size.w / 2., y));
        assert!(rect.contains(pos), "the probe must be inside the rect");
        assert_eq!(strip.thumb_under(pos), None);
        assert_eq!(strip.drop_target(pos), None);

        // ...while a thumbnail that straddles the band edge still is, on its visible part.
        let straddling = strip
            .thumbs
            .iter()
            .position(|t| t.loc.x < 1920. && t.loc.x + t.size.w > 1920.)
            .expect("the overflowing edge must end on a partial thumbnail");
        assert_eq!(
            strip.thumb_under(Point::from((1920. - 4., y))),
            Some(straddling)
        );
    }

    /// The row fills the band it was allocated, so the allocator is the only thing that
    /// decides whether it clears the top panel.
    #[test]
    fn the_row_fills_its_allocated_band() {
        let strip = strip(3, None);
        assert_eq!(strip.thumbs[0].size.h, THUMB_H);
        assert_eq!(strip.thumbs[0].loc.y, BAND_Y);
        assert_eq!(strip.thumbs[0].size.h, band().size.h);

        // Move the band and the whole row follows.
        let moved = Rectangle::new(Point::from((40., 300.)), Size::from((800., THUMB_H)));
        let strip = strip_geometry(view(), moved, THUMB_W, GAP, 3, None, 0.);
        assert_eq!(strip.thumbs[0].loc.y, 300.);
        // 800 is not wide enough for three, so the row scrolls to the active workspace
        // inside the moved band rather than fitting into it.
        assert_eq!(strip.thumbs[0].loc.x, 40. + GAP);
    }

    /// The close button is **inset**, not corner-centred like the window preview's: the
    /// row clips everything it draws to its band, and the band is exactly one thumbnail
    /// tall, so a half-overhanging button would be sliced along its top edge.
    #[test]
    fn the_close_button_sits_inside_its_thumbnail() {
        let strip = strip(3, None);
        let thumb = strip.thumbs[1];
        let rect = close_rect(thumb, 1.);

        assert_eq!(rect.size, Size::from((CLOSE_SIZE, CLOSE_SIZE)));
        // Top-right corner, inset on both edges it touches.
        assert_eq!(rect.loc.y, thumb.loc.y + CLOSE_INSET);
        assert_eq!(
            rect.loc.x + rect.size.w,
            thumb.loc.x + thumb.size.w - CLOSE_INSET
        );
        // Wholly inside, so both the clip and the hit test see all of it.
        assert!(
            rect.loc.x >= thumb.loc.x && rect.loc.y + rect.size.h <= thumb.loc.y + thumb.size.h
        );

        // It ramps with the rest of the overview chrome, so it stays proportionate on a
        // canvas whose thumbnails have shrunk.
        let small = close_rect(thumb, 0.5);
        assert_eq!(small.size, Size::from((12., 12.)));
        assert_eq!(small.loc.y, thumb.loc.y + 3.);
    }

    #[test]
    fn placeholder_spreads_the_thumbnails_apart() {
        let at_rest = strip(3, None);
        let strip = strip(3, Some(1));

        let rect = strip
            .placeholder
            .expect("placeholder rect must be laid out");
        assert_eq!(rect.size, Size::from((PLACEHOLDER_WIDTH, THUMB_H)));
        // It sits between the first two thumbnails, with normal spacing.
        assert_eq!(rect.loc.x, strip.thumbs[0].loc.x + THUMB_W + GAP);
        assert_eq!(strip.thumbs[1].loc.x, rect.loc.x + PLACEHOLDER_WIDTH + GAP);

        // The run is longer by exactly the placeholder, and still centered — so it opens
        // symmetrically rather than pushing the row off one edge.
        let shift = at_rest.thumbs[0].loc.x - strip.thumbs[0].loc.x;
        assert!((shift - (PLACEHOLDER_WIDTH + GAP) / 2.).abs() <= 1.);

        // A pointer over the placeholder still maps to the same insertion point, so the
        // hover is stable.
        let center = Point::from((rect.loc.x + rect.size.w / 2., BAND_Y + 40.));
        assert_eq!(strip.drop_target(center), Some(DropTarget::NewAt(1)));
    }

    #[test]
    fn drop_targets_split_thumbs_and_gaps() {
        let strip = strip(3, None);
        let y = BAND_Y + 40.;
        let first = strip.thumbs[0];
        let second = strip.thumbs[1];

        // Center of a thumbnail: the workspace itself.
        assert_eq!(
            strip.drop_target(Point::from((first.loc.x + 48., y))),
            Some(DropTarget::Workspace(0))
        );
        // The gap between the first two (plus the 10px cuts): insert at 1.
        assert_eq!(
            strip.drop_target(Point::from((second.loc.x - 4., y))),
            Some(DropTarget::NewAt(1))
        );
        assert_eq!(
            strip.drop_target(Point::from((second.loc.x + 5., y))),
            Some(DropTarget::NewAt(1))
        );
        // Outside the row: no target.
        assert_eq!(strip.drop_target(Point::from((10., y))), None);
        assert_eq!(
            strip.drop_target(Point::from((first.loc.x + 48., 500.))),
            None
        );
    }
}
