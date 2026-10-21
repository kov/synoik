// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Text on the CPU, for the initramfs splash (`docs/fork/boot-splash.md` §7.3): the outer's
//! shaping (`synoik_vk::text::TextContext::shape_line_named`: the same metrics, hinting and
//! rasterization), into coverage masks the painter blends. Its fonts come from named files, not
//! fontconfig: the image has none.

use std::collections::HashMap;
use std::path::Path;

use cosmic_text::{
    fontdb, Attrs, Buffer, CacheKey, Family, FontSystem, Hinting, Metrics, Shaping, Weight,
};
use swash::scale::image::Content;
use swash::scale::{Render, ScaleContext, Source};
use swash::zeno::{Format, Vector};

use crate::layout::{Font, Size};

/// Where the image carries the faces the splash sets text in ([`Font::family`]): the shell's sans
/// (§4.4) and its monospace (§8.2). The image builders copy the files fontconfig picks for them
/// here: distributions install them under different paths.
pub const FONT_FILE: &str = "/usr/share/synoik-splash/sans.ttf";
pub const MONO_FONT_FILE: &str = "/usr/share/synoik-splash/mono.ttf";

/// One glyph's coverage, placed relative to the line's origin.
#[derive(Debug, Clone)]
pub struct Glyph {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    /// `w * h` alpha bytes, row by row.
    pub coverage: Vec<u8>,
}

/// A shaped, rasterized line.
#[derive(Debug, Clone, Default)]
pub struct Line {
    pub glyphs: Vec<Glyph>,
    /// The first line's baseline, from the same origin as the glyphs.
    baseline: i32,
    ascent: f32,
    descent: f32,
}

impl Line {
    /// The ink's bounding box, `(x, y, w, h)` from the line's origin; zeros without ink.
    pub fn ink_bounds(&self) -> (i32, i32, i32, i32) {
        let mut bounds: Option<(i32, i32, i32, i32)> = None;
        for g in &self.glyphs {
            let (x0, y0, x1, y1) = (g.x, g.y, g.x + g.w as i32, g.y + g.h as i32);
            bounds = Some(match bounds {
                None => (x0, y0, x1, y1),
                Some((a, b, c, d)) => (a.min(x0), b.min(y0), c.max(x1), d.max(y1)),
            });
        }
        bounds.map_or((0, 0, 0, 0), |(x0, y0, x1, y1)| (x0, y0, x1 - x0, y1 - y0))
    }

    /// The font's line box, ascent plus descent; the ink's height for a line without metrics.
    pub fn line_box_height(&self) -> i32 {
        if self.ascent + self.descent <= 0. {
            return self.ink_bounds().3;
        }
        (self.ascent + self.descent).ceil() as i32
    }

    /// What the layout measures: the ink's width and the line box's height, as the outer does.
    pub fn size(&self) -> Size {
        Size::from((self.ink_bounds().2, self.line_box_height()))
    }

    /// Where the line's origin goes for its pen to start at `x`, its line box centred in a band
    /// as [`Self::origin`] centres it: for text set in columns, where the pen, not the ink, has to
    /// line up.
    pub fn pen_origin(&self, x: i32, band_top: i32, band_h: i32) -> (i32, i32) {
        (x, self.origin(x, band_top, band_h).1)
    }

    /// Where the line's origin goes for its ink to start at `x` and its line box to sit centred
    /// in a band of `band_h` at `band_top`: the outer's `Painter::text_band`.
    pub fn origin(&self, x: i32, band_top: i32, band_h: i32) -> (i32, i32) {
        let (ix, iy, _, ih) = self.ink_bounds();
        let y = if self.ascent + self.descent <= 0. {
            (band_h - ih) / 2 - iy
        } else {
            let box_top = (band_h as f32 - (self.ascent + self.descent)) / 2.;
            (box_top - (self.baseline as f32 - self.ascent)).round() as i32
        };
        (x - ix, band_top + y)
    }
}

/// The fonts, and the glyphs rasterized so far.
pub struct Fonts {
    system: FontSystem,
    scale: ScaleContext,
    glyphs: HashMap<CacheKey, Option<Glyph>>,
}

impl Fonts {
    /// The faces in `files`; those that fail to load are left out, and text in them with them.
    pub fn load(files: &[&Path]) -> Self {
        let mut db = fontdb::Database::new();
        for file in files {
            if let Err(err) = db.load_font_file(file) {
                warn!("splash: font {}: {err}", file.display());
            }
        }
        Self {
            system: FontSystem::new_with_locale_and_db("en-US".into(), db),
            scale: ScaleContext::new(),
            glyphs: HashMap::new(),
        }
    }

    /// Whether any face loaded.
    pub fn is_empty(&self) -> bool {
        self.system.db().is_empty()
    }

    fn shape(&mut self, text: &str, font: Font, scale: f64) -> (Buffer, f32) {
        let px = (font.px() * scale) as f32;
        let mut buffer = Buffer::new(&mut self.system, Metrics::new(px, (px * 1.25).round()));
        buffer.set_hinting(Hinting::Enabled);
        {
            let mut b = buffer.borrow_with(&mut self.system);
            b.set_size(None, None);
            let attrs = Attrs::new()
                .family(Family::Name(font.family()))
                .weight(Weight(font.weight()));
            b.set_text(text, &attrs, Shaping::Advanced, None);
            b.shape_until_scroll(false);
        }
        (buffer, px)
    }

