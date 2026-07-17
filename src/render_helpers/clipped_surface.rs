use niri_config::CornerRadius;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform};

use super::damage::ExtraDamage;

#[derive(Debug)]
pub struct ClippedSurfaceRenderElement {
    inner: WaylandSurfaceRenderElement<VulkanRenderer>,
    corner_radius: CornerRadius,
    geometry: Rectangle<f64, Logical>,
    scale: f32,
}

#[derive(Debug, Default, Clone)]
pub struct RoundedCornerDamage {
    damage: ExtraDamage,
    corner_radius: CornerRadius,
}

impl ClippedSurfaceRenderElement {
    /// Build a clipped-surface element. The renderer clips in its own pipeline
    /// (`clipped_texture.frag`); the `RenderElement` draw folds the geometry and radius into that
    /// pipeline's push constants.
    pub fn new(
        elem: WaylandSurfaceRenderElement<VulkanRenderer>,
        scale: Scale<f64>,
        geometry: Rectangle<f64, Logical>,
        corner_radius: CornerRadius,
    ) -> Self {
        Self {
            inner: elem,
            corner_radius,
            geometry,
            scale: scale.x as f32,
        }
    }

    pub fn will_clip(
        elem: &WaylandSurfaceRenderElement<VulkanRenderer>,
        scale: Scale<f64>,
        geometry: Rectangle<f64, Logical>,
        corner_radius: CornerRadius,
    ) -> bool {
        let elem_geo = elem.geometry(scale);
        let geo = geometry.to_physical_precise_round(scale);

        if corner_radius == CornerRadius::default() {
            !geo.contains_rect(elem_geo)
        } else {
            let corners = Self::rounded_corners(geometry, corner_radius);
            let corners = corners
                .into_iter()
                .map(|rect| rect.to_physical_precise_up(scale));
            let geo = Rectangle::subtract_rects_many([geo], corners);
            !Rectangle::subtract_rects_many([elem_geo], geo).is_empty()
        }
    }

    fn rounded_corners(
        geo: Rectangle<f64, Logical>,
        corner_radius: CornerRadius,
    ) -> [Rectangle<f64, Logical>; 4] {
        let top_left = corner_radius.top_left as f64;
        let top_right = corner_radius.top_right as f64;
        let bottom_right = corner_radius.bottom_right as f64;
        let bottom_left = corner_radius.bottom_left as f64;

        [
            Rectangle::new(geo.loc, Size::from((top_left, top_left))),
            Rectangle::new(
                Point::from((geo.loc.x + geo.size.w - top_right, geo.loc.y)),
                Size::from((top_right, top_right)),
            ),
            Rectangle::new(
                Point::from((
                    geo.loc.x + geo.size.w - bottom_right,
                    geo.loc.y + geo.size.h - bottom_right,
                )),
                Size::from((bottom_right, bottom_right)),
            ),
            Rectangle::new(
                Point::from((geo.loc.x, geo.loc.y + geo.size.h - bottom_left)),
                Size::from((bottom_left, bottom_left)),
            ),
        ]
    }
}

impl Element for ClippedSurfaceRenderElement {
    fn id(&self) -> &Id {
        self.inner.id()
    }

    fn current_commit(&self) -> CommitCounter {
        self.inner.current_commit()
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.inner.geometry(scale)
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        self.inner.src()
    }

    fn transform(&self) -> Transform {
        self.inner.transform()
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        // FIXME: radius changes need to cause damage.
        let damage = self.inner.damage_since(scale, commit);

        // Intersect with geometry, since we're clipping by it.
        let mut geo = self.geometry.to_physical_precise_round(scale);
        geo.loc -= self.geometry(scale).loc;
        damage
            .into_iter()
            .filter_map(|rect| rect.intersection(geo))
            .collect()
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        let regions = self.inner.opaque_regions(scale);

        // Intersect with geometry, since we're clipping by it.
        let mut geo = self.geometry.to_physical_precise_round(scale);
        geo.loc -= self.geometry(scale).loc;
        let regions = regions
            .into_iter()
            .filter_map(|rect| rect.intersection(geo));

        // Subtract the rounded corners.
        if self.corner_radius == CornerRadius::default() {
            regions.collect()
        } else {
            let corners = Self::rounded_corners(self.geometry, self.corner_radius);

            let elem_loc = self.geometry(scale).loc;
            let corners = corners.into_iter().map(|rect| {
                let mut rect = rect.to_physical_precise_up(scale);
                rect.loc -= elem_loc;
                rect
            });

            OpaqueRegions::from_slice(&Rectangle::subtract_rects_many(regions, corners))
        }
    }

