// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! COLRv1 colour-glyph rasterization: skrifa walks the paint graph, tiny-skia draws it.
//!
//! Our text path rasterizes outlines into an alpha mask ([`crate::text`]), which is everything a
//! letter needs and nothing a colour emoji does: a COLRv1 glyph's base outline is *empty*, its ink
//! lives in a graph of transforms, clips, gradients and composites. swash reads the COLR **v0**
//! table only, and Fedora ships Noto Color Emoji as COLRv1 with no bitmap strikes, so there is no
//! shortcut through the existing scaler — this module is the colour half of the glyph pipeline.
//!
//! The output is premultiplied RGBA, placed like a [`crate::text`] mask: `left`/`top` are the
//! offset of the bitmap's top-left corner from the pen position on the baseline.

use skrifa::color::{
    Brush, ColorPainter, ColorStop, CompositeMode, Extend, Transform as ColrTransform,
};
use skrifa::instance::{LocationRef, Size};
use skrifa::outline::{DrawSettings, OutlinePen};
use skrifa::raw::types::BoundingBox;
use skrifa::raw::TableProvider;
use skrifa::{FontRef, GlyphId, MetadataProvider};
use tiny_skia::{
    BlendMode, Color, FillRule, GradientStop, Mask, Paint, PathBuilder, Pixmap, PixmapPaint, Point,
    Rect, Shader, SpreadMode, Transform,
};

/// A rasterized colour glyph.
pub struct ColorRaster {
    /// Premultiplied RGBA, row-major, `w * h * 4` bytes.
    pub data: Vec<u8>,
    pub w: u32,
    pub h: u32,
    /// Offset of the bitmap's left edge from the pen, in pixels.
    pub left: i32,
    /// Offset of the bitmap's top edge *above* the baseline, in pixels — the same sign convention
    /// as swash's `placement.top`, so a caller places both kinds of glyph identically.
    pub top: i32,
}

/// A colour glyph is more than a few hundred paints only if the font is pathological or hostile.
///
/// skrifa already guards against paint cycles; this bounds the other runaway, a graph that is
/// acyclic but exponential through repeated subgraph references.
const MAX_PAINTS: u32 = 4096;

/// Rasterize `glyph` from `data` at `px` pixels-per-em, or `None` when it has no colour glyph.
///
/// `face_offset` is the face's table-directory offset within `data` (0 for a plain font file);
/// [`face_index`] turns one into a collection index.
///
/// `foreground` is premultiplication-free RGBA, used for the fills that reference the *text*
/// colour rather than a palette entry (palette index `0xFFFF`). Noto Color Emoji never does, but
/// the format allows it, so it is a parameter and not a guess — a caller that caches these must
/// therefore key on it.
pub fn rasterize(
    data: &[u8],
    face_offset: u32,
    glyph: u16,
    px: f32,
    foreground: [u8; 4],
) -> Option<ColorRaster> {
    let font = FontRef::from_index(data, face_index(data, face_offset)).ok()?;
    rasterize_from(&font, GlyphId::from(glyph), px, foreground)
}

/// The collection index of the face whose table directory sits at `offset`.
///
/// cosmic-text carries the offset (swash's addressing), skrifa wants the index; for a plain
/// `.ttf`/`.otf` both are zero, and only a `.ttc` makes them differ.
pub fn face_index(data: &[u8], offset: u32) -> u32 {
    if offset == 0 || data.get(..4) != Some(b"ttcf") {
        return 0;
    }
    let word = |at: usize| -> Option<u32> {
        Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?))
    };
    let count = word(8).unwrap_or(0);
    (0..count)
        .find(|i| word(12 + *i as usize * 4) == Some(offset))
        .unwrap_or(0)
}

