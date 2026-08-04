// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! A rounded solid-colour rectangle as a *render element*.
//!
//! The same SDF material the toolkit's [`Painter::fill_rounded`] paints
//! (`sdf_rect.frag`, via [`VulkanFrame::render_rounded_rect`]) — but drawn straight into
//! the frame instead of into an offscreen bake.
//!
//! Why both exist: a bake is a GPU round trip, so it pays off only when the result is
//! reused across frames. A shape whose *geometry* animates never hits its cache, and the
//! trick that saves an animated pill — bake it opaque, ride the fade on the element's
//! alpha ([`crate::ui::panel::pill_element`]) — cannot help, because a size cannot be
//! composited on. Such a shape has to be drawn where the frame is already recording. The
//! panel's workspace dots are the case that motivated this: they interpolate their width,
//! height and opacity per frame for the length of every workspace switch.
//!
//! Rule of thumb: a rounded fill whose size is settled belongs in a bake with the rest of
//! its widget (one texture, one draw); a rounded fill whose size moves belongs here.

use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::utils::{CommitCounter, OpaqueRegions};
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size};

use super::vulkan::{VulkanError, VulkanFrame, VulkanRenderer};

/// The persistent identity behind a [`RoundedSolidRenderElement`] — an [`Id`] and the
/// commit counter that tells damage tracking the shape changed.
///
/// Kept by the widget across frames for the same reason [`super::solid_color::SolidColorBuffer`]
/// is: an element built with a fresh `Id` every frame reads to the damage tracker as a
/// *different* element each time, which costs a full redraw of its area and loses the
/// "same element, new geometry" path. Nothing is allocated here — the name follows
/// smithay's buffer convention, but a rounded rect has no pixels of its own.
#[derive(Debug, Clone)]
pub struct RoundedSolidBuffer {
    id: Id,
    commit: CommitCounter,
    size: Size<f64, Logical>,
    /// Corner radius in logical units. The shader clamps it to half the shorter side, so
    /// any large value is a stadium and `size.h / 2.` on a square is a circle.
    corner_radius: f64,
    /// Straight-alpha RGBA, the toolkit's convention; the frame premultiplies.
    color: [f32; 4],
}

impl Default for RoundedSolidBuffer {
    fn default() -> Self {
        Self {
            id: Id::new(),
            commit: CommitCounter::default(),
            size: Size::default(),
            corner_radius: 0.,
            color: [0., 0., 0., 0.],
        }
    }
}

impl RoundedSolidBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the shape, bumping the commit counter only when something actually changed.
    ///
    /// The location is deliberately not here: it lives on the element, and smithay damages
    /// the union of an element's old and new geometry when it moves.
    pub fn update(&mut self, size: Size<f64, Logical>, corner_radius: f64, color: [f32; 4]) {
        if self.size != size || self.corner_radius != corner_radius || self.color != color {
            self.size = size;
            self.corner_radius = corner_radius;
            self.color = color;
            self.commit.increment();
        }
    }

    pub fn size(&self) -> Size<f64, Logical> {
        self.size
    }
}

/// A [`RoundedSolidBuffer`] placed at a location, ready to draw.
#[derive(Debug, Clone)]
pub struct RoundedSolidRenderElement {
    id: Id,
    commit: CommitCounter,
    geometry: Rectangle<f64, Logical>,
    corner_radius: f64,
    color: [f32; 4],
    scale: f64,
    kind: Kind,
}

impl RoundedSolidRenderElement {
    pub fn from_buffer(
        buffer: &RoundedSolidBuffer,
        location: impl Into<Point<f64, Logical>>,
        scale: Scale<f64>,
        kind: Kind,
    ) -> Self {
        Self {
            id: buffer.id.clone(),
            commit: buffer.commit,
            geometry: Rectangle::new(location.into(), buffer.size),
            corner_radius: buffer.corner_radius,
            color: buffer.color,
            scale: scale.x,
            kind,
        }
    }
}

