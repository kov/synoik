// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Layer A — the St theme node: a resolved style bag plus the box-model math that
//! consumes it. See `docs/fork/layer-a-theme-node.md`.
//!
//! GNOME's St is *not* a general CSS engine: layout stays Clutter's job (which we
//! hand-roll), and CSS contributes only **border + padding + sizing** into size
//! negotiation, plus **paint** (background/border). [`ThemeNode`] models exactly that
//! subset of `st-theme-node.c` — the numbers each widget currently re-derives by hand
//! from the SCSS and Clutter allocation semantics, now derived once, correctly.
//!
//! Today a `ThemeNode` is built from typed constants; Layer B (a `cssparser`/cascade
//! fast-follow) will produce the *same* struct from the real `gnome-shell.css`, so
//! widgets consuming it never re-touch.
//!
//! The three ported St primitives, cited to 50.1 `src/st/`:
//! - [`ThemeNode::content_box`] ← `st_theme_node_get_content_box` (`st-theme-node.c:4268`)
//! - [`ThemeNode::allocation_for`] ← `st_theme_node_adjust_preferred_*` + `get_*_inc`
//!   (`st-theme-node.c:4161,4228`)
//! - [`allocate_align`] / [`allocate_1d`] ← Clutter's `clutter_actor_allocate_align_fill`, the
//!   cross-axis child placement a `BoxLayout` performs (the exact logic the dash running-dot fix
//!   `1495990b` turned on getting right).

use smithay::utils::{Logical, Point, Rectangle, Size};

use super::widget::{Painter, Rgba};

/// Per-side lengths in **logical** px (St's `ST_SIDE_*` order: top, right, bottom,
/// left). Used for both padding and per-side border width.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Edges {
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
    pub left: f64,
}

impl Edges {
    pub const ZERO: Edges = Edges::uniform(0.);

    /// Same length on all four sides (CSS `padding: v`).
    pub const fn uniform(v: f64) -> Self {
        Edges {
            top: v,
            right: v,
            bottom: v,
            left: v,
        }
    }

    /// Vertical then horizontal (CSS `padding: v h`).
    pub const fn symmetric(v: f64, h: f64) -> Self {
        Edges {
            top: v,
            right: h,
            bottom: v,
            left: h,
        }
    }

    /// `left + right`.
    pub fn horizontal(&self) -> f64 {
        self.left + self.right
    }

    /// `top + bottom`.
    pub fn vertical(&self) -> f64 {
        self.top + self.bottom
    }
}

/// A resolved styled node — the subset of `st-theme-node.c` we consume: the
/// box-model insets (border + padding), the background/border paint, a corner
/// radius, and optional fixed intrinsic size. Built from typed constants now, from
/// the parsed stylesheet under Layer B.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ThemeNode {
    pub padding: Edges,
    /// Per-side border **width** (logical px). Paint currently strokes only a
    /// uniform border (our stroke primitive is uniform); non-uniform widths still
    /// inset the box model correctly.
    pub border: Edges,
    pub border_color: Rgba,
    pub border_radius: f64,
    pub background: Option<Rgba>,
    /// CSS `width`/`height`: a fixed intrinsic size folded into the natural size
    /// (`max(content, fixed)`), like St's `adjust_preferred_*`.
    pub width: Option<f64>,
    pub height: Option<f64>,
}

impl ThemeNode {
    /// An unstyled node: no insets, no paint. A base to spread fields onto.
    pub const EMPTY: ThemeNode = ThemeNode {
        padding: Edges::ZERO,
        border: Edges::ZERO,
        border_color: [0., 0., 0., 0.],
        border_radius: 0.,
        background: None,
        width: None,
        height: None,
    };

