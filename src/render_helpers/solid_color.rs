// SPDX-License-Identifier: GPL-3.0-only
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::utils::{CommitCounter, OpaqueRegions};
use smithay::backend::renderer::{Color32F, Frame as _, Renderer};
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size};

/// Smithay's solid color buffer, but with fractional scale.
#[derive(Debug, Clone)]
pub struct SolidColorBuffer {
    id: Id,
    size: Size<f64, Logical>,
    commit: CommitCounter,
    color: Color32F,
}

/// Render element for a [`SolidColorBuffer`].
#[derive(Debug, Clone)]
pub struct SolidColorRenderElement {
    id: Id,
    geometry: Rectangle<f64, Logical>,
    commit: CommitCounter,
    color: Color32F,
    kind: Kind,
}

impl Default for SolidColorBuffer {
    fn default() -> Self {
        Self {
            id: Id::new(),
            size: Default::default(),
            commit: Default::default(),
            color: Default::default(),
        }
    }
}

impl SolidColorBuffer {
    pub fn new(size: impl Into<Size<f64, Logical>>, color: impl Into<Color32F>) -> Self {
        SolidColorBuffer {
            id: Id::new(),
            color: color.into(),
            commit: CommitCounter::default(),
            size: size.into(),
        }
    }

    pub fn resize(&mut self, size: impl Into<Size<f64, Logical>>) {
        let size = size.into();
        if size != self.size {
            self.size = size;
            self.commit.increment();
        }
    }

    pub fn set_color(&mut self, color: impl Into<Color32F>) {
        let color = color.into();
        if color != self.color {
            self.color = color;
            self.commit.increment();
        }
    }

    pub fn update(&mut self, size: impl Into<Size<f64, Logical>>, color: impl Into<Color32F>) {
        let size = size.into();
        let color = color.into();
        if size != self.size || color != self.color {
            self.size = size;
            self.color = color;
            self.commit.increment();
        }
    }

    pub fn color(&self) -> Color32F {
        self.color
    }

    pub fn size(&self) -> Size<f64, Logical> {
        self.size
    }
}

impl SolidColorRenderElement {
    pub fn from_buffer(
        buffer: &SolidColorBuffer,
        location: impl Into<Point<f64, Logical>>,
        alpha: f32,
        kind: Kind,
    ) -> Self {
        let geo = Rectangle::new(location.into(), buffer.size());
        let color = buffer.color * alpha;
        Self::new(buffer.id.clone(), geo, buffer.commit, color, kind)
    }

    pub fn new(
        id: Id,
        geometry: Rectangle<f64, Logical>,
        commit: CommitCounter,
        color: Color32F,
        kind: Kind,
    ) -> Self {
        SolidColorRenderElement {
            id,
            geometry,
            commit,
            color,
            kind,
        }
    }

    pub fn color(&self) -> Color32F {
        self.color
    }

    pub fn geo(&self) -> Rectangle<f64, Logical> {
        self.geometry
    }
}

impl Element for SolidColorRenderElement {
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
        if self.color.is_opaque() {
            let rect = Rectangle::from_size(self.geometry.size).to_physical_precise_down(scale);
            OpaqueRegions::from_slice(&[rect])
        } else {
            OpaqueRegions::default()
        }
    }

    fn alpha(&self) -> f32 {
        self.color.a()
    }

    fn kind(&self) -> Kind {
        self.kind
    }
}

impl<R: Renderer> RenderElement<R> for SolidColorRenderElement {
    fn draw(
        &self,
        frame: &mut R::Frame<'_, '_>,
        _src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), R::Error> {
        frame.draw_solid(dst, damage, self.color)
    }

    #[inline]
    fn underlying_storage(&self, _renderer: &mut R) -> Option<UnderlyingStorage<'_>> {
        None
    }
}

