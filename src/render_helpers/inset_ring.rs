//! A rectangular ring drawn *inside* a rect — a CSS `border` on a box with no background.
//!
//! This is the shape St gives any widget with a `border` and no `background-color`: the stroke
//! eats the outer `width` px of the widget's own allocation and the middle stays transparent.
//! [`FocusRing`](crate::layout::focus_ring::FocusRing) is the *outset* sibling — it grows the
//! window by the border width and draws around it — so neither can stand in for the other.
//!
//! Four solid rects rather than one shader pass, because the interior must not be touched: a
//! single [`BorderRenderElement`](crate::render_helpers::border::BorderRenderElement) fills its
//! whole area (its `border_width` only shapes the inner rounding), which would paint the window
//! this ring exists to frame.
//!
//! Square corners only, which is what the one caller needs (`.cycler-highlight` sets a plain
//! `border` and no radius, `_switcher-popup.scss:80-82`). Rounding it means four corner pieces
//! like `FocusRing`'s, and that is the moment to grow this into a shared stroke primitive rather
//! than to special-case it here.

use smithay::backend::renderer::element::Kind;
use smithay::utils::{Logical, Point, Rectangle, Size};
use synoik_config::Color;

use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};

/// Top, bottom, left, right — kept as buffers so a ring that does not move does not re-damage.
#[derive(Debug)]
pub struct InsetRing {
    buffers: [SolidColorBuffer; 4],
    locs: [Point<f64, Logical>; 4],
    /// Zero-sized until the first [`update`](Self::update), which is also how a ring with nothing
    /// to frame renders nothing.
    live: bool,
}

impl Default for InsetRing {
    fn default() -> Self {
        Self::new()
    }
}

impl InsetRing {
    pub fn new() -> Self {
        Self {
            buffers: Default::default(),
            locs: Default::default(),
            live: false,
        }
    }

    /// Frame `rect` with a `width`-px stroke of `color`, drawn inside it.
    ///
    /// A rect too small to hold two strokes is drawn filled rather than with overlapping edges,
    /// which is what the browser box model does with an over-thick border as well.
    pub fn update(&mut self, rect: Rectangle<f64, Logical>, width: f64, color: Color) {
        let w = width.min(rect.size.w / 2.).max(0.);
        let h = width.min(rect.size.h / 2.).max(0.);
        let inner_h = (rect.size.h - h * 2.).max(0.);

        let sizes = [
            Size::from((rect.size.w, h)),
            Size::from((rect.size.w, h)),
            Size::from((w, inner_h)),
            Size::from((w, inner_h)),
        ];
        self.locs = [
            rect.loc,
            Point::from((rect.loc.x, rect.loc.y + rect.size.h - h)),
            Point::from((rect.loc.x, rect.loc.y + h)),
            Point::from((rect.loc.x + rect.size.w - w, rect.loc.y + h)),
        ];
        for (buf, size) in self.buffers.iter_mut().zip(sizes) {
            buf.update(size, color);
        }
        self.live = rect.size.w > 0. && rect.size.h > 0. && width > 0.;
    }

    pub fn render(&self, push: &mut dyn FnMut(SolidColorRenderElement)) {
        if !self.live {
            return;
        }
        for (buf, loc) in self.buffers.iter().zip(self.locs) {
            if buf.size().w <= 0. || buf.size().h <= 0. {
                continue;
            }
            push(SolidColorRenderElement::from_buffer(
                buf,
                loc,
                1.,
                Kind::Unspecified,
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCENT: Color = Color::new_unpremul(0., 0.5, 1., 1.);

    fn boxes(ring: &InsetRing) -> Vec<Rectangle<f64, Logical>> {
        let mut out = Vec::new();
        ring.render(&mut |elem| out.push(elem.geo()));
        out
    }

    /// The stroke lands *inside* the rect and leaves the middle untouched — the whole point of
    /// this helper. A ring that covered its interior would hide the window it frames.
    #[test]
    fn the_stroke_stays_inside_and_leaves_a_hole() {
        let mut ring = InsetRing::new();
        let rect = Rectangle::new(Point::from((100., 50.)), Size::from((400., 300.)));
        ring.update(rect, 5., ACCENT);

        let boxes = boxes(&ring);
        assert_eq!(boxes.len(), 4);
        for b in &boxes {
            assert!(rect.contains_rect(*b), "{b:?} escaped {rect:?}");
        }

        // Nothing covers the centre.
        let centre = Point::from((300., 200.));
        assert!(!boxes.iter().any(|b| b.contains(centre)));

        // But every edge is covered, one stroke deep.
        for probe in [
            Point::from((300., 52.)),
            Point::from((300., 348.)),
            Point::from((102., 200.)),
            Point::from((498., 200.)),
        ] {
            assert!(
                boxes.iter().any(|b| b.contains(probe)),
                "{probe:?} is not stroked"
            );
        }
    }

    /// A border thicker than half the box collapses to a fill instead of drawing edges that
    /// overlap and double their alpha.
    #[test]
    fn an_over_thick_stroke_becomes_a_fill() {
        let mut ring = InsetRing::new();
        let rect = Rectangle::new(Point::from((0., 0.)), Size::from((20., 10.)));
        ring.update(rect, 40., ACCENT);

        let boxes = boxes(&ring);
        // The two horizontal halves cover it; the verticals collapse to zero height.
        assert!(boxes.iter().any(|b| b.contains(Point::from((10., 5.)))));
        assert_eq!(
            boxes.iter().map(|b| b.size.w * b.size.h).sum::<f64>(),
            rect.size.w * rect.size.h
        );
    }

    /// A zero-sized target renders nothing at all rather than a degenerate element.
    #[test]
    fn nothing_to_frame_draws_nothing() {
        let mut ring = InsetRing::new();
        ring.update(Rectangle::default(), 5., ACCENT);
        assert!(boxes(&ring).is_empty());
    }
}