    fn alpha(&self) -> f32 {
        self.inner.alpha()
    }

    fn kind(&self) -> Kind {
        self.inner.kind()
    }
}

// The renderer clips the surface in its own `clipped_texture` pipeline: the draw arms the clip on
// the frame, then draws the inner `WaylandSurfaceRenderElement`, whose sampling
// (`render_texture_from_to`) picks up the clip and swaps to the clipped pipeline.
use crate::render_helpers::vulkan::{ClipParams, VulkanError, VulkanFrame, VulkanRenderer};

impl RenderElement<VulkanRenderer> for ClippedSurfaceRenderElement {
    fn draw(
        &self,
        frame: &mut VulkanFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), VulkanError> {
        let scale = Scale::from(f64::from(self.scale));
        // Build `input_to_geo` from *creation-space* quantities, never the draw-time `dst`: the
        // element can be wrapped in a `RescaleRenderElement` / `RelocateRenderElement` (overview
        // zoom, MRU thumbnails, snapshot/offscreen renders, region screencasts) that transforms
        // `dst` but not `self.inner`/`self.geometry`. `v_uv` (0..1 across the quad) spans the
        // element's content, so mapping it to `[0, 1]` geometry space via `elem_geo → geo` (both
        // creation-space physical) is invariant under those wrappers — exactly why GLES's
        // `compute_uniforms` derives its `input_to_geo` from `self.inner.geometry(scale)`, not
        // `dst`. The buffer transform / y-invert stay in the sampling `tex_transform`, so
        // this geometric mapping needs only a scale + translate (no rotation/flip terms).
        let elem_geo: Rectangle<i32, Physical> = self.inner.geometry(scale);
        let geo: Rectangle<i32, Physical> = self.geometry.to_physical_precise_round(scale);
        let (gw, gh) = (geo.size.w as f32, geo.size.h as f32);
        // Degenerate (zero-size) geometry would divide by zero; nothing is inside it, so skip the
        // whole element (leaving the clip disarmed).
        if gw <= 0. || gh <= 0. {
            return Ok(());
        }
        // coords_geo = (elem_geo.loc + v_uv * elem_geo.size - geo.loc) / geo.size, packed as 3
        // `vec4` columns (`.xyz` used) for the shader's `mat3(i2g...) * vec3(v_uv, 1)`.
        let input_to_geo = [
            [elem_geo.size.w as f32 / gw, 0., 0., 0.],
            [0., elem_geo.size.h as f32 / gh, 0., 0.],
            [
                (elem_geo.loc.x - geo.loc.x) as f32 / gw,
                (elem_geo.loc.y - geo.loc.y) as f32 / gh,
                1.,
                0.,
            ],
        ];
        let clip = ClipParams {
            input_to_geo,
            // Logical geometry size = the rounding coordinate space (matches `compute_uniforms`).
            geo_size: [self.geometry.size.w as f32, self.geometry.size.h as f32],
            corner_radius: <[f32; 4]>::from(self.corner_radius),
            niri_scale: self.scale,
        };
        // Arm the clip, draw the inner surface, then disarm unconditionally (even on error) so a
        // later unclipped surface on the same frame isn't clipped by a stale override.
        frame.set_clip_override(Some(clip));
        let res = RenderElement::<VulkanRenderer>::draw(
            &self.inner,
            frame,
            src,
            dst,
            damage,
            opaque_regions,
            cache,
        );
        frame.set_clip_override(None);
        res
    }

    fn underlying_storage(&self, _renderer: &mut VulkanRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}

impl RoundedCornerDamage {
    pub fn set_corner_radius(&mut self, corner_radius: CornerRadius) {
        if self.corner_radius == corner_radius {
            return;
        }

        // FIXME: make the damage granular.
        self.corner_radius = corner_radius;
        self.damage.damage_all();
    }

    pub fn render(&self, geometry: Rectangle<f64, Logical>) -> ExtraDamage {
        self.damage.render(geometry)
    }
}