fn rasterize_from(
    font: &FontRef,
    glyph: GlyphId,
    px: f32,
    foreground: [u8; 4],
) -> Option<ColorRaster> {
    let color_glyph = font.color_glyphs().get(glyph)?;
    let upem = font.head().ok()?.units_per_em();
    if upem == 0 || !(px.is_finite() && px > 0.) {
        return None;
    }
    let scale = px / upem as f32;

    let bounds = glyph_bounds(font, &color_glyph, glyph, px, scale)?;
    let left = bounds.x_min.floor() as i32;
    let top = bounds.y_max.ceil() as i32;
    let w = u32::try_from(bounds.x_max.ceil() as i32 - left).ok()?;
    let h = u32::try_from(top - bounds.y_min.floor() as i32).ok()?;
    if w == 0 || h == 0 {
        return None;
    }

    let mut clip = Mask::new(w, h)?;
    // `Mask::new` is fully *masked out*; the base clip has to be fully open.
    clip.invert();

    let mut painter = Painter {
        font,
        palette: palette(font),
        foreground: Color::from_rgba8(foreground[0], foreground[1], foreground[2], foreground[3]),
        // Font units are y-up from the baseline; a pixmap is y-down from its top-left corner.
        transforms: vec![Transform::from_row(
            scale,
            0.,
            0.,
            -scale,
            -left as f32,
            top as f32,
        )],
        clips: vec![clip],
        layers: vec![Pixmap::new(w, h)?],
        paints: 0,
    };
    color_glyph
        .paint(LocationRef::default(), &mut painter)
        .ok()?;

    // Unbalanced pushes would leave layers stranded; merge them rather than lose the ink.
    while painter.layers.len() > 1 {
        painter.pop_layer_with_mode(CompositeMode::SrcOver);
    }
    let pixmap = painter.layers.pop()?;
    Some(ColorRaster {
        w: pixmap.width(),
        h: pixmap.height(),
        data: pixmap.take(),
        left,
        top,
    })
}

/// The glyph's extent in scaled pixels: its COLRv1 clip box when it has one, else the union of
/// the bounding boxes of the glyphs it clips with.
fn glyph_bounds(
    font: &FontRef,
    color_glyph: &skrifa::color::ColorGlyph,
    glyph: GlyphId,
    px: f32,
    scale: f32,
) -> Option<BoundingBox<f32>> {
    if let Some(clip_box) = color_glyph.bounding_box(LocationRef::default(), Size::new(px)) {
        return Some(clip_box);
    }
    // No clip box (every COLRv0 glyph, and COLRv1 fonts that omit them): ask the paint graph
    // which glyphs it draws with and take the union of their outlines, in font units.
    let mut extent = ExtentPainter {
        font,
        transforms: vec![Transform::from_scale(scale, scale)],
        bounds: None,
    };
    color_glyph
        .paint(LocationRef::default(), &mut extent)
        .ok()?;
    let rect = extent.bounds.or_else(|| {
        // Nothing measurable: fall back to the glyph's own outline, then to the em box, so a
        // colour glyph never rasterizes to nothing merely because we could not size it.
        font.outline_glyphs()
            .get(glyph)
            .and_then(|o| bbox_of(&o, Transform::from_scale(scale, scale)))
    })?;
    // The extent painter scales but does not flip, so its rect is already y-up like the clip box
    // it stands in for; `Rect` merely calls the smaller edge "top".
    Some(BoundingBox {
        x_min: rect.left(),
        y_min: rect.top(),
        x_max: rect.right(),
        y_max: rect.bottom(),
    })
}

fn palette(font: &FontRef) -> Vec<Color> {
    font.cpal()
        .ok()
        .and_then(|cpal| cpal.color_records_array()?.ok())
        .map(|records| {
            records
                .iter()
                .map(|r| Color::from_rgba8(r.red(), r.green(), r.blue(), r.alpha()))
                .collect()
        })
        .unwrap_or_default()
}

