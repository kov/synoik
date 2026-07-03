//! The overview workspace thumbnails strip — gnome-shell's `ThumbnailsBox`
//! (js/ui/workspaceThumbnail.js), as pure geometry.
//!
//! The strip shows every workspace as a miniature, horizontally centered in
//! the band above the zoomed-out workspace row. With dynamic workspaces it
//! only appears once there are more than [`NUM_WORKSPACES_THRESHOLD`]
//! workspaces, i.e. once a second desktop is populated.

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

/// Lays out the strip: each thumbnail is the workspace at 5% scale (smaller
/// if the row wouldn't fit the view width), the row horizontally centered,
/// and vertically centered in the `top_band` tall margin above the workspace
/// row. A `placeholder` index makes room for the new-workspace drop
/// placeholder before that thumbnail (gnome-shell's drop placeholder).
pub fn strip_geometry(
    view_size: Size<f64, Logical>,
    top_band: f64,
    n: usize,
    placeholder: Option<usize>,
) -> Strip {
    // gnome-shell's get_preferred_height: the scale shrinks below the cap
    // when n thumbnails plus spacing exceed the available width.
    let avail = view_size.w - SPACING * 2.;
    let scale = f64::min(
        (avail - SPACING * (n - 1) as f64) / (view_size.w * n as f64),
        MAX_THUMBNAIL_SCALE,
    );

    let thumb_h = (view_size.h * scale).round();
    let thumb_w = (thumb_h * (view_size.w / view_size.h)).round();
    let thumb = Size::from((thumb_w, thumb_h));

    let extra = placeholder.map_or(0., |_| PLACEHOLDER_WIDTH + SPACING);
    let total_w = thumb_w * n as f64 + SPACING * (n - 1) as f64 + extra;
    let x0 = ((view_size.w - total_w) / 2.).round();
    let y = ((top_band - thumb_h) / 2.).round();

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

    #[test]
    fn three_thumbnails_at_the_gnome_cap() {
        // 5% of 1080 = 54 tall, 96 wide; row of three centered.
        let strip = strip_geometry(view(), 108., 3, None);
        assert_eq!(strip.scale, 54. / 1080.);
        let expected_x0 = (1920. - (96. * 3. + 8. * 2.)) / 2.;
        assert_eq!(
            strip.thumbs[0],
            Rectangle::new(Point::from((expected_x0, 27.)), Size::from((96., 54.)))
        );
        assert_eq!(strip.thumbs[1].loc.x, expected_x0 + 96. + 8.);
        assert_eq!(strip.thumbs[2].loc.x, expected_x0 + (96. + 8.) * 2.);
    }

    #[test]
    fn placeholder_spreads_the_thumbnails_apart() {
        let at_rest = strip_geometry(view(), 108., 3, None);
        let strip = strip_geometry(view(), 108., 3, Some(1));

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
        let center = Point::from((rect.loc.x + rect.size.w / 2., 40.));
        assert_eq!(strip.drop_target(center), Some(DropTarget::NewAt(1)));
    }

    #[test]
    fn many_thumbnails_shrink_to_fit() {
        let n = 25;
        let strip = strip_geometry(view(), 108., n, None);
        assert!(strip.scale < MAX_THUMBNAIL_SCALE);
        let bounds = strip.bounds();
        assert!(bounds.loc.x >= 0. && bounds.loc.x + bounds.size.w <= 1920.);
    }

    #[test]
    fn drop_targets_split_thumbs_and_gaps() {
        let strip = strip_geometry(view(), 108., 3, None);
        let y = 40.;
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
