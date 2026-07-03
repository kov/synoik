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

/// The laid-out strip.
#[derive(Debug)]
pub struct Strip {
    /// Scale from workspace to thumbnail coordinates.
    pub scale: f64,
    /// Per-workspace thumbnail rects, in view coordinates, workspace order.
    pub thumbs: Vec<Rectangle<f64, Logical>>,
}

/// Lays out the strip: each thumbnail is the workspace at 5% scale (smaller
/// if the row wouldn't fit the view width), the row horizontally centered,
/// and vertically centered in the `top_band` tall margin above the workspace
/// row.
pub fn strip_geometry(view_size: Size<f64, Logical>, top_band: f64, n: usize) -> Strip {
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

    let total_w = thumb_w * n as f64 + SPACING * (n - 1) as f64;
    let x0 = ((view_size.w - total_w) / 2.).round();
    let y = ((top_band - thumb_h) / 2.).round();

    let thumbs = (0..n)
        .map(|i| Rectangle::new(Point::from((x0 + (thumb_w + SPACING) * i as f64, y)), thumb))
        .collect();

    Strip {
        // The exact scale the rounded thumbnail size implies, so contents
        // fill it precisely.
        scale: thumb_h / view_size.h,
        thumbs,
    }
}

impl Strip {
    /// The strip's overall bounding rect.
    pub fn bounds(&self) -> Rectangle<f64, Logical> {
        let first = self.thumbs[0];
        let last = self.thumbs[self.thumbs.len() - 1];
        Rectangle::new(
            first.loc,
            Size::from((last.loc.x + last.size.w - first.loc.x, first.size.h)),
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
        let strip = strip_geometry(view(), 108., 3);
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
    fn many_thumbnails_shrink_to_fit() {
        let n = 25;
        let strip = strip_geometry(view(), 108., n);
        assert!(strip.scale < MAX_THUMBNAIL_SCALE);
        let bounds = strip.bounds();
        assert!(bounds.loc.x >= 0. && bounds.loc.x + bounds.size.w <= 1920.);
    }

    #[test]
    fn drop_targets_split_thumbs_and_gaps() {
        let strip = strip_geometry(view(), 108., 3);
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
