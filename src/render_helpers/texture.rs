// SPDX-License-Identifier: GPL-3.0-only
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::utils::{CommitCounter, OpaqueRegions};
use smithay::backend::renderer::{ContextId, Frame as _, ImportMem, Renderer, Texture};
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform};

use super::memory::MemoryBuffer;

/// Smithay's texture buffer, but with fractional scale.
#[derive(Debug, Clone)]
pub struct TextureBuffer<T: Texture> {
    id: Id,
    commit_counter: CommitCounter,
    renderer_context_id: ContextId<T>,
    texture: T,
    scale: Scale<f64>,
    transform: Transform,
    opaque_regions: Vec<Rectangle<i32, Buffer>>,
}

/// Render element for a [`TextureBuffer`].
#[derive(Debug, Clone)]
pub struct TextureRenderElement<T: Texture> {
    buffer: TextureBuffer<T>,
    location: Point<f64, Logical>,
    alpha: f32,
    src: Option<Rectangle<f64, Logical>>,
    size: Option<Size<f64, Logical>>,
    kind: Kind,
}

impl<T: Texture> TextureBuffer<T> {
    pub fn from_texture<R: Renderer<TextureId = T>>(
        renderer: &R,
        texture: T,
        scale: impl Into<Scale<f64>>,
        transform: Transform,
        opaque_regions: Vec<Rectangle<i32, Buffer>>,
    ) -> Self {
        TextureBuffer {
            id: Id::new(),
            commit_counter: CommitCounter::default(),
            renderer_context_id: renderer.context_id(),
            texture,
            scale: scale.into(),
            transform,
            opaque_regions,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn from_memory<R: Renderer<TextureId = T> + ImportMem>(
        renderer: &mut R,
        data: &[u8],
        format: Fourcc,
        size: impl Into<Size<i32, Buffer>>,
        flipped: bool,
        scale: impl Into<Scale<f64>>,
        transform: Transform,
        opaque_regions: Vec<Rectangle<i32, Buffer>>,
    ) -> Result<Self, R::Error> {
        let texture = renderer.import_memory(data, format, size.into(), flipped)?;
        Ok(TextureBuffer::from_texture(
            renderer,
            texture,
            scale,
            transform,
            opaque_regions,
        ))
    }

    pub fn from_memory_buffer<R: Renderer<TextureId = T> + ImportMem>(
        renderer: &mut R,
        buffer: &MemoryBuffer,
    ) -> Result<Self, R::Error> {
        Self::from_memory(
            renderer,
            buffer.data(),
            buffer.format(),
            buffer.size(),
            false,
            buffer.scale(),
            buffer.transform(),
            Vec::new(),
        )
    }

    pub fn texture(&self) -> &T {
        &self.texture
    }

    pub fn texture_scale(&self) -> Scale<f64> {
        self.scale
    }

    pub fn set_texture_scale(&mut self, scale: impl Into<Scale<f64>>) {
        self.scale = scale.into();
    }

    pub fn texture_transform(&self) -> Transform {
        self.transform
    }

    pub fn set_texture_transform(&mut self, transform: Transform) {
        self.transform = transform;
    }
}

impl<T: Texture> TextureBuffer<T> {
    pub fn logical_size(&self) -> Size<f64, Logical> {
        self.texture
            .size()
            .to_f64()
            .to_logical(self.scale, self.transform)
    }
}

impl<T: Texture> TextureRenderElement<T> {
    pub fn from_texture_buffer(
        buffer: TextureBuffer<T>,
        location: impl Into<Point<f64, Logical>>,
        alpha: f32,
        src: Option<Rectangle<f64, Logical>>,
        size: Option<Size<f64, Logical>>,
        kind: Kind,
    ) -> Self {
        TextureRenderElement {
            buffer,
            location: location.into(),
            alpha,
            src,
            size,
            kind,
        }
    }

    pub fn buffer(&self) -> &TextureBuffer<T> {
        &self.buffer
    }
}

impl<T: Texture> TextureRenderElement<T> {
    pub fn logical_size(&self) -> Size<f64, Logical> {
        self.size
            .or_else(|| self.src.map(|src| src.size))
            .unwrap_or_else(|| self.buffer.logical_size())
    }

    /// Overall opacity multiplier for this element. Used to fade a composited overlay
    /// (e.g. the panel popover) by an animation progress value after the element is built.
    pub fn set_alpha(&mut self, alpha: f32) {
        self.alpha = alpha;
    }

    /// This element's top-left in output-local logical coords.
    pub fn location(&self) -> Point<f64, Logical> {
        self.location
    }

    pub fn set_location(&mut self, location: Point<f64, Logical>) {
        self.location = location;
    }

    /// This element narrowed to `clip` (output-local logical), or `None` when it falls entirely
    /// outside. The visible sub-rect is expressed as a narrower `src` over the same buffer, so a
    /// clipped element still costs one quad — the alternative is `CropRenderElement`, which would
    /// change the element type of every caller that builds a plain `Vec<TextureRenderElement>`.
    ///
    /// The `src` this narrows is in the *buffer's* logical space, which is only the same space as
    /// `clip` when the element is drawn at its natural size; the ratio below is what carries a
    /// scaled element across.
    pub fn clipped(mut self, clip: Rectangle<f64, Logical>) -> Option<Self> {
        let dst = Rectangle::new(self.location, self.logical_size());
        let visible = dst.intersection(clip)?;
        if visible == dst {
            return Some(self);
        }
        let src = self
            .src
            .unwrap_or_else(|| Rectangle::from_size(self.buffer.logical_size()));
        // Zero-sized in either axis: there is no ratio to map through, and nothing to draw.
        if dst.size.w <= 0. || dst.size.h <= 0. {
            return None;
        }
        let (rx, ry) = (src.size.w / dst.size.w, src.size.h / dst.size.h);
        self.src = Some(Rectangle::new(
            Point::from((
                src.loc.x + (visible.loc.x - dst.loc.x) * rx,
                src.loc.y + (visible.loc.y - dst.loc.y) * ry,
            )),
            Size::from((visible.size.w * rx, visible.size.h * ry)),
        ));
        self.size = Some(visible.size);
        self.location = visible.loc;
        Some(self)
    }

    /// Override the element's logical size, scaling the texture to fit. Paired with
    /// `set_location`, this scales an already-built overlay element about a pivot (the
    /// panel popover's open/close scale). Only sound to use while the element is also
    /// translucent: a `size` override isn't reflected in [`Self::opaque_regions`], but a
    /// translucent element already reports no opaque regions, so nothing reads the stale
    /// value. (`logical_size` prefers this override.)
    pub fn set_size(&mut self, size: Size<f64, Logical>) {
        self.size = Some(size);
    }

    /// This element cut down to the part of it inside `clip`, or `None` when it falls
    /// outside entirely. The visible part keeps its position and its scale: only the
    /// source rectangle narrows, so this is a true clip and not a squeeze.
    ///
    /// This is what a container that scrolls or paginates its children needs — a widget
    /// whose content travels a whole viewport width relies on *something* cutting it off,
    /// and only a full-screen surface gets that for free from the output edge.
    pub fn cropped(mut self, clip: Rectangle<f64, Logical>) -> Option<Self> {
        let geo = Rectangle::new(self.location, self.logical_size());
        let visible = geo.intersection(clip)?;
        if visible.size.w <= 0. || visible.size.h <= 0. {
            return None;
        }
        if visible == geo {
            return Some(self);
        }
        // The source travels with the destination: an element already cropped or resized
        // samples its buffer at some ratio, and the same ratio applies to the cut. Read
        // from the field, not `logical_src`, whose fallback is the *size override*.
        let src = self
            .src
            .unwrap_or_else(|| Rectangle::from_size(self.buffer.logical_size()));
        let ratio = (
            src.size.w / geo.size.w.max(f64::EPSILON),
            src.size.h / geo.size.h.max(f64::EPSILON),
        );
        let offset = visible.loc - geo.loc;
        let src = Rectangle::new(
            Point::from((
                src.loc.x + offset.x * ratio.0,
                src.loc.y + offset.y * ratio.1,
            )),
            Size::from((visible.size.w * ratio.0, visible.size.h * ratio.1)),
        );
        self.location = visible.loc;
        self.src = Some(src);
        self.size = Some(visible.size);
        Some(self)
    }

    pub fn logical_src(&self) -> Rectangle<f64, Logical> {
        self.src
            .unwrap_or_else(|| Rectangle::from_size(self.logical_size()))
    }
}

impl<T: Texture> Element for TextureRenderElement<T> {
    fn id(&self) -> &Id {
        &self.buffer.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.buffer.commit_counter
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        let logical_geo = Rectangle::new(self.location, self.logical_size());
        logical_geo.to_physical_precise_round(scale)
    }

    fn transform(&self) -> Transform {
        self.buffer.transform
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        self.src
            .map(|src| {
                src.to_buffer(
                    self.buffer.scale,
                    self.buffer.transform,
                    &self.buffer.logical_size(),
                )
            })
            .unwrap_or_else(|| Rectangle::from_size(self.buffer.texture.size()).to_f64())
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        // A translucent element occludes nothing: reporting opaque regions while `alpha < 1`
        // would let the damage tracker skip clearing/repainting the scene beneath, so the fade
        // blends over stale framebuffer content (the panel-popover close-fade bug). Matches
        // smithay's own texture element and the sibling gate in `rounded_texture.rs`.
        if self.alpha < 1.0 {
            return OpaqueRegions::default();
        }
        let texture_size = self.buffer.texture.size().to_f64();
        let src = self.src();

        self.buffer
            .opaque_regions
            .iter()
            .filter_map(|region| {
                let mut region = region.to_f64().intersection(src)?;

                region.loc -= src.loc;
                region = region.upscale(texture_size / src.size);

                let logical =
                    region.to_logical(self.buffer.scale, self.buffer.transform, &src.size);
                Some(logical.to_physical_precise_down(scale))
            })
            .collect()
    }

    fn alpha(&self) -> f32 {
        self.alpha
    }

    fn kind(&self) -> Kind {
        self.kind
    }
}

impl<R, T> RenderElement<R> for TextureRenderElement<T>
where
    R: Renderer<TextureId = T>,
    T: Texture,
{
    fn draw(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dest: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), R::Error> {
        if frame.context_id() != self.buffer.renderer_context_id {
            warn!("trying to render texture from different renderer");
            return Ok(());
        }

        frame.render_texture_from_to(
            &self.buffer.texture,
            src,
            dest,
            damage,
            opaque_regions,
            self.buffer.transform,
            self.alpha,
        )
    }

    fn underlying_storage(&self, _renderer: &mut R) -> Option<UnderlyingStorage<'_>> {
        None
    }
}
