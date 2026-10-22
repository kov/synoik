// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! A pie slice as a *render element*: GNOME's `PieTimer` (`js/ui/pointerA11yTimeout.js`), the
//! countdown pointer accessibility draws at the pointer. Its angle moves every frame, so it is
//! drawn straight into the frame rather than baked — the reasoning of
//! [`super::rounded_solid`].

use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::utils::{CommitCounter, OpaqueRegions};
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Logical, Physical, Rectangle, Scale, Size};

use super::vulkan::{VulkanError, VulkanFrame, VulkanRenderer};

#[derive(Debug, Clone)]
pub struct PieRenderElement {
    id: Id,
    commit: CommitCounter,
    geometry: Rectangle<f64, Logical>,
    /// Straight-alpha, as the toolkit states colours.
    fill: [f32; 4],
    border: [f32; 4],
    /// Logical px.
    border_width: f64,
    /// Radians swept from twelve o'clock, clockwise.
    angle: f64,
    scale: f64,
}

impl PieRenderElement {
    /// `commit` must change whenever anything else does; a pie that animates every frame passes
    /// its frame's sequence.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: Id,
        commit: CommitCounter,
        geometry: Rectangle<f64, Logical>,
        fill: [f32; 4],
        border: [f32; 4],
        border_width: f64,
        angle: f64,
        scale: Scale<f64>,
    ) -> Self {
        Self {
            id,
            commit,
            geometry,
            fill,
            border,
            border_width,
            angle,
            scale: scale.x,
        }
    }
}

impl Element for PieRenderElement {
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

    fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        // A disc in a square: the corners are always see-through.
        OpaqueRegions::default()
    }

    fn kind(&self) -> Kind {
        Kind::Unspecified
    }
}

impl RenderElement<VulkanRenderer> for PieRenderElement {
    fn draw(
        &self,
        frame: &mut VulkanFrame<'_, '_>,
        _src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), VulkanError> {
        // As `RoundedSolidRenderElement::draw`: an outer rescale moves `dst` without our scale.
        let geo_w = self.geometry(Scale::from(self.scale)).size.w;
        let rescale = if geo_w != 0 {
            dst.size.w as f64 / f64::from(geo_w)
        } else {
            1.
        };
        let border_px = (self.border_width * self.scale * rescale) as f32;
        frame.render_pie(
            self.fill,
            self.border,
            border_px,
            self.angle as f32,
            dst,
            damage,
        )
    }

    fn underlying_storage(&self, _renderer: &mut VulkanRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}
