// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The custom window **open**/**close** animation element: a window snapshot `VkTexture` plus a
//! prebuilt `CustomAnimPush`, drawn through
//! [`VulkanFrame::render_custom_anim`](super::vulkan::VulkanFrame::render_custom_anim) for the
//! user's `open`/`close` shader.

use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Logical, Physical, Rectangle, Scale, Transform};

use crate::render_helpers::vulkan::{
    CustomAnimPush, CustomShaderType, VkTexture, VulkanError, VulkanFrame, VulkanRenderer,
};

/// One window snapshot + a prebuilt `CustomAnimPush`, drawn via `VulkanFrame::render_custom_anim`
/// for the user's `open`/`close` shader (`ty`). The material fields of `push` are filled by the
/// animation; origin/size/target/proj are filled by `render_custom_anim`.
#[derive(Debug)]
pub struct CustomAnimRenderElement {
    id: Id,
    commit: CommitCounter,
    ty: CustomShaderType,
    area: Rectangle<f64, Logical>,
    alpha: f32,
    kind: Kind,
    texture: VkTexture,
    push: CustomAnimPush,
}

impl CustomAnimRenderElement {
    /// Build a custom `open`/`close` animation over `texture`, covering `area` (logical). `push`
    /// carries the material fields (`input_to_geo`/`geo_to_tex`/`geo_size`/`progress`/
    /// `random_seed`/`alpha`/`scale`); `ty` selects the shader slot.
    pub(crate) fn new_vulkan_anim(
        ty: CustomShaderType,
        texture: VkTexture,
        area: Rectangle<f64, Logical>,
        alpha: f32,
        push: CustomAnimPush,
    ) -> Self {
        Self {
            id: Id::new(),
            commit: CommitCounter::default(),
            ty,
            area,
            alpha,
            kind: Kind::Unspecified,
            texture,
            push,
        }
    }
}

impl Element for CustomAnimRenderElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.commit
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.area.to_physical_precise_round(scale)
    }

    fn transform(&self) -> Transform {
        Transform::Normal
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        Rectangle::from_size((1., 1.).into())
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        if commit != Some(self.commit) {
            DamageSet::from_slice(&[self.area.to_physical_precise_round(scale)])
        } else {
            DamageSet::default()
        }
    }

    fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        OpaqueRegions::default()
    }

    fn alpha(&self) -> f32 {
        self.alpha
    }

    fn kind(&self) -> Kind {
        self.kind
    }
}

impl RenderElement<VulkanRenderer> for CustomAnimRenderElement {
    fn draw(
        &self,
        frame: &mut VulkanFrame<'_, '_>,
        _src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), VulkanError> {
        frame.render_custom_anim(self.ty, &self.texture, dst, damage, self.push)
    }

    fn underlying_storage(&self, _renderer: &mut VulkanRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}