/// A rescaled edge must land where the arithmetic puts it.
///
/// The overview's window previews are drawn through smithay's `RescaleRenderElement`
/// (`workspace.rs` `render_expose`), which animates *both* the scale and the origin it scales
/// about. `Rectangle::to_i32_round` rounds a rect's location and size **independently**, so the
/// far edge came out as `origin + round((loc−origin)·z) + round(size·z)` — two roundings whose
/// errors add, landing up to a pixel away from the real edge and flipping between frames as the
/// animation's per-frame change goes sub-pixel. Measured on 2026-07-28 at output scale 2.25: a
/// preview's bottom edge read 602, 602, 601, 602, 602, 601 over consecutive frames while the
/// workspace behind it did not move at all — the shimmering bottom edge Gustavo reported.
///
/// Our smithay fork rounds the extremities, which is exactly "the edge lands at `round(edge)`".
/// That is what this pins; it fails on the independent rounding.
#[cfg(test)]
mod rescale_tests {
    use smithay::backend::renderer::element::utils::RescaleRenderElement;
    use smithay::backend::renderer::element::{Element, Kind};

    use super::*;

    /// `RelocateRenderElement` moves an element's `geometry` but forwards its `opaque_regions`
    /// unmoved, and the two are in the same physical space — so a relocated opaque element claims
    /// to be opaque where it no longer is.
    ///
    /// The correctness half is worse than the cost half. A relocation moves the claim *onto* pixels
    /// the element does not cover, so the tracker can skip drawing something that is actually
    /// visible — stale framebuffer, not a slow frame. We have no reproduction of either half yet;
    /// this test pins the mechanism only.
    ///
    /// It is a live *suspect*, not a diagnosis. A full-damage desktop frame on kov's seat shades
    /// 1.9x the output, because the scene's bottom element is a full-output opaque backdrop
    /// (`Synoik::render_output`'s `push(backdrop)`) that `DrmCompositor` would skip if the
    /// wallpaper above it declared full coverage — and it declares 0.98x. Whether *this* is why it
    /// falls short is unestablished: the relocation would have to be non-zero, and a full-output
    /// wallpaper's is likely zero. Do not cite this test as the cause of that until the element is
    /// identified.
    ///
    /// Upstream, not ours: fixing it means relocating the regions in
    /// `RelocateRenderElement::opaque_regions`, exactly as `geometry` already does. Pinned here so
    /// that whenever we take a smithay bump, this fails and tells us the workaround can go.
    #[test]
    fn relocating_an_element_does_not_move_the_region_it_claims_is_opaque() {
        use smithay::backend::renderer::element::utils::{Relocate, RelocateRenderElement};

        let logical = Rectangle::new(Point::from((0., 0.)), Size::from((100., 100.)));
        let opaque = SolidColorRenderElement::new(
            Id::new(),
            logical,
            CommitCounter::default(),
            Color32F::from([0., 0., 0., 1.]),
            Kind::Unspecified,
        );
        let scale = Scale::from(1.);
        assert_eq!(
            opaque.opaque_regions(scale).to_vec(),
            vec![Rectangle::new(Point::from((0, 0)), Size::from((100, 100)))],
            "an opaque solid color covers its whole geometry — the premise of the rest",
        );

        let moved = RelocateRenderElement::from_element(
            opaque,
            Point::<i32, smithay::utils::Physical>::from((40, 40)),
            Relocate::Relative,
        );

        assert_eq!(
            moved.geometry(scale).loc,
            Point::from((40, 40)),
            "geometry follows the relocation",
        );
        assert_eq!(
            moved.opaque_regions(scale).to_vec(),
            vec![Rectangle::new(Point::from((0, 0)), Size::from((100, 100)))],
            "and the opaque region does not — it still claims the pre-move rectangle. When this \
             fails, smithay has been fixed: drop the workaround this pins and delete the test.",
        );
    }