impl Element for RoundedSolidRenderElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.commit
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        Rectangle::from_size(Size::from((1., 1.)))
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.geometry.to_physical_precise_round(scale)
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        // A cut corner is transparent, so a rounded fill occludes nothing — claiming
        // otherwise would let occlusion culling drop whatever shows through the corners.
        if self.corner_radius > 0. || self.color[3] < 1. {
            return OpaqueRegions::default();
        }
        let rect = Rectangle::from_size(self.geometry.size).to_physical_precise_down(scale);
        OpaqueRegions::from_slice(&[rect])
    }

    fn alpha(&self) -> f32 {
        self.color[3]
    }

    fn kind(&self) -> Kind {
        self.kind
    }
}

impl RenderElement<VulkanRenderer> for RoundedSolidRenderElement {
    fn draw(
        &self,
        frame: &mut VulkanFrame<'_, '_>,
        _src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), VulkanError> {
        // The radius is logical and the SDF pipeline is in physical pixels of `dst`. An
        // outer `RescaleRenderElement` (overview zoom, thumbnail strip) scales `dst`
        // relative to our own geometry without touching `self.scale`, so fold that ratio
        // in and the radius tracks the on-screen size — same correction, and the same
        // reason, as `RoundedTextureRenderElement::draw`. With no rescale it is 1.
        let geo_w = self.geometry(Scale::from(self.scale)).size.w;
        let rescale = if geo_w != 0 {
            dst.size.w as f32 / geo_w as f32
        } else {
            1.
        };
        let radius_px = (self.corner_radius * self.scale) as f32 * rescale;
        frame.render_rounded_rect(self.color, radius_px, dst, damage)
    }

    fn underlying_storage(&self, _renderer: &mut VulkanRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The buffer only bumps its commit when the shape really changed — re-setting the
    /// same values every frame (what a settled widget does) must not damage anything.
    #[test]
    fn an_unchanged_update_is_not_a_change() {
        let mut buffer = RoundedSolidBuffer::new();
        let size = Size::from((12., 12.));
        buffer.update(size, 6., [1., 1., 1., 1.]);
        let after_first = buffer.commit;
        buffer.update(size, 6., [1., 1., 1., 1.]);
        assert_eq!(
            buffer.commit, after_first,
            "re-stating the same shape damaged the element"
        );

        // Each of the three inputs is on its own — an animating dot changes all three.
        buffer.update(Size::from((18., 12.)), 6., [1., 1., 1., 1.]);
        assert_ne!(buffer.commit, after_first, "a resize was not a change");
        let after_size = buffer.commit;
        buffer.update(Size::from((18., 12.)), 9., [1., 1., 1., 1.]);
        assert_ne!(buffer.commit, after_size, "a new radius was not a change");
        let after_radius = buffer.commit;
        buffer.update(Size::from((18., 12.)), 9., [1., 1., 1., 0.5]);
        assert_ne!(buffer.commit, after_radius, "a new colour was not a change");
    }

    /// A rounded fill never claims an opaque region: its corners are cut, so anything
    /// behind them shows through and occlusion culling must not drop it.
    #[test]
    fn a_rounded_fill_occludes_nothing() {
        let mut buffer = RoundedSolidBuffer::new();
        buffer.update(Size::from((20., 20.)), 10., [1., 1., 1., 1.]);
        let element = RoundedSolidRenderElement::from_buffer(
            &buffer,
            (0., 0.),
            Scale::from(1.),
            Kind::Unspecified,
        );
        assert!(
            element.opaque_regions(Scale::from(1.)).is_empty(),
            "a rounded fill claimed an opaque region"
        );

        // A square, fully opaque one may — that is the only shape whose pixels are all there.
        buffer.update(Size::from((20., 20.)), 0., [1., 1., 1., 1.]);
        let square = RoundedSolidRenderElement::from_buffer(
            &buffer,
            (0., 0.),
            Scale::from(1.),
            Kind::Unspecified,
        );
        assert!(!square.opaque_regions(Scale::from(1.)).is_empty());

        // ...unless it is translucent.
        buffer.update(Size::from((20., 20.)), 0., [1., 1., 1., 0.5]);
        let faded = RoundedSolidRenderElement::from_buffer(
            &buffer,
            (0., 0.),
            Scale::from(1.),
            Kind::Unspecified,
        );
        assert!(faded.opaque_regions(Scale::from(1.)).is_empty());
    }
}