fn blend(mode: CompositeMode) -> BlendMode {
    match mode {
        CompositeMode::Clear => BlendMode::Clear,
        CompositeMode::Src => BlendMode::Source,
        CompositeMode::Dest => BlendMode::Destination,
        CompositeMode::DestOver => BlendMode::DestinationOver,
        CompositeMode::SrcIn => BlendMode::SourceIn,
        CompositeMode::DestIn => BlendMode::DestinationIn,
        CompositeMode::SrcOut => BlendMode::SourceOut,
        CompositeMode::DestOut => BlendMode::DestinationOut,
        CompositeMode::SrcAtop => BlendMode::SourceAtop,
        CompositeMode::DestAtop => BlendMode::DestinationAtop,
        CompositeMode::Xor => BlendMode::Xor,
        CompositeMode::Plus => BlendMode::Plus,
        CompositeMode::Screen => BlendMode::Screen,
        CompositeMode::Overlay => BlendMode::Overlay,
        CompositeMode::Darken => BlendMode::Darken,
        CompositeMode::Lighten => BlendMode::Lighten,
        CompositeMode::ColorDodge => BlendMode::ColorDodge,
        CompositeMode::ColorBurn => BlendMode::ColorBurn,
        CompositeMode::HardLight => BlendMode::HardLight,
        CompositeMode::SoftLight => BlendMode::SoftLight,
        CompositeMode::Difference => BlendMode::Difference,
        CompositeMode::Exclusion => BlendMode::Exclusion,
        CompositeMode::Multiply => BlendMode::Multiply,
        CompositeMode::HslHue => BlendMode::Hue,
        CompositeMode::HslSaturation => BlendMode::Saturation,
        CompositeMode::HslColor => BlendMode::Color,
        CompositeMode::HslLuminosity => BlendMode::Luminosity,
        // `SrcOver` and anything a newer COLR revision adds that we do not know.
        _ => BlendMode::SourceOver,
    }
}

fn spread(extend: Extend) -> SpreadMode {
    match extend {
        Extend::Repeat => SpreadMode::Repeat,
        Extend::Reflect => SpreadMode::Reflect,
        _ => SpreadMode::Pad,
    }
}

fn colr_transform(t: ColrTransform) -> Transform {
    Transform::from_row(t.xx, t.yx, t.xy, t.yy, t.dx, t.dy)
}

/// Collects an outline into a tiny-skia path.
#[derive(Default)]
struct PathPen(PathBuilder);

impl OutlinePen for PathPen {
    fn move_to(&mut self, x: f32, y: f32) {
        self.0.move_to(x, y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.0.line_to(x, y);
    }
    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        self.0.quad_to(cx, cy, x, y);
    }
    fn curve_to(&mut self, c1x: f32, c1y: f32, c2x: f32, c2y: f32, x: f32, y: f32) {
        self.0.cubic_to(c1x, c1y, c2x, c2y, x, y);
    }
    fn close(&mut self) {
        self.0.close();
    }
}

/// A glyph outline in font units, as a path.
fn glyph_path(font: &FontRef, glyph: GlyphId) -> Option<tiny_skia::Path> {
    let outline = font.outline_glyphs().get(glyph)?;
    let mut pen = PathPen::default();
    outline
        .draw(
            DrawSettings::unhinted(Size::unscaled(), LocationRef::default()),
            &mut pen,
        )
        .ok()?;
    pen.0.finish()
}

fn bbox_of(outline: &skrifa::outline::OutlineGlyph, at: Transform) -> Option<Rect> {
    let mut pen = PathPen::default();
    outline
        .draw(
            DrawSettings::unhinted(Size::unscaled(), LocationRef::default()),
            &mut pen,
        )
        .ok()?;
    pen.0.finish()?.transform(at).map(|p| p.bounds())
}

/// Measures a paint graph instead of drawing it, for the glyphs that carry no clip box.
struct ExtentPainter<'a> {
    font: &'a FontRef<'a>,
    transforms: Vec<Transform>,
    bounds: Option<Rect>,
}