    /// The box within an `alloc` occupied by the node's content — allocation minus
    /// border and padding. Faithful to `st_theme_node_get_content_box`
    /// (`st-theme-node.c:4268`): the origin inset is rounded to whole px and the
    /// content size is clamped at zero.
    pub fn content_box(&self, alloc: Rectangle<f64, Logical>) -> Rectangle<f64, Logical> {
        let nc_left = self.border.left + self.padding.left;
        let nc_top = self.border.top + self.padding.top;
        let nc_right = self.border.right + self.padding.right;
        let nc_bottom = self.border.bottom + self.padding.bottom;

        let x1 = nc_left.round();
        let y1 = nc_top.round();
        let cw = (alloc.size.w - nc_left - nc_right).max(0.);
        let ch = (alloc.size.h - nc_top - nc_bottom).max(0.);
        let x2 = (x1 + cw).round();
        let y2 = (y1 + ch).round();

        Rectangle::new(
            Point::from((alloc.loc.x + x1, alloc.loc.y + y1)),
            Size::from((x2 - x1, y2 - y1)),
        )
    }

    /// The allocation size needed to hold `content` — the inverse of
    /// [`content_box`](Self::content_box). Faithful to the *natural* channel of
    /// `st_theme_node_adjust_preferred_*`: clamp the content up to any fixed
    /// `width`/`height`, then add the border+padding increment (border widths
    /// individually rounded, per `get_width_inc`/`get_height_inc`).
    pub fn allocation_for(&self, content: Size<f64, Logical>) -> Size<f64, Logical> {
        let mut w = content.w;
        if let Some(fixed) = self.width {
            w = w.max(fixed);
        }
        w += self.border.left.round()
            + self.padding.left
            + self.border.right.round()
            + self.padding.right;

        let mut h = content.h;
        if let Some(fixed) = self.height {
            h = h.max(fixed);
        }
        h += self.border.top.round()
            + self.padding.top
            + self.border.bottom.round()
            + self.padding.bottom;

        Size::from((w, h))
    }

    /// Paint the node's background then border into `alloc` (the St paint order —
    /// background under border). Only a **uniform** border is stroked (our stroke
    /// primitive is uniform); a non-uniform border is a toolkit gap to grow when a
    /// widget first needs it, not faked here.
    pub fn paint(
        &self,
        p: &mut Painter<'_, '_, '_>,
        alloc: Rectangle<f64, Logical>,
    ) -> anyhow::Result<()> {
        if let Some(bg) = self.background {
            p.fill_rounded(alloc, self.border_radius, bg)?;
        }
        let bw = self.border.top;
        let uniform = self.border.right == bw && self.border.bottom == bw && self.border.left == bw;
        if bw > 0. && uniform && self.border_color[3] > 0. {
            p.stroke_rounded(alloc, self.border_radius, bw, self.border_color)?;
        }
        Ok(())
    }
}

/// One-axis child alignment within a parent span — Clutter's
/// `clutter_actor_allocate_align_fill` per axis. `Fill` stretches the child across
/// the whole span (the effective mode when the actor's `*-align` is `FILL`, e.g. a
/// `y_expand` StWidget filling its box); the others place the child's natural length
/// (clamped to the span) flush start / centred / flush end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align1 {
    Fill,
    Start,
    Center,
    End,
}

/// `(start, len)` for a child of natural length `natural` placed within
/// `[start, start + avail)` under `align`. `Fill` ignores `natural` and returns the
/// whole span; the others clamp `natural` to `avail`.
pub fn allocate_1d(start: f64, avail: f64, natural: f64, align: Align1) -> (f64, f64) {
    if align == Align1::Fill {
        return (start, avail);
    }
    let len = natural.clamp(0., avail);
    let extra = avail - len;
    let off = match align {
        Align1::Start => 0.,
        Align1::Center => extra / 2.,
        Align1::End => extra,
        Align1::Fill => unreachable!(),
    };
    (start + off, len)
}

