use std::cell::RefCell;

use anyhow::Context as _;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::utils::{Relocate, RelocateRenderElement};
use smithay::backend::renderer::element::{
    Element, Id, Kind, RenderElement, RenderElementStates, UnderlyingStorage,
};
use smithay::backend::renderer::gles::{GlesError, GlesFrame, GlesRenderer, GlesTexture};
use smithay::backend::renderer::sync::SyncPoint;
use smithay::backend::renderer::utils::{
    CommitCounter, DamageBag, DamageSet, DamageSnapshot, OpaqueRegions,
};
use smithay::backend::renderer::{
    Bind, Color32F, ContextId, Frame as _, Offscreen, Renderer, Texture,
};
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform};

use super::encompassing_geo;
use super::renderer::OffscreenRenderer;
use crate::render_helpers::vulkan::{VkTexture, VulkanError, VulkanFrame, VulkanRenderer};

/// Buffer for offscreen rendering.
///
/// Generic over the offscreen texture type `T` (the renderer's `TextureId`), defaulting to
/// `GlesTexture` so bare mentions across the render tree resolve unchanged; the owned Vulkan
/// renderer specializes it to `VkTexture`. (`ContextId<T>` requires `T: Texture`.)
#[derive(Debug)]
pub struct OffscreenBuffer<T: Texture = GlesTexture> {
    id: Id,

    /// The cached texture buffer.
    ///
    /// Lazily created when `render` is called. Recreated when necessary.
    inner: RefCell<Option<Inner<T>>>,
}

#[derive(Debug)]
struct Inner<T: Texture> {
    /// The texture with offscreened contents.
    texture: T,
    /// Id of the renderer context that the texture comes from.
    renderer_context_id: ContextId<T>,
    /// Scale of the texture.
    scale: Scale<f64>,
    /// Damage tracker for drawing to the texture.
    damage: OutputDamageTracker,
    /// Damage of this offscreen element itself facing outside.
    outer_damage: DamageBag<i32, Buffer>,
}

#[derive(Debug, Clone)]
pub struct OffscreenRenderElement<T: Texture = GlesTexture> {
    id: Id,
    texture: T,
    renderer_context_id: ContextId<T>,
    scale: Scale<f64>,
    damage: DamageSnapshot<i32, Buffer>,
    offset: Point<f64, Logical>,
    src_size: Size<i32, Buffer>,
    alpha: f32,
    kind: Kind,
}

#[derive(Debug)]
pub struct OffscreenData {
    /// Id of the offscreen element.
    pub id: Id,
    /// States for the render into the offscreen buffer.
    pub states: RenderElementStates,
}