impl ExtentPainter<'_> {
    fn add(&mut self, rect: Rect) {
        self.bounds = Some(match self.bounds {
            None => rect,
            Some(had) => Rect::from_ltrb(
                had.left().min(rect.left()),
                had.top().min(rect.top()),
                had.right().max(rect.right()),
                had.bottom().max(rect.bottom()),
            )
            .unwrap_or(had),
        });
    }
}

impl ColorPainter for ExtentPainter<'_> {
    fn push_transform(&mut self, transform: ColrTransform) {
        let next = self
            .transforms
            .last()
            .unwrap()
            .pre_concat(colr_transform(transform));
        self.transforms.push(next);
    }

    fn pop_transform(&mut self) {
        if self.transforms.len() > 1 {
            self.transforms.pop();
        }
    }

    fn push_clip_glyph(&mut self, glyph: GlyphId) {
        let at = *self.transforms.last().unwrap();
        if let Some(rect) = self
            .font
            .outline_glyphs()
            .get(glyph)
            .and_then(|o| bbox_of(&o, at))
        {
            self.add(rect);
        }
    }

    fn push_clip_box(&mut self, clip_box: BoundingBox<f32>) {
        let at = *self.transforms.last().unwrap();
        if let Some(rect) = Rect::from_ltrb(
            clip_box.x_min,
            clip_box.y_min,
            clip_box.x_max,
            clip_box.y_max,
        )
        .and_then(|r| r.transform(at))
        {
            self.add(rect);
        }
    }

    fn pop_clip(&mut self) {}
    fn fill(&mut self, _brush: Brush<'_>) {}
    fn push_layer(&mut self, _mode: CompositeMode) {}
}

/// Draws a paint graph into a pixmap.
struct Painter<'a> {
    font: &'a FontRef<'a>,
    palette: Vec<Color>,
    foreground: Color,
    /// Bottom is the transform from font units to device pixels; each push concatenates.
    transforms: Vec<Transform>,
    /// Bottom is fully open; each push intersects the one below it.
    clips: Vec<Mask>,
    /// Bottom is the destination; each push is a layer waiting to be merged down.
    layers: Vec<Pixmap>,
    paints: u32,
}

impl Painter<'_> {
    fn transform(&self) -> Transform {
        *self.transforms.last().unwrap()
    }

    fn target(&mut self) -> &mut Pixmap {
        self.layers.last_mut().unwrap()
    }

    fn color(&self, palette_index: u16, alpha: f32) -> Color {
        let mut color = if palette_index == 0xFFFF {
            self.foreground
        } else {
            *self
                .palette
                .get(palette_index as usize)
                .unwrap_or(&self.foreground)
        };
        color.set_alpha((color.alpha() * alpha).clamp(0., 1.));
        color
    }

    fn stops(&self, stops: &[ColorStop]) -> Vec<GradientStop> {
        stops
            .iter()
            .map(|s| GradientStop::new(s.offset, self.color(s.palette_index, s.alpha)))
            .collect()
    }

    /// Gradient geometry is in font units, so the brush rides the current transform while the fill
    /// itself covers the whole device-space clip.
    fn shader(&self, brush: &Brush<'_>) -> Option<Shader<'static>> {
        let at = self.transform();
        Some(match brush {
            Brush::Solid {
                palette_index,
                alpha,
            } => Shader::SolidColor(self.color(*palette_index, *alpha)),
            Brush::LinearGradient {
                p0,
                p1,
                color_stops,
                extend,
            } => tiny_skia::LinearGradient::new(
                Point::from_xy(p0.x, p0.y),
                Point::from_xy(p1.x, p1.y),
                self.stops(color_stops),
                spread(*extend),
                at,
            )?,
            Brush::RadialGradient {
                c0,
                r0,
                c1,
                r1,
                color_stops,
                extend,
            } => tiny_skia::RadialGradient::new(
                Point::from_xy(c0.x, c0.y),
                // skrifa's normalization can hand back a negative radius; the spec's answer is to
                // truncate the colour line at zero, and clamping is the cheap half of that.
                r0.max(0.),
                Point::from_xy(c1.x, c1.y),
                r1.max(0.),
                self.stops(color_stops),
                spread(*extend),
                at,
            )?,
            Brush::SweepGradient {
                c0,
                start_angle,
                end_angle,
                color_stops,
                extend,
            } => tiny_skia::SweepGradient::new(
                Point::from_xy(c0.x, c0.y),
                *start_angle,
                *end_angle,
                self.stops(color_stops),
                spread(*extend),
                at,
            )?,
        })
    }
}

