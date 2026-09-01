// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

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

/// The width of the hairline that marks a gap before the drag has lingered in it long
/// enough to open the [`PLACEHOLDER_WIDTH`] pill — see [`Insert::Hairline`].
pub const HAIRLINE_WIDTH: f64 = 2.;

/// How much of a thumbnail's height the hairline spans, centred on it. Shorter than the
/// pill, which is full height: the hairline is a hint that a gap is *aimed at*, and reading
/// as a lesser mark than the thing it turns into is the point.
const HAIRLINE_HEIGHT_FRAC: f64 = 0.5;

/// A workspace that is not in the model yet, holding open the slot it would take.
///
/// gnome-shell's `collapse_fraction`, inverted so 1 reads as "there": a fully collapsed
/// slot is not there at all and a fully expanded one is a whole workspace wide
/// (`workspaceThumbnail.js:1360,1428`).
///
/// **It grows off the right end of the row, and moves nothing.** The run's centering is
/// computed as if the phantom were not there, so every real thumbnail stays exactly where
/// it was when the drag began; only the slot's own width changes. The row catches up in
/// one ease *after* the drop — a row that recentred continuously would be a moving target
/// to aim at, and it fed back into the drop decision at the last gap.
///
/// **Divergence (approved 2026-08-11).** gnome-shell only ever runs this *after* a drop —
/// during the drag it shows a fixed-width `.placeholder` pill and nothing moves
/// (`_dropPlaceholderPos`, `workspaceThumbnail.js:1352-1390`). Ours is driven by the drag
/// itself, so the row opens as you approach and the drop lands on a row already in its
/// final shape. See `docs/fork/dynamic-workspaces-divergence.md`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Phantom {
    /// The workspace index it would be inserted at. Always the append index — the slot
    /// only ever opens past the end of the row (an interior insert gets the pill).
    pub idx: usize,
    /// How much row it takes: 0 = not there at all, 1 = a full workspace's worth.
    /// gnome-shell's `collapse_fraction`, inverted.
    pub reveal: f64,
    /// How far the workspace itself has materialized in the slot the `reveal` opened:
    /// gnome-shell's `slide_position`, inverted — scale `lerp(0.75, 1, emerge)` and
    /// opacity `emerge` (`workspaceThumbnail.js:319-333`). Geometry ignores it; only the
    /// drawing does, which is why the two are separate.
    pub emerge: f64,
}

/// What the row is making room for while a drag is in flight. Alternatives, not partners:
/// a gap you have to aim into gets the pill, and the workspace a drop would *append* gets
/// the phantom.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Insert {
    /// A hairline in the gap before the thumbnail at this index, marking it as aimed at.
    /// **Lays nothing out**: it is drawn inside the gap that is already there, so the row
    /// does not move while the pointer is only passing through. The linger that promotes it
    /// to [`Insert::Placeholder`] lives on the monitor
    /// (`Monitor::thumb_placeholder_linger`).
    Hairline(usize),
    /// The fixed-width drop pill, before the thumbnail at this index (gnome-shell's
    /// `_dropPlaceholderPos`, `workspaceThumbnail.js:1352-1390`). Parts the row.
    Placeholder(usize),
    /// A whole workspace's slot, opening.
    Phantom(Phantom),
}

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

/// How far a name label is inset from the thumbnail's bottom edge, logical px.
pub const NAME_INSET: f64 = 6.;

/// A name label's box on the thumbnail drawn at `thumb`: centred across it, sitting on its
/// bottom edge. Unlike the close button the pill is *not* ramped — it holds shaped text, and
/// re-shaping it every frame of the overview's open animation would cost a bake a frame.
pub fn name_rect(
    thumb: Rectangle<f64, Logical>,
    size: Size<f64, Logical>,
) -> Rectangle<f64, Logical> {
    Rectangle::new(
        Point::from((
            (thumb.loc.x + (thumb.size.w - size.w) / 2.).round(),
            (thumb.loc.y + thumb.size.h - NAME_INSET - size.h).round(),
        )),
        size,
    )
}