    /// A character cell of `font` at `scale`: a space's advance, and the line box's height
    /// (`ply_console_viewer_new` measures a label of one space). `None` without the face.
    pub fn cell(&mut self, font: Font, scale: f64) -> Option<Size> {
        let (buffer, _) = self.shape(" ", font, scale);
        let advance = buffer.layout_runs().flat_map(|run| run.glyphs).next()?.w;
        let height = self.line(" ", font, scale).line_box_height();
        (advance > 0. && height > 0).then(|| Size::from((advance.round() as i32, height)))
    }

    /// Shape and rasterize `text` in `font` at `scale`.
    pub fn line(&mut self, text: &str, font: Font, scale: f64) -> Line {
        let (buffer, px) = self.shape(text, font, scale);

        let mut line = Line::default();
        'metrics: for run in buffer.layout_runs() {
            for glyph in run.glyphs {
                let key = glyph.physical((0., 0.), 1.).cache_key;
                if let Some(face) = self.system.get_font(key.font_id, key.font_weight) {
                    let m = face.as_swash().metrics(&[]).scale(px);
                    line.baseline = run.line_y.round() as i32;
                    line.ascent = m.ascent;
                    line.descent = m.descent;
                    break 'metrics;
                }
            }
        }
        for run in buffer.layout_runs() {
            let baseline = run.line_y.round() as i32;
            for glyph in run.glyphs {
                // Whole-pixel origin: round X, then `physical` truncates Y.
                let mut glyph = glyph.clone();
                glyph.x = glyph.x.round();
                let phys = glyph.physical((0., 0.), 1.);
                let raster = match self.glyphs.get(&phys.cache_key) {
                    Some(raster) => raster.clone(),
                    None => {
                        let raster = self.rasterize(phys.cache_key);
                        self.glyphs.insert(phys.cache_key, raster.clone());
                        raster
                    }
                };
                if let Some(mut g) = raster {
                    // Rasterized with its offset from the pen in `x`/`y`.
                    g.x += phys.x;
                    g.y = baseline + phys.y - g.y;
                    line.glyphs.push(g);
                }
            }
        }
        line
    }

    /// `key`'s coverage, its `x` the ink's offset right of the pen and its `y` the ink's top above
    /// the baseline; `None` for a glyph without ink.
    fn rasterize(&mut self, key: CacheKey) -> Option<Glyph> {
        let face = self.system.get_font(key.font_id, key.font_weight)?;
        let px = f32::from_bits(key.font_size_bits);
        let swash = face.as_swash();
        let mut scaler = self.scale.builder(swash).size(px).hint(true);
        // A variable face's outlines at the weight it was shaped at, as cosmic-text's own
        // rasterizer does (`swash_image`).
        if let Some(axis) = swash
            .variations()
            .find_by_tag(swash::Tag::from_be_bytes(*b"wght"))
        {
            let weight = f32::from(key.font_weight.0).clamp(axis.min_value(), axis.max_value());
            scaler = scaler.normalized_coords(
                swash
                    .variations()
                    .normalized_coords([(swash::Tag::from_be_bytes(*b"wght"), weight)]),
            );
        }
        let mut scaler = scaler.build();
        let offset = Vector::new(key.x_bin.as_float(), key.y_bin.as_float());
        let image = Render::new(&[Source::Outline])
            .format(Format::Alpha)
            .offset(offset)
            .render(&mut scaler, key.glyph_id)?;
        let (w, h) = (image.placement.width, image.placement.height);
        if w == 0 || h == 0 || !matches!(image.content, Content::Mask) {
            return None;
        }
        Some(Glyph {
            x: image.placement.left,
            y: image.placement.top,
            w,
            h,
            coverage: image.data,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The installed face, found as the image builders find it.
    fn fonts() -> Option<Fonts> {
        let file = std::process::Command::new("fc-match")
            .args(["-f", "%{file}", Font::Regular.family()])
            .output()
            .ok()
            .and_then(|out| String::from_utf8(out.stdout).ok())
            .filter(|file| file.contains("AdwaitaSans"));
        let Some(file) = file else {
            eprintln!("skipping: no Adwaita Sans installed");
            return None;
        };
        let fonts = Fonts::load(&[Path::new(&file)]);
        (!fonts.is_empty()).then_some(fonts)
    }

    #[test]
    fn a_line_has_ink_and_a_line_box() {
        let Some(mut fonts) = fonts() else { return };
        let line = fonts.line("Passphrase for data", Font::Regular, 1.);
        let size = line.size();
        assert!(size.w > 100 && size.w < 200, "{size:?}");
        // A 16 px face's line box is taller than its em.
        assert!(size.h > 16 && size.h < 30, "{size:?}");
        assert!(line
            .glyphs
            .iter()
            .all(|g| g.coverage.len() == (g.w * g.h) as usize));
    }

    #[test]
    fn the_weight_axis_applies() {
        let Some(mut fonts) = fonts() else { return };
        let regular = fonts.line("English (US)", Font::Regular, 1.).size().w;
        // Same size, bold: the variable face's weight axis has to widen it.
        let bold = |fonts: &mut Fonts| {
            let line = fonts.line("English (US)", Font::Keymap, 1.);
            line.size().w as f64 * Font::Regular.px() / Font::Keymap.px()
        };
        let bold = bold(&mut fonts);
        assert!(bold > f64::from(regular) * 1.03, "{bold} vs {regular}");
    }

    #[test]
    fn whitespace_has_no_ink() {
        let Some(mut fonts) = fonts() else { return };
        assert_eq!(
            fonts.line("   ", Font::Regular, 1.).ink_bounds(),
            (0, 0, 0, 0)
        );
    }
}
