use niri_config::{Color, CornerRadius};
use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::utils::CommitCounter;
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size};

/// Renders a rounded rectangle shadow.
///
/// Cloned per frame by its owners with the `Id` carried along, so clones share one damage identity.
#[derive(Debug, Clone)]
pub struct ShadowRenderElement {
    id: Id,
    commit_counter: CommitCounter,
    /// Where the element is drawn. Its size is [`Parameters::size`]; only [`Self::with_location`]
    /// sets the location, and it deliberately does not bump the commit counter.
    location: Point<f64, Logical>,
    params: Parameters,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Parameters {
    size: Size<f64, Logical>,
    geometry: Rectangle<f64, Logical>,
    color: Color,
    sigma: f32,
    corner_radius: CornerRadius,
    // Should only be used for visual improvements, i.e. corner radius anti-aliasing.
    scale: f32,
    alpha: f32,

    window_geometry: Rectangle<f64, Logical>,
    window_corner_radius: CornerRadius,
}

impl ShadowRenderElement {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        size: Size<f64, Logical>,
        geometry: Rectangle<f64, Logical>,
        color: Color,
        sigma: f32,
        corner_radius: CornerRadius,
        scale: f32,
        window_geometry: Rectangle<f64, Logical>,
        window_corner_radius: CornerRadius,
        alpha: f32,
    ) -> Self {
        Self {
            id: Id::new(),
            commit_counter: CommitCounter::default(),
            location: Point::default(),
            params: Parameters {
                size,
                geometry,
                color,
                sigma,
                corner_radius,
                scale,
                alpha,
                window_geometry,
                window_corner_radius,
            },
        }
    }

    pub fn empty() -> Self {
        Self {
            id: Id::new(),
            commit_counter: CommitCounter::default(),
            location: Point::default(),
            params: Parameters {
                size: Default::default(),
                geometry: Default::default(),
                color: Default::default(),
                sigma: 0.,
                corner_radius: Default::default(),
                scale: 1.,
                alpha: 1.,
                window_geometry: Default::default(),
                window_corner_radius: Default::default(),
            },
        }
    }

    pub fn damage_all(&mut self) {
        self.commit_counter.increment();
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &mut self,
        size: Size<f64, Logical>,
        geometry: Rectangle<f64, Logical>,
        color: Color,
        sigma: f32,
        corner_radius: CornerRadius,
        scale: f32,
        window_geometry: Rectangle<f64, Logical>,
        window_corner_radius: CornerRadius,
        alpha: f32,
    ) {
        let params = Parameters {
            size,
            geometry,
            color,
            sigma,
            alpha,
            corner_radius,
            scale,
            window_geometry,
            window_corner_radius,
        };
        // Only bump the commit counter when something actually changed: this is called every frame,
        // so incrementing unconditionally would report damage over the whole element every frame.
        if self.params == params {
            return;
        }

        self.params = params;
        self.commit_counter.increment();
    }

    /// Moves the element. Deliberately does not bump the commit counter: the owners re-place a
    /// cloned element every frame, and treating a move as damage here would defeat damage tracking.
    pub fn with_location(mut self, location: Point<f64, Logical>) -> Self {
        self.location = location;
        self
    }

    /// Fade the shadow. `params.alpha` is both what the draw reads and what `Element::alpha`
    /// reports, so there is only one place left to write.
    pub fn with_alpha(mut self, alpha: f32) -> Self {
        self.params.alpha = alpha;
        self
    }
}

impl Default for ShadowRenderElement {
    fn default() -> Self {
        Self::empty()
    }
}

// `transform` and `damage_since` are deliberately not overridden — the element this was promoted
// from did not override them either, so both keep the `Element` defaults.
impl Element for ShadowRenderElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.commit_counter
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        Rectangle::new(self.location, self.params.size).to_physical_precise_round(scale)
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        Rectangle::from_size(Size::from((1., 1.)))
    }

    fn alpha(&self) -> f32 {
        self.params.alpha
    }

    fn kind(&self) -> Kind {
        Kind::Unspecified
    }
}

