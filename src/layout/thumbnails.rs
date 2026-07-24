//! The overview workspace thumbnails strip — gnome-shell's `ThumbnailsBox`
//! (js/ui/workspaceThumbnail.js), as pure geometry.
//!
//! The strip shows every workspace as a miniature, horizontally centered in
//! the band [`crate::ui::overview_layout`] allocates it just below the search
//! entry. With dynamic workspaces it only appears once there are more than
//! [`NUM_WORKSPACES_THRESHOLD`] workspaces, i.e. once a second desktop is
//! populated.

use smithay::utils::{Logical, Point, Rectangle, Size};

/// The thumbnail cap: 5% of the screen (`MAX_THUMBNAIL_SCALE`).
pub const MAX_THUMBNAIL_SCALE: f64 = 0.05;

/// With dynamic workspaces the strip shows only when there are more
/// workspaces than this (`NUM_WORKSPACES_THRESHOLD`).
pub const NUM_WORKSPACES_THRESHOLD: usize = 2;

/// Inter-thumbnail spacing: the theme's `$base_padding`.
pub const SPACING: f64 = 8.;

/// The active-workspace indicator's border width
/// (`.workspace-thumbnail-indicator` in the shell theme).
pub const INDICATOR_WIDTH: f64 = 3.;

/// How far a gap's drop zone cuts into the neighboring thumbnails
/// (`WORKSPACE_CUT_SIZE`).
pub const WORKSPACE_CUT_SIZE: f64 = 10.;

/// The width of the new-workspace drop placeholder (the theme's
/// `.placeholder`).
pub const PLACEHOLDER_WIDTH: f64 = 18.;

/// The laid-out strip.
#[derive(Debug)]
pub struct Strip {
    /// Scale from workspace to thumbnail coordinates.
    pub scale: f64,
    /// Per-workspace thumbnail rects, in view coordinates, workspace order.
    pub thumbs: Vec<Rectangle<f64, Logical>>,
    /// The new-workspace drop placeholder, when a drag hovers a gap.
    pub placeholder: Option<Rectangle<f64, Logical>>,
}

/// The thumbnail scale: 5% of the view, shrinking below the cap once `n`
/// thumbnails plus spacing no longer fit the width (gnome-shell's
/// `vfunc_get_preferred_height`).
fn thumb_scale(view_size: Size<f64, Logical>, n: usize) -> f64 {
    let avail = view_size.w - SPACING * 2.;
    f64::min(
        (avail - SPACING * (n - 1) as f64) / (view_size.w * n as f64),
        MAX_THUMBNAIL_SCALE,
    )
}

/// How tall a band the strip needs — gnome-shell's
/// `ThumbnailsBox.get_preferred_height`, which is what
/// [`crate::ui::overview_layout`] allocates it.
///
/// Divergence: gnome-shell measures against the work-area porthole
/// (`workspaceThumbnail.js:1248-1255`); our miniature is the whole view,
/// including the strip under the top panel, so we measure against the view.
pub fn preferred_height(view_size: Size<f64, Logical>, n: usize) -> f64 {
    (view_size.h * thumb_scale(view_size, n)).round()
}

