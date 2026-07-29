//! The overview workspace thumbnails strip — gnome-shell's `ThumbnailsBox`
//! (js/ui/workspaceThumbnail.js), as pure geometry.
//!
//! The strip shows every workspace as a miniature, horizontally centered in
//! the band [`crate::ui::overview_layout`] allocates it just below the search
//! entry — or, once there are more workspaces than the band holds, scrolled to
//! follow the active one and clipped to the band. With dynamic workspaces it
//! only appears once there are more than [`NUM_WORKSPACES_THRESHOLD`]
//! workspaces, i.e. once a second desktop is populated.

use smithay::utils::{Logical, Point, Rectangle, Size};

/// With dynamic workspaces the strip shows only when there are more
/// workspaces than this (`NUM_WORKSPACES_THRESHOLD`).
pub const NUM_WORKSPACES_THRESHOLD: usize = 2;

/// Inter-thumbnail spacing.
///
/// **Divergence (approved 2026-07-29).** gnome-shell spaces thumbnails by the theme's
/// `$base_padding` (8). Ours is the app-grid row's `WORKSPACE_MIN_SPACING`, because the
/// strip is that row's twin — see [`crate::ui::overview_layout::small_workspace_height`].
/// **Adaptive chrome, rule 1 — ramped** with [`crate::ui::overview_layout::chrome_ramp`],
/// exactly as the row's own gap is.
pub const SPACING: f64 = 24.;

/// The active-workspace indicator's border width
/// (`.workspace-thumbnail-indicator` in the shell theme).
pub const INDICATOR_WIDTH: f64 = 3.;

/// How far a gap's drop zone cuts into the neighboring thumbnails
/// (`WORKSPACE_CUT_SIZE`).
pub const WORKSPACE_CUT_SIZE: f64 = 10.;

/// The width of the new-workspace drop placeholder (the theme's
/// `.placeholder`).
pub const PLACEHOLDER_WIDTH: f64 = 18.;

/// How far inside its band the row is allowed to come to rest.
///
/// The scroll clamps the row against this inset rather than against the band itself, so the
/// first and last workspaces keep room for their shadow — the active one's accent glow was
/// cut off flat where the row sat flush against the band edge. Comfortably more than the
/// glow's own reach. **Adaptive chrome, rule 1 — ramped.**
pub const EDGE_INSET: f64 = 32.;

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