// The renderer draws the shadow procedurally in its own pipeline, reading `params`.
use crate::render_helpers::vulkan::{VulkanError, VulkanFrame, VulkanRenderer};

impl ShadowRenderElement {
    /// Build the shadow material's push constants from `params`, with the quad placed at `dst`
    /// showing the `src` sub-rect of the element. (`proj`/`target` are filled by
    /// `VulkanFrame::render_shadow`.)
    ///
    /// The shader gets `v_uv` running 0..1 across `dst` and turns it into element-local logical
    /// coordinates with `v_uv * area_size - geo_loc`. A `CropRenderElement` shrinks `dst` to the
    /// visible sub-rect and reports the matching sub-rect of the unit `src`, so `src` has to be
    /// folded into that mapping — otherwise the whole gradient is squeezed into whatever survived
    /// the crop, which is what put a hard dark line down the overview workspace edge next to any
    /// window whose shadow reached it.
    fn vulkan_push(
        &self,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
    ) -> niri_vk::render::ShadowPush {
        let p = &self.params;
        // `v_uv * (size * src.size) - (loc - src.loc * size)` == `(src.loc + v_uv * src.size) *
        // size - loc`, so the crop folds into `area_size` and the two `*_geo_loc` origins with no
        // extra push constants and no shader change.
        let area = [p.size.w * src.size.w, p.size.h * src.size.h];
        let crop_off = [src.loc.x * p.size.w, src.loc.y * p.size.h];
        niri_vk::render::ShadowPush {
            origin: [dst.loc.x as f32, dst.loc.y as f32],
            size: [dst.size.w as f32, dst.size.h as f32],
            // proj/target are placeholders; VulkanFrame::render_shadow fills them from the frame.
            proj: niri_vk::render::IDENTITY_PROJ,
            target: [0.0, 0.0],
            sigma: p.sigma,
            niri_scale: p.scale,
            shadow_color: p.color.to_array_premul(),
            corner_radius: <[f32; 4]>::from(p.corner_radius),
            window_corner_radius: <[f32; 4]>::from(p.window_corner_radius),
            area_size: [area[0] as f32, area[1] as f32],
            geo_loc: [
                (p.geometry.loc.x - crop_off[0]) as f32,
                (p.geometry.loc.y - crop_off[1]) as f32,
            ],
            geo_size: [p.geometry.size.w as f32, p.geometry.size.h as f32],
            window_geo_loc: [
                (p.window_geometry.loc.x - crop_off[0]) as f32,
                (p.window_geometry.loc.y - crop_off[1]) as f32,
            ],
            window_geo_size: [
                p.window_geometry.size.w as f32,
                p.window_geometry.size.h as f32,
            ],
            niri_alpha: p.alpha,
            _pad0: 0.0,
        }
    }
}

impl RenderElement<VulkanRenderer> for ShadowRenderElement {
    fn draw(
        &self,
        frame: &mut VulkanFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), VulkanError> {
        frame.render_shadow(self.vulkan_push(src, dst), dst, damage)
    }