/// Place a child of natural `size` inside `parent` under per-axis alignment — the
/// 2-D form of [`allocate_1d`], mirroring `clutter_actor_allocate_align_fill`.
pub fn allocate_align(
    parent: Rectangle<f64, Logical>,
    size: Size<f64, Logical>,
    h: Align1,
    v: Align1,
) -> Rectangle<f64, Logical> {
    let (x, w) = allocate_1d(parent.loc.x, parent.size.w, size.w, h);
    let (y, height) = allocate_1d(parent.loc.y, parent.size.h, size.h, v);
    Rectangle::new(Point::from((x, y)), Size::from((w, height)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> Rectangle<f64, Logical> {
        Rectangle::new(Point::from((x, y)), Size::from((w, h)))
    }

    /// `content_box` subtracts border + padding on every side, offset from the
    /// allocation's own origin (`st_theme_node_get_content_box`).
    #[test]
    fn content_box_subtracts_border_and_padding() {
        let node = ThemeNode {
            padding: Edges::uniform(6.),
            border: Edges::uniform(1.),
            ..ThemeNode::EMPTY
        };
        let cb = node.content_box(rect(10., 20., 100., 80.));
        // inset = 7 per side → origin (10+7, 20+7); size (100-14, 80-14).
        assert_eq!(cb, rect(17., 27., 86., 66.));
    }

    /// Content narrower than the insets clamps to zero, never negative.
    #[test]
    fn content_box_clamps_at_zero() {
        let node = ThemeNode {
            padding: Edges::uniform(60.),
            ..ThemeNode::EMPTY
        };
        let cb = node.content_box(rect(0., 0., 100., 100.));
        assert_eq!(cb.size.w, 0.);
        assert_eq!(cb.size.h, 0.);
    }

    /// `allocation_for` is the inverse: content + border + padding.
    #[test]
    fn allocation_for_adds_border_and_padding() {
        let plain = ThemeNode {
            padding: Edges::uniform(6.),
            ..ThemeNode::EMPTY
        };
        // 64 icon + 6 padding both sides = 76 (the dash `.overview-icon` tile).
        assert_eq!(
            plain.allocation_for(Size::from((64., 64.))),
            Size::from((76., 76.))
        );
        let bordered = ThemeNode {
            border: Edges::uniform(1.),
            ..plain
        };
        // + a 1px border both sides.
        assert_eq!(
            bordered.allocation_for(Size::from((64., 64.))),
            Size::from((78., 78.))
        );
    }

    /// A fixed `width` floors the natural size before the insets are added.
    #[test]
    fn allocation_for_respects_fixed_size() {
        let node = ThemeNode {
            padding: Edges::symmetric(0., 10.),
            width: Some(200.),
            ..ThemeNode::EMPTY
        };
        // max(64, 200) + 10 + 10.
        assert_eq!(node.allocation_for(Size::from((64., 20.))).w, 220.);
    }

    /// For whole-px insets the two are exact inverses (round-trip).
    #[test]
    fn content_box_round_trips_allocation_for() {
        let node = ThemeNode {
            padding: Edges::uniform(12.),
            border: Edges::uniform(1.),
            ..ThemeNode::EMPTY
        };
        let content = Size::from((90., 40.));
        let alloc_size = node.allocation_for(content);
        let cb = node.content_box(Rectangle::new(Point::from((0., 0.)), alloc_size));
        assert_eq!(cb.size, content);
    }

    /// `allocate_1d`: Fill spans everything; Start/Center/End place the clamped
    /// natural length flush-start / centred / flush-end.
    #[test]
    fn allocate_1d_covers_every_alignment() {
        assert_eq!(allocate_1d(0., 100., 5., Align1::Fill), (0., 100.));
        assert_eq!(allocate_1d(0., 100., 5., Align1::Start), (0., 5.));
        assert_eq!(allocate_1d(0., 100., 5., Align1::Center), (47.5, 5.));
        assert_eq!(allocate_1d(0., 100., 5., Align1::End), (95., 5.));
        // natural larger than the span clamps to the span.
        assert_eq!(allocate_1d(10., 4., 5., Align1::Center), (10., 4.));
    }

    /// `allocate_align` places a bottom-centre child — the dash running dot's
    /// pre-translation spot (`y_align: END`, `x_align: CENTER`).
    #[test]
    fn allocate_align_places_bottom_centre() {
        let parent = rect(10., 20., 100., 100.);
        let dot = allocate_align(parent, Size::from((5., 5.)), Align1::Center, Align1::End);
        assert_eq!(dot, rect(10. + 47.5, 20. + 95., 5., 5.));
    }
}