    #[test]
    fn a_rescaled_edge_lands_where_the_arithmetic_puts_it() {
        // A window-ish box, at the fractional scale the live session runs at.
        let logical = Rectangle::new(Point::from((100., 137.)), Size::from((640., 400.)));
        let scale = Scale::from(2.25);
        let physical = SolidColorRenderElement::new(
            Id::new(),
            logical,
            CommitCounter::default(),
            Color32F::from([1., 0., 0., 1.]),
            Kind::Unspecified,
        )
        .geometry(scale)
        .to_f64();

        // Sweep the shape of an expose animation: the tile shrinks toward its slot while the
        // origin it scales about travels there too. Neither alone reproduces this — with a fixed
        // origin both roundings grow together and the error cancels.
        for step in 0..=600 {
            let progress = f64::from(step) / 600.;
            let zoom = 1. - 0.45 * progress;
            let origin = Point::<i32, crate::render_helpers::Physical>::from((
                (60. + 500. * progress).round() as i32,
                (40. + 430. * progress).round() as i32,
            ));

            let rescaled = RescaleRenderElement::from_element(
                SolidColorRenderElement::new(
                    Id::new(),
                    logical,
                    CommitCounter::default(),
                    Color32F::from([1., 0., 0., 1.]),
                    Kind::Unspecified,
                ),
                origin,
                zoom,
            );
            let geo = rescaled.geometry(scale);

            for (axis, got, o, loc, size) in [
                (
                    "x",
                    geo.loc.x + geo.size.w,
                    f64::from(origin.x),
                    physical.loc.x,
                    physical.size.w,
                ),
                (
                    "y",
                    geo.loc.y + geo.size.h,
                    f64::from(origin.y),
                    physical.loc.y,
                    physical.size.h,
                ),
            ] {
                let exact = o + (loc - o) * zoom + size * zoom;
                assert_eq!(
                    f64::from(got),
                    exact.round(),
                    "{axis} far edge at zoom {zoom}, origin {o}: rounding the location and the \
                     size apart puts the edge {} off",
                    f64::from(got) - exact.round(),
                );
            }
        }
    }

    /// Two elements that abut must still abut after a rescale.
    ///
    /// This is what makes the fix matter beyond a shimmer: in the overview every element is
    /// wrapped in its **own** `RescaleRenderElement` (`workspace.rs` `render_expose`), so a
    /// window's bottom and the workspace edge under it are rounded independently. When their
    /// shared extremity maps to two different pixels, a one-pixel gap opens and the overview
    /// backdrop shows through it — Gustavo's "the background is bleeding through", at the one
    /// place the report named: the bottom of the workspace.
    #[test]
    fn two_abutting_elements_stay_abutting() {
        let scale = Scale::from(2.25);
        // A window ending exactly where the workspace edge below it begins.
        let seam = 537.;
        let upper = Rectangle::new(Point::from((100., 137.)), Size::from((640., seam - 137.)));
        let lower = Rectangle::new(Point::from((100., seam)), Size::from((640., 120.)));

        for step in 0..=400 {
            let progress = f64::from(step) / 400.;
            let zoom = 1. - 0.45 * progress;
            // The origin travels too, as a tile flying to its slot does. Holding it still — and
            // worse, holding it *on* one of the element edges — makes the two roundings agree by
            // construction and the test proves nothing.
            let origin = Point::<i32, crate::render_helpers::Physical>::from((
                (60. + 500. * progress).round() as i32,
                (40. + 430. * progress).round() as i32,
            ));

            let rescaled = |geo: Rectangle<f64, Logical>| {
                RescaleRenderElement::from_element(
                    SolidColorRenderElement::new(
                        Id::new(),
                        geo,
                        CommitCounter::default(),
                        Color32F::from([1., 0., 0., 1.]),
                        Kind::Unspecified,
                    ),
                    origin,
                    zoom,
                )
                .geometry(scale)
            };

            let upper = rescaled(upper);
            let lower = rescaled(lower);
            assert_eq!(
                upper.loc.y + upper.size.h,
                lower.loc.y,
                "zoom {zoom}: the seam split, {upper:?} then {lower:?}"
            );
        }
    }
}