/// The hairline marking the gap between two thumbnails, centred in the space that is
/// actually visible — hence the rects **as drawn** rather than their slots: an inactive
/// thumbnail is drawn inset inside its slot, and measuring from the slots leans the mark
/// toward whichever neighbour is drawn at full size.
///
/// The outermost insertion points have a neighbour on one side only, and measure the gap off
/// the one they have.
pub fn hairline_rect(
    left: Option<Rectangle<f64, Logical>>,
    right: Option<Rectangle<f64, Logical>>,
    gap: f64,
) -> Option<Rectangle<f64, Logical>> {
    let reference = right.or(left)?;
    let start = left.map_or(reference.loc.x - gap, |r| r.loc.x + r.size.w);
    let end = right.map_or(reference.loc.x + reference.size.w + gap, |r| r.loc.x);
    let h = (reference.size.h * HAIRLINE_HEIGHT_FRAC).round();
    Some(Rectangle::new(
        Point::from((
            ((start + end - HAIRLINE_WIDTH) / 2.).round(),
            (reference.loc.y + (reference.size.h - h) / 2.).round(),
        )),
        Size::from((HAIRLINE_WIDTH, h)),
    ))
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
    /// The new-workspace drop placeholder pill, when a drag has lingered in an interior gap.
    pub placeholder: Option<Rectangle<f64, Logical>>,
    /// The gap a drag is aiming at, before the linger promotes it to [`Self::placeholder`].
    /// An index, not a rect: the mark is centred between the thumbnails **as drawn**, and
    /// the shrink that insets a thumbnail inside its slot belongs to the monitor
    /// (`Monitor::strip_hairline_rect`). It sits inside a gap that is already there, so it
    /// never widens [`Self::bounds`].
    pub hairline: Option<usize>,
    /// The slot held open for a workspace that does not exist yet, and how far open it is.
    pub phantom: Option<(Rectangle<f64, Logical>, Phantom)>,
    /// The band the strip was allocated: the row's viewport, and the clip.
    pub band: Rectangle<f64, Logical>,
    /// The gap the row was laid with — what the space between two slots measures.
    pub gap: f64,
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
/// An [`Insert`] lengthens the run like an extra slot, marking where a drop would put a
/// new workspace.
pub fn strip_geometry(
    view_size: Size<f64, Logical>,
    band: Rectangle<f64, Logical>,
    thumb_w: f64,
    gap: f64,
    n: usize,
    insert: Option<Insert>,
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
        if let Some(Insert::Placeholder(idx)) = insert {
            if idx == i {
                placeholder_rect = Some(Rectangle::new(
                    Point::from((*x, y)),
                    Size::from((PLACEHOLDER_WIDTH, thumb.h)),
                ));
                *x += PLACEHOLDER_WIDTH + gap;
            }
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

    // The phantom is placed *after* the run is positioned, and takes no part in either the
    // centering or the scroll: one gap past the last thumbnail, growing rightwards from
    // nothing to a whole workspace. That is what keeps the row still while the drag is in
    // flight. gnome-shell rounds only the collapsing portion, so a slot mid-animation
    // never jitters by a pixel while a settled one is exactly a thumbnail wide
    // (`workspaceThumbnail.js:1424-1428`).
    let phantom_rect = match insert {
        Some(Insert::Phantom(ph)) => {
            let last = thumbs[n - 1];
            let collapse = 1. - ph.reveal.clamp(0., 1.);
            let w = thumb.w - (thumb.w * collapse).round();
            Some((
                Rectangle::new(
                    Point::from((last.loc.x + last.size.w + gap, y)),
                    Size::from((w, thumb.h)),
                ),
                ph,
            ))
        }
        _ => None,
    };

    Strip {
        // The exact scale the rounded thumbnail size implies, so contents
        // fill it precisely.
        scale: thumb.h / view_size.h,
        thumbs,
        placeholder: placeholder_rect,
        hairline: match insert {
            Some(Insert::Hairline(idx)) => Some(idx),
            _ => None,
        },
        phantom: phantom_rect,
        band,
        gap,
    }
}

impl Strip {
    /// The strip's overall bounding rect.
    pub fn bounds(&self) -> Rectangle<f64, Logical> {
        let first = self.thumbs[0];
        let last = self.thumbs[self.thumbs.len() - 1];
        let mut x0 = first.loc.x;
        let mut x1 = last.loc.x + last.size.w;
        // A slot with no width in it is not part of the row — it sits one gap past the
        // last thumbnail, and counting it would stretch the drop-target region there
        // before the phantom had opened at all.
        let phantom = self
            .phantom
            .iter()
            .map(|(r, _)| r)
            .filter(|r| r.size.w > 0.);
        for rect in self.placeholder.iter().chain(phantom) {
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
        let insert = placeholder.map(Insert::Placeholder);
        strip_geometry(view(), band(), THUMB_W, GAP, n, insert, 0.)
    }

    /// The row with a phantom slot at `idx` open by `reveal`.
    fn strip_phantom(n: usize, idx: usize, reveal: f64) -> Strip {
        let insert = Some(Insert::Phantom(Phantom {
            idx,
            reveal,
            emerge: 0.,
        }));
        strip_geometry(view(), band(), THUMB_W, GAP, n, insert, 0.)
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

    /// A fully revealed phantom is a whole workspace, one gap past the end of the row —
    /// and the row it hangs off has not moved a pixel.
    #[test]
    fn a_full_phantom_hangs_off_the_end_without_moving_the_row() {
        let at_rest = strip(3, None);
        let strip = strip_phantom(3, 3, 1.);

        let (rect, _) = strip.phantom.expect("phantom rect must be laid out");
        assert_eq!(rect.size, Size::from((THUMB_W, THUMB_H)));
        let last = strip.thumbs[2];
        assert_eq!(rect.loc.x, last.loc.x + THUMB_W + GAP);

        // Every real thumbnail is exactly where it was with no slot open. This is the
        // whole point: the row is a still target for the duration of the drag, and only
        // catches up once the drop has made the workspace real.
        let xs: Vec<_> = strip.thumbs.iter().map(|r| r.loc.x).collect();
        let base: Vec<_> = at_rest.thumbs.iter().map(|r| r.loc.x).collect();
        assert_eq!(xs, base, "the open slot must not move the row");

        // Which means it is *not* the row a real fourth workspace produces — that row is
        // half a slot to the left, and reaching it is the drop's job.
        let real = self::strip(4, None);
        let shift = strip.thumbs[0].loc.x - real.thumbs[0].loc.x;
        assert!((shift - (THUMB_W + GAP) / 2.).abs() <= 1.);

        // A pointer over the phantom maps to the insertion point it stands for.
        let center = Point::from((rect.loc.x + rect.size.w / 2., BAND_Y + 40.));
        assert_eq!(strip.drop_target(center), Some(DropTarget::NewAt(3)));
    }

    /// A phantom at zero reveal costs nothing at all, so arming one is not itself a
    /// visible event — the slot only starts growing as the reveal does.
    #[test]
    fn a_zero_phantom_lays_the_row_out_unchanged() {
        let at_rest = strip(3, None);
        let strip = strip_phantom(3, 3, 0.);
        let (rect, _) = strip.phantom.unwrap();
        assert_eq!(rect.size.w, 0.);
        let xs: Vec<_> = strip.thumbs.iter().map(|r| r.loc.x).collect();
        let base: Vec<_> = at_rest.thumbs.iter().map(|r| r.loc.x).collect();
        assert_eq!(xs, base, "a zero phantom moved the row");
        // Including for the drop targets, which are taken against the bounds.
        assert_eq!(
            strip.bounds(),
            at_rest.bounds(),
            "a zero phantom stretched the row's bounds",
        );
    }

    /// The slot opens monotonically, and the row spreads with it — nothing overshoots or
    /// doubles back partway through, which is what a jerky reveal would look like.
    #[test]
    fn the_phantom_opens_monotonically() {
        let mut last_w = -1.;
        let mut last_run = -1.;
        for step in 0..=20 {
            let reveal = step as f64 / 20.;
            let strip = strip_phantom(3, 3, reveal);
            let (rect, _) = strip.phantom.unwrap();
            let run = strip.bounds().size.w;
            assert!(
                rect.size.w >= last_w && run >= last_run,
                "reveal {reveal}: slot {} run {run} went backwards",
                rect.size.w,
            );
            (last_w, last_run) = (rect.size.w, run);
        }
        // ...and the full reveal is a whole workspace wider than none.
        let none = strip_phantom(3, 3, 0.).bounds().size.w;
        assert!((last_run - none - (THUMB_W + GAP)).abs() <= 1.);
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