impl<T: Texture + Clone + 'static> OffscreenBuffer<T> {
    pub fn render<R>(
        &self,
        renderer: &mut R,
        scale: Scale<f64>,
        elements: &[impl RenderElement<R>],
    ) -> anyhow::Result<(OffscreenRenderElement<T>, SyncPoint, OffscreenData)>
    where
        R: OffscreenRenderer + Renderer<TextureId = T> + Offscreen<T> + Bind<T>,
        R::Error: std::error::Error + Send + Sync + 'static,
    {
        let _span = tracy_client::span!("OffscreenBuffer::render");

        let geo = encompassing_geo(scale, elements.iter());
        let elements = Vec::from_iter(elements.iter().map(|ele| {
            RelocateRenderElement::from_element(ele, geo.loc.upscale(-1), Relocate::Relative)
        }));

        // Guard against empty elements producing a zero size.
        let mut src_size = geo.size;
        if src_size.w == 0 || src_size.h == 0 {
            src_size = Size::new(1, 1);
        }

        let src_size = src_size.to_logical(1).to_buffer(1, Transform::Normal);
        let offset = geo.loc.to_f64().to_logical(scale);

        let mut inner = self.inner.borrow_mut();

        // Check if we need to create or recreate the texture.
        let size_string;
        let mut reason = "";
        if let Some(Inner {
            texture,
            renderer_context_id,
            ..
        }) = inner.as_mut()
        {
            let old_size = texture.size();
            if old_size.w < src_size.w || old_size.h < src_size.h {
                size_string = format!(
                    "size increased from {} × {} to {} × {}",
                    old_size.w, old_size.h, src_size.w, src_size.h
                );
                reason = &size_string;

                *inner = None;
            } else if !renderer.offscreen_is_reusable(texture) {
                reason = "not unique";

                *inner = None;
            } else if *renderer_context_id != renderer.context_id() {
                reason = "renderer id changed";

                *inner = None;
            }
        } else {
            reason = "first render";
        }

        let inner = if let Some(inner) = inner.as_mut() {
            inner
        } else {
            trace!("creating new texture: {reason}");
            let span = tracy_client::span!("creating offscreen buffer");
            span.emit_text(reason);

            let texture: T = renderer
                .create_buffer(Fourcc::Abgr8888, src_size)
                .context("error creating texture")?;

            let buffer_size = src_size.to_logical(1, Transform::Normal).to_physical(1);
            let damage = OutputDamageTracker::new(buffer_size, scale, Transform::Normal);

            inner.insert(Inner {
                texture,
                renderer_context_id: renderer.context_id(),
                scale,
                damage,
                outer_damage: DamageBag::default(),
            })
        };

        // When leaving the old texture as is, its size might be bigger than src_size.
        let texture_size = inner.texture.size();
        let buffer_size = texture_size.to_logical(1, Transform::Normal).to_physical(1);

        // Recreate the damage tracker if the scale changes. We already recreate it for buffer size
        // changes, and transform is always Normal.
        if inner.scale != scale {
            inner.scale = scale;

            trace!("recreating damage tracker due to scale change");
            inner.damage = OutputDamageTracker::new(buffer_size, scale, Transform::Normal);
            inner.outer_damage = DamageBag::default();
        }

        let res = {
            let mut target = renderer.bind(&mut inner.texture)?;
            inner
                .damage
                .render_output(renderer, &mut target, 1, &elements, Color32F::TRANSPARENT)
                .context("error rendering")?
        };

        // Make the just-rendered offscreen sampleable by the returned element's draw (a no-op on
        // GLES; the owned Vulkan renderer inserts the layout barrier its images need).
        renderer
            .make_offscreen_sampleable(&inner.texture)
            .context("error preparing offscreen for sampling")?;

        // Add the resulting damage to the outer tracker.
        if let Some(damage) = res.damage {
            // OutputDamageTracker gives us Physical coordinate space, but it's actually the Buffer
            // space because we were rendering to a texture.
            let size = buffer_size.to_logical(1);
            let damage = damage
                .iter()
                .map(|rect| rect.to_logical(1).to_buffer(1, Transform::Normal, &size));
            inner.outer_damage.add(damage);
        }

        let elem = OffscreenRenderElement {
            id: self.id.clone(),
            texture: inner.texture.clone(),
            renderer_context_id: inner.renderer_context_id.clone(),
            scale,
            damage: inner.outer_damage.snapshot(),
            offset,
            src_size,
            alpha: 1.,
            kind: Kind::Unspecified,
        };

        let data = OffscreenData {
            id: self.id.clone(),
            states: res.states,
        };

        Ok((elem, res.sync, data))
    }
}

impl<T: Texture> Default for OffscreenBuffer<T> {
    fn default() -> Self {
        OffscreenBuffer {
            inner: RefCell::new(None),
            id: Id::new(),
        }
    }
}

impl<T: Texture> OffscreenRenderElement<T> {
    pub fn texture(&self) -> &T {
        &self.texture
    }

    pub fn offset(&self) -> Point<f64, Logical> {
        self.offset
    }

    pub fn with_alpha(mut self, alpha: f32) -> Self {
        self.alpha = alpha;
        self
    }

    pub fn with_offset(mut self, offset: Point<f64, Logical>) -> Self {
        self.offset = offset;
        self
    }

    pub fn logical_size(&self) -> Size<f64, Logical> {
        self.src_size
            .to_f64()
            .to_logical(self.scale, Transform::Normal)
    }

    fn damage_since(&self, commit: Option<CommitCounter>) -> DamageSet<i32, Buffer> {
        self.damage
            .damage_since(commit)
            .unwrap_or_else(|| DamageSet::from_slice(&[Rectangle::from_size(self.texture.size())]))
    }
}

impl<T: Texture> Element for OffscreenRenderElement<T> {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.damage.current_commit()
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        let logical_geo = Rectangle::new(self.offset, self.logical_size());
        logical_geo.to_physical_precise_round(scale)
    }

    fn transform(&self) -> Transform {
        Transform::Normal
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        Rectangle::from_size(self.src_size).to_f64()
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        let texture_size = self.texture.size().to_f64();
        let src = self.src();

        self.damage_since(commit)
            .into_iter()
            .filter_map(|region| {
                let mut region = region.to_f64().intersection(src)?;

                region.loc -= src.loc;
                region = region.upscale(texture_size / src.size);

                let logical = region.to_logical(self.scale, Transform::Normal, &src.size);
                Some(logical.to_physical_precise_up(scale))
            })
            .collect()
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

impl RenderElement<GlesRenderer> for OffscreenRenderElement<GlesTexture> {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dest: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        if frame.context_id() != self.renderer_context_id {
            warn!("trying to render texture from different renderer");
            return Ok(());
        }

        frame.render_texture_from_to(
            &self.texture,
            src,
            dest,
            damage,
            opaque_regions,
            Transform::Normal,
            self.alpha,
            None,
            &[],
        )
    }

    fn underlying_storage(&self, _renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        // If scanout for things other than Wayland buffers is implemented, this will need to take
        // the target GPU into account.
        None
    }
}