/// Lays out the strip inside its allocated `band`: each thumbnail is the
/// workspace at 5% scale (smaller if the row wouldn't fit the view width),
/// the row horizontally centered in the band and anchored to its top —
/// gnome-shell allocates the box exactly `get_preferred_height` tall
/// (`overviewControls.js:184-196`), so at rest the two coincide. A
/// `placeholder` index makes room for the new-workspace drop placeholder
/// before that thumbnail (gnome-shell's drop placeholder).
pub fn strip_geometry(
    view_size: Size<f64, Logical>,
    band: Rectangle<f64, Logical>,
    n: usize,
    placeholder: Option<usize>,
) -> Strip {
    let scale = thumb_scale(view_size, n);

    let thumb_h = (view_size.h * scale).round();
    let thumb_w = (thumb_h * (view_size.w / view_size.h)).round();
    let thumb = Size::from((thumb_w, thumb_h));

    let extra = placeholder.map_or(0., |_| PLACEHOLDER_WIDTH + SPACING);
    let total_w = thumb_w * n as f64 + SPACING * (n - 1) as f64 + extra;
    let x0 = (band.loc.x + (band.size.w - total_w) / 2.).round();
    let y = band.loc.y.round();

    let mut x = x0;
    let mut placeholder_rect = None;
    let mut place = |i: usize, x: &mut f64| {
        if placeholder == Some(i) {
            placeholder_rect = Some(Rectangle::new(
                Point::from((*x, y)),
                Size::from((PLACEHOLDER_WIDTH, thumb_h)),
            ));
            *x += PLACEHOLDER_WIDTH + SPACING;
        }
    };
    let mut thumbs = Vec::with_capacity(n);
    for i in 0..n {
        place(i, &mut x);
        thumbs.push(Rectangle::new(Point::from((x, y)), thumb));
        x += thumb_w + SPACING;
    }
    place(n, &mut x);

    Strip {
        // The exact scale the rounded thumbnail size implies, so contents
        // fill it precisely.
        scale: thumb_h / view_size.h,
        thumbs,
        placeholder: placeholder_rect,
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

    /// The workspace whose thumbnail contains the position.
    pub fn thumb_under(&self, pos: Point<f64, Logical>) -> Option<usize> {
        self.thumbs.iter().position(|rect| rect.contains(pos))
    }

    /// The drop target at the position: a workspace, or a gap insertion
    /// index. Gaps cut [`WORKSPACE_CUT_SIZE`] into the neighboring
    /// thumbnails (gnome-shell's `_getPlaceholderTarget`); only interior
    /// gaps insert.
    pub fn drop_target(&self, pos: Point<f64, Logical>) -> Option<DropTarget> {
        let bounds = self.bounds();
        if !bounds.contains(pos) {
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

    /// The band [`crate::ui::overview_layout`] allocates for `n` thumbnails at
    /// the 1920×1080 / 35px-strut reference: y = 35 + 58 + 13.
    fn band(n: usize) -> Rectangle<f64, Logical> {
        Rectangle::new(
            Point::from((0., 106.)),
            Size::from((1920., preferred_height(view(), n))),
        )
    }

    #[test]
    fn three_thumbnails_at_the_gnome_cap() {
        // 5% of 1080 = 54 tall, 96 wide; row of three centered.
        let strip = strip_geometry(view(), band(3), 3, None);
        assert_eq!(strip.scale, 54. / 1080.);
        let expected_x0 = (1920. - (96. * 3. + 8. * 2.)) / 2.;
        assert_eq!(
            strip.thumbs[0],
            Rectangle::new(Point::from((expected_x0, 106.)), Size::from((96., 54.)))
        );
        assert_eq!(strip.thumbs[1].loc.x, expected_x0 + 96. + 8.);
        assert_eq!(strip.thumbs[2].loc.x, expected_x0 + (96. + 8.) * 2.);
    }

    /// The strip fills the band it was allocated, so the allocator is the only
    /// thing that decides whether it clears the top panel. (Before the
    /// `ControlsManagerLayout` port the strip centered itself in a band derived
    /// from the workspace zoom and had to re-derive the panel strut itself.)
    #[test]
    fn the_strip_fills_its_allocated_band() {
        let strip = strip_geometry(view(), band(3), 3, None);
        assert_eq!(preferred_height(view(), 3), 54.);
        assert_eq!(strip.thumbs[0].loc.y, 106.);
        assert_eq!(strip.thumbs[0].size.h, band(3).size.h);

        // Move the band and the whole row follows, with no re-centering.
        let moved = Rectangle::new(Point::from((40., 300.)), Size::from((800., 54.)));
        let strip = strip_geometry(view(), moved, 3, None);
        assert_eq!(strip.thumbs[0].loc.y, 300.);
        let total_w = 96. * 3. + 8. * 2.;
        assert_eq!(strip.thumbs[0].loc.x, 40. + (800. - total_w) / 2.);
    }

    #[test]
    fn placeholder_spreads_the_thumbnails_apart() {
        let at_rest = strip_geometry(view(), band(3), 3, None);
        let strip = strip_geometry(view(), band(3), 3, Some(1));

        let rect = strip
            .placeholder
            .expect("placeholder rect must be laid out");
        assert_eq!(rect.size, Size::from((PLACEHOLDER_WIDTH, 54.)));
        // It sits between the first two thumbnails, with normal spacing.
        assert_eq!(rect.loc.x, strip.thumbs[0].loc.x + 96. + SPACING);
        assert_eq!(
            strip.thumbs[1].loc.x,
            rect.loc.x + PLACEHOLDER_WIDTH + SPACING
        );
        // The row stays centered: it grew by the placeholder plus one gap.
        assert_eq!(
            strip.thumbs[0].loc.x,
            at_rest.thumbs[0].loc.x - (PLACEHOLDER_WIDTH + SPACING) / 2.,
        );

        // A pointer over the placeholder still maps to the same insertion
        // point, so the hover is stable.
        let center = Point::from((rect.loc.x + rect.size.w / 2., 130.));
        assert_eq!(strip.drop_target(center), Some(DropTarget::NewAt(1)));
    }

    #[test]
    fn many_thumbnails_shrink_to_fit() {
        let n = 25;
        let strip = strip_geometry(view(), band(n), n, None);
        assert!(strip.scale < MAX_THUMBNAIL_SCALE);
        let bounds = strip.bounds();
        assert!(bounds.loc.x >= 0. && bounds.loc.x + bounds.size.w <= 1920.);
    }

    #[test]
    fn drop_targets_split_thumbs_and_gaps() {
        let strip = strip_geometry(view(), band(3), 3, None);
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
