// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The magnifier's view drawn through its colour effects (`mag_effects.frag`): an offscreen of
//! the zoom region, sampled once through desaturate, brightness/contrast and invert lightness.

use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Physical, Rectangle, Scale, Transform};

use super::offscreen::OffscreenRenderElement;
use super::vulkan::{VulkanError, VulkanFrame, VulkanRenderer};

/// `MagShaderEffects`' parameters as the shader takes them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MagEffects {
    pub desaturation: f32,
    pub invert: bool,
    pub scale: [f32; 3],
    pub bias: [f32; 3],
}

#[derive(Debug)]
pub struct MagEffectsRenderElement {
    inner: OffscreenRenderElement,
    effects: MagEffects,
}

impl MagEffectsRenderElement {
    pub fn new(inner: OffscreenRenderElement, effects: MagEffects) -> Self {
        Self { inner, effects }
    }
}

impl Element for MagEffectsRenderElement {
    fn id(&self) -> &Id {
        self.inner.id()
    }

    fn current_commit(&self) -> CommitCounter {
        self.inner.current_commit()
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.inner.geometry(scale)
    }

    fn transform(&self) -> Transform {
        Transform::Normal
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        self.inner.src()
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        self.inner.damage_since(scale, commit)
    }

    fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        OpaqueRegions::default()
    }

    fn alpha(&self) -> f32 {
        self.inner.alpha()
    }

    fn kind(&self) -> Kind {
        Kind::Unspecified
    }

    fn draw_key(&self) -> u64 {
        // The effects rewrite every pixel and are none of the instance key: a changed setting must
        // repaint the view as a move would.
        let e = &self.effects;
        let mut key = 0xcbf2_9ce4_8422_2325_u64;
        for bits in [e.desaturation, f32::from(u8::from(e.invert))]
            .into_iter()
            .chain(e.scale)
            .chain(e.bias)
            .map(f32::to_bits)
        {
            key = (key ^ u64::from(bits)).wrapping_mul(0x0000_0100_0000_01b3);
        }
        key
    }
}

impl RenderElement<VulkanRenderer> for MagEffectsRenderElement {
    fn draw(
        &self,
        frame: &mut VulkanFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), VulkanError> {
        let e = &self.effects;
        frame.render_mag_effects(
            self.inner.texture(),
            src,
            dst,
            damage,
            self.inner.alpha(),
            e.desaturation,
            e.invert,
            e.scale,
            e.bias,
        )
    }

    fn underlying_storage(&self, _renderer: &mut VulkanRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}