impl ColorPainter for Painter<'_> {
    fn push_transform(&mut self, transform: ColrTransform) {
        let next = self.transform().pre_concat(colr_transform(transform));
        self.transforms.push(next);
    }

    fn pop_transform(&mut self) {
        if self.transforms.len() > 1 {
            self.transforms.pop();
        }
    }

    fn push_clip_glyph(&mut self, glyph: GlyphId) {
        let mut mask = self.clips.last().unwrap().clone();
        match glyph_path(self.font, glyph) {
            Some(path) => mask.intersect_path(&path, FillRule::Winding, true, self.transform()),
            // A clip we cannot resolve clips everything away, not nothing: a layer that failed to
            // be bounded must not spill across the whole glyph.
            None => mask.clear(),
        }
        self.clips.push(mask);
    }

    fn push_clip_box(&mut self, clip_box: BoundingBox<f32>) {
        let mut mask = self.clips.last().unwrap().clone();
        match Rect::from_ltrb(
            clip_box.x_min,
            clip_box.y_min,
            clip_box.x_max,
            clip_box.y_max,
        )
        .and_then(|r| PathBuilder::from_rect(r).transform(self.transform()))
        {
            Some(path) => {
                mask.intersect_path(&path, FillRule::Winding, true, Transform::identity())
            }
            None => mask.clear(),
        }
        self.clips.push(mask);
    }

    fn pop_clip(&mut self) {
        if self.clips.len() > 1 {
            self.clips.pop();
        }
    }

    fn fill(&mut self, brush: Brush<'_>) {
        self.paints += 1;
        if self.paints > MAX_PAINTS {
            return;
        }
        let Some(shader) = self.shader(&brush) else {
            return;
        };
        let paint = Paint {
            shader,
            anti_alias: true,
            ..Paint::default()
        };
        let clip = self.clips.last().unwrap().clone();
        let target = self.target();
        let Some(rect) = Rect::from_xywh(0., 0., target.width() as f32, target.height() as f32)
        else {
            return;
        };
        target.fill_rect(rect, &paint, Transform::identity(), Some(&clip));
    }

    fn push_layer(&mut self, _mode: CompositeMode) {
        let (w, h) = {
            let target = self.layers.last().unwrap();
            (target.width(), target.height())
        };
        // A 1x1 stand-in when the allocation fails keeps the stacks balanced: `pop_layer` must
        // always find something to pop, or every later layer merges into the wrong destination.
        let layer = Pixmap::new(w, h).or_else(|| Pixmap::new(1, 1));
        self.layers.extend(layer);
    }

    fn pop_layer_with_mode(&mut self, mode: CompositeMode) {
        if self.layers.len() < 2 {
            return;
        }
        let layer = self.layers.pop().unwrap();
        let paint = PixmapPaint {
            blend_mode: blend(mode),
            ..PixmapPaint::default()
        };
        self.target()
            .draw_pixmap(0, 0, layer.as_ref(), &paint, Transform::identity(), None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The system's COLRv1 emoji font, or `None` on a machine without one.
    ///
    /// Read straight off the filesystem rather than through fontconfig: these tests are about the
    /// rasterizer, and a runner whose `Noto Color Emoji` resolves to the monochrome face would
    /// otherwise report a font-packaging difference as a rasterizer bug.
    fn emoji_font() -> Option<Vec<u8>> {
        [
            "/usr/share/fonts/google-noto-color-emoji-fonts/Noto-COLRv1.ttf",
            "/usr/share/fonts/truetype/noto/NotoColorEmoji.ttf",
            "/usr/share/fonts/noto/NotoColorEmoji.ttf",
        ]
        .into_iter()
        .find_map(|path| std::fs::read(path).ok())
    }

    fn glyph_for(data: &[u8], ch: char) -> u16 {
        let font = FontRef::new(data).unwrap();
        font.charmap()
            .map(ch)
            .unwrap_or_else(|| panic!("{ch} is not in the font"))
            .to_u32() as u16
    }

    fn raster(data: &[u8], ch: char, px: f32) -> ColorRaster {
        let glyph = glyph_for(data, ch);
        rasterize(data, 0, glyph, px, [0, 0, 0, 255])
            .unwrap_or_else(|| panic!("{ch} rasterized to nothing"))
    }

    /// Pixels whose channels are not all equal — the ones a coverage mask could never produce.
    fn colored(raster: &ColorRaster) -> usize {
        raster
            .data
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[0] != p[1] || p[1] != p[2])
            .count()
    }

    fn covered(raster: &ColorRaster) -> usize {
        raster
            .data
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[3] > 0)
            .count()
    }

    #[test]
    fn a_colrv1_glyph_rasterizes_in_colour() {
        let Some(data) = emoji_font() else {
            eprintln!("no colour emoji font installed; skipping");
            return;
        };
        // 😀 is a solid-filled face: yellow ground, black eyes and mouth, white teeth.
        let grin = raster(&data, '\u{1f600}', 48.);
        assert!(grin.w >= 40 && grin.h >= 40, "{}x{}", grin.w, grin.h);
        assert_eq!(grin.data.len(), (grin.w * grin.h * 4) as usize);
        // Most of the box is ink, and a good share of it is not grey.
        let area = (grin.w * grin.h) as usize;
        assert!(
            covered(&grin) > area / 2,
            "{} of {area} covered",
            covered(&grin)
        );
        assert!(
            colored(&grin) > area / 4,
            "{} of {area} coloured",
            colored(&grin)
        );
    }

    #[test]
    fn a_gradient_paint_draws_a_ramp_and_not_a_flat_fill() {
        let Some(data) = emoji_font() else {
            eprintln!("no colour emoji font installed; skipping");
            return;
        };
        // 😀's face is a radial gradient. Counting *colours* would not catch a painter that
        // flattened it — antialiasing between the flat shapes of any emoji makes hundreds of
        // distinct values. A ramp is instead visible as adjacent opaque pixels that differ by a
        // little: measured on this font, 1487 such steps with the gradient, 17 without.
        let grin = raster(&data, '\u{1f600}', 64.);
        let (w, h) = (grin.w as usize, grin.h as usize);
        let at = |x: usize, y: usize| {
            let i = (y * w + x) * 4;
            [
                grin.data[i],
                grin.data[i + 1],
                grin.data[i + 2],
                grin.data[i + 3],
            ]
        };
        let mut steps = 0;
        for y in 0..h {
            for x in 1..w {
                let (a, b) = (at(x - 1, y), at(x, y));
                if a[3] != 255 || b[3] != 255 {
                    continue;
                }
                let delta = (0..3)
                    .map(|c| (a[c] as i32 - b[c] as i32).abs())
                    .max()
                    .unwrap();
                if (1..=6).contains(&delta) {
                    steps += 1;
                }
            }
        }
        assert!(
            steps > 500,
            "only {steps} smooth steps; the gradient is flat"
        );
    }

    #[test]
    fn the_measured_extent_shares_the_clip_boxs_coordinate_space() {
        let Some(data) = emoji_font() else {
            eprintln!("no colour emoji font installed; skipping");
            return;
        };
        // `ExtentPainter` sizes the glyphs whose font gives no clip box (every COLRv0 one). It
        // works in the same y-up font space as the clip box, which is exactly what is easy to get
        // wrong — a flipped sign here reads as a glyph rasterized from the wrong half of the em.
        // Every glyph in this font *does* carry a clip box, so what this pins is that agreement.
        let font = FontRef::new(&data).unwrap();
        let scale = 64. / font.head().unwrap().units_per_em() as f32;
        for ch in ['\u{1f600}', '\u{1f308}', '\u{1f44d}'] {
            let glyph = GlyphId::from(glyph_for(&data, ch));
            let color_glyph = font.color_glyphs().get(glyph).unwrap();
            let box_ = color_glyph
                .bounding_box(LocationRef::default(), Size::new(64.))
                .expect("this font has clip boxes");

            let mut extent = ExtentPainter {
                font: &font,
                transforms: vec![Transform::from_scale(scale, scale)],
                bounds: None,
            };
            color_glyph
                .paint(LocationRef::default(), &mut extent)
                .unwrap();
            let rect = extent.bounds.expect("the paint graph draws something");

            // The clip box is authored with slack around the ink, so the measured extent may be
            // smaller — never larger, or a glyph sized this way would be cut off.
            assert!(rect.left() >= box_.x_min - 1., "{ch}: {rect:?} vs {box_:?}");
            assert!(
                rect.right() <= box_.x_max + 1.,
                "{ch}: {rect:?} vs {box_:?}"
            );
            assert!(rect.top() >= box_.y_min - 1., "{ch}: {rect:?} vs {box_:?}");
            assert!(
                rect.bottom() <= box_.y_max + 1.,
                "{ch}: {rect:?} vs {box_:?}"
            );
            // And it must not collapse: the ink fills most of the box.
            assert!(
                rect.width() > box_.x_max - box_.x_min - 12.,
                "{ch}: {rect:?}"
            );
        }
    }

    #[test]
    fn every_pixel_is_premultiplied() {
        let Some(data) = emoji_font() else {
            eprintln!("no colour emoji font installed; skipping");
            return;
        };
        // The atlas blends premultiplied-over, so a channel above its own alpha is a bug that
        // shows up as a bright halo on the glyph's antialiased edge, not as a hard failure.
        for ch in ['\u{1f600}', '\u{1f308}', '\u{2764}', '\u{1f44d}'] {
            let raster = raster(&data, ch, 24.);
            for px in raster.data.as_chunks::<4>().0 {
                assert!(
                    px[0] <= px[3] && px[1] <= px[3] && px[2] <= px[3],
                    "{ch}: {px:?} is not premultiplied"
                );
            }
        }
    }

    #[test]
    fn the_raster_scales_with_the_pixel_size() {
        let Some(data) = emoji_font() else {
            eprintln!("no colour emoji font installed; skipping");
            return;
        };
        let small = raster(&data, '\u{1f600}', 16.);
        let large = raster(&data, '\u{1f600}', 64.);
        assert!(large.w > small.w * 3, "{} vs {}", large.w, small.w);
        // The pen-relative placement scales too: both sit above the baseline.
        assert!(small.top > 0 && large.top > small.top);
    }

    #[test]
    fn a_glyph_with_no_colour_is_declined() {
        let Some(data) = emoji_font() else {
            eprintln!("no colour emoji font installed; skipping");
            return;
        };
        // .notdef is glyph 0 and has no COLR entry, so the caller falls back to the mask path.
        assert!(rasterize(&data, 0, 0, 48., [0, 0, 0, 255]).is_none());
    }

    #[test]
    fn a_plain_font_file_is_face_zero() {
        // Only a collection makes an offset mean anything; a stray one must not index off the end.
        assert_eq!(face_index(b"\x00\x01\x00\x00rest", 0), 0);
        assert_eq!(face_index(b"\x00\x01\x00\x00rest", 4096), 0);
    }
}