    fn underlying_storage(&self, _renderer: &mut VulkanRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn elem(alpha: f32) -> ShadowRenderElement {
        ShadowRenderElement::new(
            Size::from((100., 100.)),
            Rectangle::from_size(Size::from((100., 100.))),
            Color::new_unpremul(0., 0., 0., 1.),
            8.,
            CornerRadius::default(),
            1.,
            Rectangle::from_size(Size::from((80., 80.))),
            CornerRadius::default(),
            alpha,
        )
    }

    /// The owners call `update` every frame with mostly-unchanged parameters and re-place a clone
    /// via `with_location`. Both must leave the commit counter alone, or every frame reports damage
    /// over the whole element. Only a real parameter change may bump it.
    #[test]
    fn only_a_real_parameter_change_damages() {
        let mut e = elem(1.);
        let base = e.current_commit();

        let same = |e: &mut ShadowRenderElement, alpha: f32| {
            e.update(
                Size::from((100., 100.)),
                Rectangle::from_size(Size::from((100., 100.))),
                Color::new_unpremul(0., 0., 0., 1.),
                8.,
                CornerRadius::default(),
                1.,
                Rectangle::from_size(Size::from((80., 80.))),
                CornerRadius::default(),
                alpha,
            );
        };

        same(&mut e, 1.);
        assert_eq!(
            e.current_commit(),
            base,
            "an update with identical parameters must not damage"
        );

        let moved = e.clone().with_location(Point::from((7., 9.)));
        assert_eq!(
            moved.current_commit(),
            base,
            "with_location must not damage"
        );
        assert_eq!(
            moved.id(),
            e.id(),
            "a relocated clone must keep the damage identity"
        );

        same(&mut e, 0.5);
        assert_ne!(
            e.current_commit(),
            base,
            "a changed parameter must damage the element"
        );
    }

    /// The alpha the draw reads must be the one `with_alpha` sets and the one `Element` reports.
    /// This is the exact split that made the fade a silent no-op before the promotion.
    #[test]
    fn with_alpha_reaches_both_the_draw_and_the_element() {
        let e = elem(1.).with_alpha(0.25);
        assert_eq!(e.alpha(), 0.25, "Element::alpha must see the fade");
        let push = e.vulkan_push(
            Rectangle::from_size(Size::from((1., 1.))),
            Rectangle::from_size(smithay::utils::Size::from((100, 100))),
        );
        assert_eq!(push.niri_alpha, 0.25, "the draw must see the fade");
    }

    /// A `CropRenderElement` hands `draw` a sub-rect of the unit `src` together with the shrunken
    /// `dst`. The shader maps `v_uv` (0..1 over `dst`) through `area_size`/`geo_loc`, so the crop
    /// has to be folded into those — otherwise the whole shadow gradient is squeezed into the
    /// surviving strip, which is the hard dark line the overview showed along a workspace edge.
    #[test]
    fn a_cropped_draw_keeps_the_shadow_anchored() {
        let e = elem(1.);

        // Uncropped: the mapping is the identity on the element's own logical space.
        let full = e.vulkan_push(
            Rectangle::from_size(Size::from((1., 1.))),
            Rectangle::from_size(smithay::utils::Size::from((100, 100))),
        );
        assert_eq!(full.area_size, [100., 100.]);
        assert_eq!(full.geo_loc, [0., 0.]);

        // Crop away the left 40%: `v_uv = 0` is now element-local x = 40, so the geometry (and the
        // window cutout) must move 40 logical pixels to the left of the quad's origin, and one unit
        // of `v_uv` must still cover 60 logical pixels rather than the full 100.
        let cropped = e.vulkan_push(
            Rectangle::new(Point::from((0.4, 0.)), Size::from((0.6, 1.))),
            Rectangle::new(
                smithay::utils::Point::from((40, 0)),
                smithay::utils::Size::from((60, 100)),
            ),
        );
        assert_eq!(cropped.area_size, [60., 100.]);
        assert_eq!(cropped.geo_loc, [-40., 0.]);
        assert_eq!(cropped.window_geo_loc, [-40., 0.]);
        // The extents describe the window, not the visible strip, so the crop must not touch them.
        assert_eq!(cropped.geo_size, full.geo_size);
        assert_eq!(cropped.window_geo_size, full.window_geo_size);

        // The point that was element-local x = 70 must land at the same element-local coordinate
        // through both mappings: uncropped `v_uv = 0.7`, cropped `v_uv = 0.5`.
        let coords = |p: &niri_vk::render::ShadowPush, uv: f32| uv * p.area_size[0] - p.geo_loc[0];
        assert_eq!(coords(&full, 0.7), coords(&cropped, 0.5));
    }
}