/// Lays out the strip inside its allocated `band`: each thumbnail is `thumb_h` tall
/// (the app-grid row's workspace height — see
/// [`crate::ui::overview_layout::small_workspace_height`]) and keeps the output's aspect,
/// anchored to the band's top. gnome-shell allocates the box exactly
/// `get_preferred_height` tall (`overviewControls.js:184-196`), so at rest the band and the
/// row coincide. A `placeholder` index makes room for the new-workspace drop placeholder
/// before that thumbnail (gnome-shell's drop placeholder).
///
/// **Divergence (approved 2026-07-29).** gnome-shell sizes thumbnails at
/// `MAX_THUMBNAIL_SCALE` (0.05) and shrinks them further until the row fits its width
/// (`vfunc_get_preferred_height`), which past a dozen workspaces makes a row of specks.
/// Ours are the size of the app-grid row's workspaces, whatever the count, and the row
/// scrolls.
///
/// A row that fits is centered in the band, as gnome-shell's is. One that does not
/// **scrolls to follow `focus`** — the fractional active-workspace index, so it tracks a
/// workspace switch as it animates — clamped so the row never leaves a gap at either end.
/// This is the same rule the overflowing workspace row follows
/// ([`crate::layout::monitor::scroll_to_follow`]), and it replaces gnome-shell's
/// shrink-to-fit (see [`preferred_height`]).
pub fn strip_geometry(
    view_size: Size<f64, Logical>,
    band: Rectangle<f64, Logical>,
    thumb_h: f64,
    n: usize,
    placeholder: Option<usize>,
    focus: f64,
) -> Strip {
    let spacing = (SPACING * crate::ui::overview_layout::chrome_ramp(view_size)).round();
    let thumb_h = thumb_h.round();
    let thumb_w = (thumb_h * (view_size.w / view_size.h)).round();
    let thumb = Size::from((thumb_w, thumb_h));

    let extra = placeholder.map_or(0., |_| PLACEHOLDER_WIDTH + spacing);
    let total_w = thumb_w * n as f64 + spacing * (n - 1) as f64 + extra;
    let y = band.loc.y.round();

    // Laid out from the row's own origin first, so the scroll can be computed from
    // where the focused thumbnail actually landed — the placeholder displaces it.
    let mut x = 0.;
    let mut placeholder_rect = None;
    let mut place = |i: usize, x: &mut f64| {
        if placeholder == Some(i) {
            placeholder_rect = Some(Rectangle::new(
                Point::from((*x, y)),
                Size::from((PLACEHOLDER_WIDTH, thumb_h)),
            ));
            *x += PLACEHOLDER_WIDTH + spacing;
        }
    };
    let mut thumbs = Vec::with_capacity(n);
    for i in 0..n {
        place(i, &mut x);
        thumbs.push(Rectangle::new(Point::from((x, y)), thumb));
        x += thumb_w + spacing;
    }
    place(n, &mut x);

    // Where the active workspace's thumbnail sits along the row, interpolated between
    // its neighbours exactly as the indicator ring is, so the two scroll together.
    let idx = focus.clamp(0., (n - 1) as f64);
    let (lo, hi) = (idx.floor() as usize, idx.ceil() as usize);
    let t = idx.fract();
    let focus_x = thumbs[lo].loc.x + (thumbs[hi].loc.x - thumbs[lo].loc.x) * t;

    let inset = (EDGE_INSET * crate::ui::overview_layout::chrome_ramp(view_size)).round();
    let span = (band.size.w - inset * 2.).max(1.);
    let x0 = (band.loc.x
        + inset
        + super::monitor::scroll_to_follow(span, total_w, focus_x + thumb_w / 2.))
    .round();
    for rect in &mut thumbs {
        rect.loc.x += x0;
    }
    if let Some(rect) = &mut placeholder_rect {
        rect.loc.x += x0;
    }

    Strip {
        // The exact scale the rounded thumbnail size implies, so contents
        // fill it precisely.
        scale: thumb_h / view_size.h,
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

    /// The band width [`crate::ui::overview_layout`] allocates the strip at the 1920×1080
    /// reference: the view minus the floating search entry's zone (12 + 352 + 12) at each
    /// edge.
    const AVAIL_W: f64 = 1168.;
    /// Where that band starts.
    const BAND_X: f64 = 376.;

    /// The band [`crate::ui::overview_layout`] allocates the strip at the 1920×1080 /
    /// 35px-strut reference: y = 35 + 13 (the entry floats and no longer displaces the
    /// strip). Its height no longer depends on the workspace count — the row scrolls.
    fn band() -> Rectangle<f64, Logical> {
        Rectangle::new(Point::from((BAND_X, 48.)), Size::from((AVAIL_W, THUMB_H)))
    }

    /// One thumbnail at the 1920×1080 / 35px-strut reference: the app-grid row's
    /// workspace height, `round((1080 - 35) * SMALL_WORKSPACE_RATIO)`, and the output's
    /// aspect.
    const THUMB_H: f64 = 157.;
    const THUMB_W: f64 = 279.;

    /// The strip with the first workspace active, which is where every test that is
    /// not about scrolling wants to be.
    fn strip(n: usize, placeholder: Option<usize>) -> Strip {
        strip_geometry(view(), band(), THUMB_H, n, placeholder, 0.)
    }

    /// A thumbnail is the app-grid row's workspace, not gnome-shell's 5% speck: same
    /// height, same 24px spacing, the output's aspect.
    #[test]
    fn three_thumbnails_at_the_app_grid_size() {
        let strip = strip(3, None);
        assert_eq!(strip.scale, THUMB_H / 1080.);
        // Centered in the band, which is itself centered in the view.
        let expected_x0 = (BAND_X + (AVAIL_W - (THUMB_W * 3. + SPACING * 2.)) / 2.).round();
        assert_eq!(
            strip.thumbs[0],
            Rectangle::new(
                Point::from((expected_x0, 48.)),
                Size::from((THUMB_W, THUMB_H))
            )
        );
        assert_eq!(strip.thumbs[1].loc.x, expected_x0 + THUMB_W + SPACING);
        assert_eq!(
            strip.thumbs[2].loc.x,
            expected_x0 + (THUMB_W + SPACING) * 2.
        );
        // Still centered on the *view*, because the entry's zone is reserved at both edges.
        let right_gap = 1920. - (expected_x0 + THUMB_W * 3. + SPACING * 2.);
        assert!((strip.bounds().loc.x - right_gap).abs() <= 1.);
    }

    /// The row scrolls to follow the active workspace instead of shrinking to fit
    /// (the approved divergence). The scale is the cap whatever the count, and
    /// whichever workspace is active is fully inside the band the entry left.
    #[test]
    fn the_row_scrolls_to_the_active_workspace() {
        // 10 thumbs of 279 plus gaps = 3006, well past the 1168-wide band.
        let n = 10;
        for active in 0..n {
            let strip = strip_geometry(view(), band(), THUMB_H, n, None, active as f64);
            assert_eq!(
                strip.scale,
                THUMB_H / 1080.,
                "the size must not give way to the count"
            );
            let rect = strip.thumbs[active];
            assert!(
                rect.loc.x >= BAND_X && rect.loc.x + rect.size.w <= BAND_X + AVAIL_W,
                "thumbnail {active} is outside the band at {rect:?}"
            );
            // Rigid: the scroll moves the row, it never re-spaces it.
            for pair in strip.thumbs.windows(2) {
                assert_eq!(pair[1].loc.x - pair[0].loc.x, THUMB_W + SPACING);
            }
        }

        // The ends stay flush against the band — the clamp, not just the centering.
        // Flush against the row's resting inset, not the band edge: the first and last
        // workspaces keep room for their shadow (see `EDGE_INSET`).
        let first = strip_geometry(view(), band(), THUMB_H, n, None, 0.);
        assert_eq!(first.thumbs[0].loc.x, BAND_X + EDGE_INSET);
        let last = strip_geometry(view(), band(), THUMB_H, n, None, (n - 1) as f64);
        let tail = last.thumbs[n - 1];
        assert_eq!(tail.loc.x + tail.size.w, BAND_X + AVAIL_W - EDGE_INSET);

        // A fractional focus scrolls smoothly between the two, so the row tracks a
        // workspace switch as it animates rather than jumping at the halfway point.
        let mid = strip_geometry(view(), band(), THUMB_H, n, None, 4.5);
        let (a, b) = (
            strip_geometry(view(), band(), THUMB_H, n, None, 4.).thumbs[0]
                .loc
                .x,
            strip_geometry(view(), band(), THUMB_H, n, None, 5.).thumbs[0]
                .loc
                .x,
        );
        assert!(
            (mid.thumbs[0].loc.x - (a + b) / 2.).abs() <= 1.,
            "a half-step focus must scroll half a step"
        );
    }

    /// Only what is drawn can be hit: a scrolled row is clipped to the band, and past
    /// the band's edge is where the floating search entry lives.
    #[test]
    fn a_scrolled_thumbnail_is_not_hit_outside_the_band() {
        let n = 10;
        let strip = strip_geometry(view(), band(), THUMB_H, n, None, 0.);
        let y = 100.;

        // The row runs off the right edge; the first thumbnail past it is not clickable
        // even though its rect contains the point.
        let outside = strip
            .thumbs
            .iter()
            .position(|r| r.loc.x >= BAND_X + AVAIL_W)
            .expect("the row must overflow the band");
        let rect = strip.thumbs[outside];
        let pos = Point::from((rect.loc.x + rect.size.w / 2., y));
        assert!(rect.contains(pos), "the probe must be inside the rect");
        assert_eq!(strip.thumb_under(pos), None);
        assert_eq!(strip.drop_target(pos), None);

        // …while the one at the row's resting edge still is, on its visible part.
        assert_eq!(
            strip.thumb_under(Point::from((BAND_X + EDGE_INSET + 4., y))),
            Some(0)
        );
    }

    /// The strip fills the band it was allocated, so the allocator is the only
    /// thing that decides whether it clears the top panel. (Before the
    /// `ControlsManagerLayout` port the strip centered itself in a band derived
    /// from the workspace zoom and had to re-derive the panel strut itself.)
    #[test]
    fn the_strip_fills_its_allocated_band() {
        let strip = strip(3, None);
        assert_eq!(strip.thumbs[0].size.h, THUMB_H);
        assert_eq!(strip.thumbs[0].loc.y, 48.);
        assert_eq!(strip.thumbs[0].size.h, band().size.h);

        // Move the band and the whole row follows, with no re-centering.
        let moved = Rectangle::new(Point::from((40., 300.)), Size::from((800., THUMB_H)));
        let strip = strip_geometry(view(), moved, THUMB_H, 3, None, 0.);
        assert_eq!(strip.thumbs[0].loc.y, 300.);
        // 800 is not wide enough for three at 279 + 24, so the row scrolls: with the
        // first workspace active it starts flush at the band's left edge.
        let total_w = THUMB_W * 3. + SPACING * 2.;
        assert!(total_w > 800.);
        assert_eq!(strip.thumbs[0].loc.x, 40. + EDGE_INSET);
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
        assert_eq!(rect.loc.x, strip.thumbs[0].loc.x + THUMB_W + SPACING);
        assert_eq!(
            strip.thumbs[1].loc.x,
            rect.loc.x + PLACEHOLDER_WIDTH + SPACING
        );
        // The row stays centered: it grew by the placeholder plus one gap.
        assert!(
            (strip.thumbs[0].loc.x
                - (at_rest.thumbs[0].loc.x - (PLACEHOLDER_WIDTH + SPACING) / 2.))
                .abs()
                <= 1.
        );

        // A pointer over the placeholder still maps to the same insertion
        // point, so the hover is stable.
        let center = Point::from((rect.loc.x + rect.size.w / 2., 130.));
        assert_eq!(strip.drop_target(center), Some(DropTarget::NewAt(1)));
    }

    #[test]
    fn drop_targets_split_thumbs_and_gaps() {
        let strip = strip(3, None);
        let y = 130.;
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
        // Outside the strip: no target.
        assert_eq!(strip.drop_target(Point::from((10., y))), None);
        assert_eq!(
            strip.drop_target(Point::from((first.loc.x + 48., 500.))),
            None
        );
    }
}