// The `VkTexture` specialization samples the offscreen through the owned Vulkan renderer (the
// sampleable-offscreen bridge); the render tree's `<GlesTexture>` variant stays a degraded no-op.
impl RenderElement<VulkanRenderer> for OffscreenRenderElement<VkTexture> {
    fn draw(
        &self,
        frame: &mut VulkanFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), VulkanError> {
        if frame.context_id() != self.renderer_context_id {
            warn!("trying to render texture from different renderer");
            return Ok(());
        }

        frame.render_texture_from_to(
            &self.texture,
            src,
            dst,
            damage,
            opaque_regions,
            Transform::Normal,
            self.alpha,
        )
    }

    fn underlying_storage(&self, _renderer: &mut VulkanRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}

/// A concrete enum carrying either the GLES-sampled offscreen element or the `VkTexture` one
/// sampled through the owned Vulkan renderer — the same type-unification bridge as
/// [`DualTextureRenderElement`](super::dual_texture::DualTextureRenderElement), for callers that
/// push an offscreen element into a generic `<R>` render tree (the alt-tab MRU closing fade).
/// `OffscreenRenderElement<VkTexture>` has no `RenderElement<GlesRenderer>` impl, so it can't ride
/// a generic arm directly; this enum dispatches per renderer, no-op on the wrong one. The type is
/// present with or without the `vulkan` feature (only the `Vulkan` variant is gated) so the render
/// tree's arm stays unconditional, matching the macro's inability to cfg individual arms.
#[derive(Debug)]
pub enum DualOffscreenRenderElement {
    Gles(OffscreenRenderElement<GlesTexture>),
    Vulkan(OffscreenRenderElement<VkTexture>),
}

impl DualOffscreenRenderElement {
    fn inner(&self) -> &dyn Element {
        match self {
            DualOffscreenRenderElement::Gles(e) => e,
            DualOffscreenRenderElement::Vulkan(e) => e,
        }
    }
}

impl Element for DualOffscreenRenderElement {
    fn id(&self) -> &Id {
        self.inner().id()
    }

    fn current_commit(&self) -> CommitCounter {
        self.inner().current_commit()
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.inner().geometry(scale)
    }

    fn transform(&self) -> Transform {
        self.inner().transform()
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        self.inner().src()
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        self.inner().damage_since(scale, commit)
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        self.inner().opaque_regions(scale)
    }

    fn alpha(&self) -> f32 {
        self.inner().alpha()
    }

    fn kind(&self) -> Kind {
        self.inner().kind()
    }
}

impl RenderElement<GlesRenderer> for DualOffscreenRenderElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        match self {
            DualOffscreenRenderElement::Gles(e) => RenderElement::<GlesRenderer>::draw(
                e,
                frame,
                src,
                dst,
                damage,
                opaque_regions,
                cache,
            ),
            // The Vulkan arm is only ever constructed on a Vulkan session, never here.
            DualOffscreenRenderElement::Vulkan(_) => {
                debug_assert!(
                    false,
                    "Vulkan DualOffscreenRenderElement drawn through GLES"
                );
                Ok(())
            }
        }
    }

    fn underlying_storage(&self, renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        match self {
            DualOffscreenRenderElement::Gles(e) => e.underlying_storage(renderer),
            DualOffscreenRenderElement::Vulkan(_) => None,
        }
    }
}

impl RenderElement<VulkanRenderer> for DualOffscreenRenderElement {
    fn draw(
        &self,
        frame: &mut VulkanFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), VulkanError> {
        // Both arms dispatch to the inner element: the GLES variant's
        // `RenderElement<VulkanRenderer>` is a degraded no-op (reached only if the Vulkan
        // offscreen render failed and we kept it), the Vulkan variant draws for real.
        match self {
            DualOffscreenRenderElement::Gles(e) => RenderElement::<VulkanRenderer>::draw(
                e,
                frame,
                src,
                dst,
                damage,
                opaque_regions,
                cache,
            ),
            DualOffscreenRenderElement::Vulkan(e) => RenderElement::<VulkanRenderer>::draw(
                e,
                frame,
                src,
                dst,
                damage,
                opaque_regions,
                cache,
            ),
        }
    }

    fn underlying_storage(&self, _renderer: &mut VulkanRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}
