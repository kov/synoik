//! Hinted glyph-atlas text: cosmic-text shapes (harfrust), swash rasterizes each glyph with
//! hinting on, etagere packs the coverage bitmaps into one R8 atlas, and each glyph draws as a
//! textured quad. This is the "own the text stack" path — cosmic-text/swash give shaping + hinted
//! coverage; the atlas and rendering are ours.
//!
//! Crisp 1x text needs three things acting together (per the swash/cosmic-text source):
//!   1. `Buffer::set_hinting(Hinting::Enabled)` snaps the pen X during layout,
//!   2. rounding `glyph.x` before `physical((0,0), 1.0)` forces a whole-pixel origin (x_bin=0),
//!   3. swash `.hint(true)` grid-fits the outline vertically.
//!
//! swash's hinter is skrifa's own (autohint-style), not the font's TrueType bytecode, so results
//! differ slightly from pango/cairo/FreeType — expected, not a bug.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use anyhow::{bail, Context, Result};
use ash::vk;
use cosmic_text::{
    Align, Attrs, Buffer, CacheKey, Family, FontSystem, Hinting, Metrics, Shaping, Weight,
};
use etagere::{size2, AtlasAllocator};
use swash::scale::image::Content;
use swash::scale::{Render, ScaleContext, Source};
use swash::zeno::{Format, Vector};

use crate::gpu::Gpu;
use crate::render::{as_bytes, load_module, TextPush};
use crate::shaders::{TEXT_FRAG, TEXT_VERT};
use crate::texture::Texture;

/// One placed glyph: where it goes on screen (run-local, top-left) and its slot in the atlas.
#[derive(Clone, Copy, Debug)]
pub struct PlacedGlyph {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub atlas_x: u32,
    pub atlas_y: u32,
}

pub struct GlyphAtlas {
    pub texture: Texture,
    pub glyphs: Vec<PlacedGlyph>,
    /// Source-span index of each glyph in [`Self::glyphs`], parallel to it. For
    /// [`TextContext::build_paragraph`] this is the position in its `spans` slice; for the
    /// single-run [`TextContext::build_atlas`] it is all zeroes. Lets a caller recover a span's
    /// ink rectangle to paint an inline background (e.g. a keycap).
    pub spans: Vec<u32>,
    pub side: u32,
}

/// Atlas slot (top-left in atlas px) for each distinct glyph, keyed by its shaping cache key.
type GlyphSlots = HashMap<CacheKey, (u32, u32)>;

/// A process-global font system used only for GPU-free text *measurement* — sizing a hit rectangle
/// or a layout box before any renderer (hence any [`TextContext`]) exists. The first call scans the
/// system fonts (tens of ms); every measurement after is cheap. Shaping only, never rasterization,
/// so it needs no [`Gpu`]. The draw path uses the renderer's own [`TextContext`] font system.
fn measure_fonts() -> &'static Mutex<FontSystem> {
    static FONTS: OnceLock<Mutex<FontSystem>> = OnceLock::new();
    FONTS.get_or_init(|| Mutex::new(FontSystem::new()))
}

/// Logical width, in pixels, of a single-line `text` shaped SansSerif at `px` pixels-per-em — the
/// advance the renderer lays the run out to. GPU-free (shaping only), so callers can size a hit
/// rectangle at construction time, before a renderer exists. Matches the width `build_glyph_run`
/// would produce at the same `px` (both shape SansSerif through cosmic-text).
pub fn measure_line_width(text: &str, px: f32) -> f64 {
    measure_line_width_weighted(text, px, false)
}

/// Like [`measure_line_width`], but shapes at [`Weight::BOLD`] when `bold` — the width a bold run
/// (e.g. the panel clock, which draws `font-weight: bold` like GNOME's `panel_button`) lays out to,
/// so its hit rectangle and centering match the glyphs `build_glyph_run_weighted` rasterizes.
pub fn measure_line_width_weighted(text: &str, px: f32, bold: bool) -> f64 {
    let mut fonts = measure_fonts().lock().unwrap();
    let mut buffer = Buffer::new(&mut fonts, Metrics::new(px, (px * 1.25).round()));
    {
        let mut b = buffer.borrow_with(&mut fonts);
        b.set_size(None, None);
        let mut attrs = Attrs::new().family(Family::SansSerif);
        if bold {
            attrs = attrs.weight(Weight::BOLD);
        }
        b.set_text(text, &attrs, Shaping::Advanced, None);
        b.shape_until_scroll(false);
    }
    buffer
        .layout_runs()
        .map(|run| run.line_w)
        .fold(0.0_f32, f32::max) as f64
}

/// Shape `text` wrapped to `wrap_px` and return each visual line's byte range. Ranges come from
/// the laid-out glyphs, so they follow the same word-then-glyph break rules the draw path uses
/// (cosmic-text's default wrap, the equivalent of Pango's `WORD_CHAR`).
fn shape_line_ranges(
    fonts: &mut FontSystem,
    text: &str,
    px: f32,
    bold: bool,
    wrap_px: f32,
) -> Vec<(usize, usize)> {
    let mut buffer = Buffer::new(fonts, Metrics::new(px, (px * 1.25).round()));
    {
        let mut b = buffer.borrow_with(fonts);
        b.set_size(Some(wrap_px), None);
        let mut attrs = Attrs::new().family(Family::SansSerif);
        if bold {
            attrs = attrs.weight(Weight::BOLD);
        }
        b.set_text(text, &attrs, Shaping::Advanced, None);
        b.shape_until_scroll(false);
    }
    buffer
        .layout_runs()
        .map(|run| {
            // Glyphs come in VISUAL order — a bidi run (RTL words in an LTR
            // paragraph, or vice versa) stores them byte-reversed, so
            // first/last would produce an inverted (panicking) range. A
            // visual line still covers one contiguous logical span, so
            // min/max over the glyph ranges recovers it.
            let start = run.glyphs.iter().map(|g| g.start).min().unwrap_or(0);
            let end = run.glyphs.iter().map(|g| g.end).max().unwrap_or(start);
            (start, end)
        })
        .collect()
}

/// Wrap single-paragraph `text` (pre-flatten newlines) at `wrap_px` into at most `max_lines`
/// visual lines; when it doesn't fit, the last line is truncated with a `…`. Returns each line's
/// text, top to bottom, so a caller can lay the lines out itself (at whatever pixel density) with
/// scale-independent break points. GPU-free (shaping only), like [`measure_line_width`]. This is
/// the notification-card body path: GNOME clamps a message body to one ellipsized line collapsed
/// and six expanded (`LabelExpanderLayout`, gnome-shell `js/ui/messageList.js:220-275`).
pub fn wrap_lines_weighted(
    text: &str,
    px: f32,
    bold: bool,
    wrap_px: f64,
    max_lines: usize,
) -> Vec<String> {
    const ELLIPSIS: char = '\u{2026}';
    let max_lines = max_lines.max(1);
    if text.is_empty() {
        return Vec::new();
    }
    let mut fonts = measure_fonts().lock().unwrap();
    let wrap = wrap_px as f32;
    let ranges = shape_line_ranges(&mut fonts, text, px, bold, wrap);
    if ranges.len() <= max_lines {
        return ranges.iter().map(|&(s, e)| text[s..e].to_owned()).collect();
    }
    // Cut at the end of the last kept line, append the ellipsis, and pop characters until the
    // ellipsis no longer spills onto an extra line (word wrap can pull the whole last word down
    // with it, so this may retreat past a word boundary).
    let (_, mut cut) = ranges[max_lines - 1];
    loop {
        let head = text[..cut].trim_end();
        let candidate = format!("{head}{ELLIPSIS}");
        let ranges = shape_line_ranges(&mut fonts, &candidate, px, bold, wrap);
        if ranges.len() <= max_lines {
            return ranges
                .iter()
                .map(|&(s, e)| candidate[s..e].to_owned())
                .collect();
        }
        match head.char_indices().next_back() {
            Some((idx, _)) if idx > 0 => cut = idx,
            _ => return vec![ELLIPSIS.to_string()],
        }
    }
}

/// The long-lived pieces of the text stack. `FontSystem::new()` scans and parses the system fonts
/// (tens of ms); `ScaleContext` caches per-font scaler state. Both are expensive to build and
/// cheap to reuse, so the compositor holds ONE `TextContext` for the life of the renderer and
/// rebuilds only the per-string atlas. (The atlas itself is still per-call for now — a shared
/// growing atlas is deferred until dynamic text needs it.)
pub struct TextContext {
    fonts: FontSystem,
    scale: ScaleContext,
}

impl Default for TextContext {
    fn default() -> Self {
        Self::new()
    }
}

impl TextContext {
    pub fn new() -> Self {
        TextContext {
            fonts: FontSystem::new(),
            scale: ScaleContext::new(),
        }
    }

    /// Shape a single run of `text` at `px` pixels-per-em (SansSerif), rasterize hinted, and pack
    /// into an R8 coverage atlas, reusing this context's font system and scaler cache.
    pub fn build_atlas(
        &mut self,
        gpu: &Gpu,
        pool: vk::CommandPool,
        text: &str,
        px: f32,
    ) -> Result<GlyphAtlas> {
        self.build_atlas_weighted(gpu, pool, text, px, false)
    }

    /// Like [`Self::build_atlas`], but shapes and rasterizes at [`Weight::BOLD`] when `bold` — the
    /// bold panel-clock path (GNOME's `panel_button` is `font-weight: bold`).
    pub fn build_atlas_weighted(
        &mut self,
        gpu: &Gpu,
        pool: vk::CommandPool,
        text: &str,
        px: f32,
        bold: bool,
    ) -> Result<GlyphAtlas> {
        let mut buffer = Buffer::new(&mut self.fonts, Metrics::new(px, (px * 1.25).round()));
        buffer.set_hinting(Hinting::Enabled);
        {
            let mut b = buffer.borrow_with(&mut self.fonts);
            b.set_size(None, None);
            let mut attrs = Attrs::new().family(Family::SansSerif);
            if bold {
                attrs = attrs.weight(Weight::BOLD);
            }
            b.set_text(text, &attrs, Shaping::Advanced, None);
            b.shape_until_scroll(false);
        }
        Self::rasterize(&mut self.fonts, &mut self.scale, gpu, pool, &buffer)
    }

    /// Lay out a styled, center-aligned paragraph wrapped to `wrap_px` pixels: each [`TextSpan`]
    /// carries its own family/weight/size (via cosmic-text rich text), and the multi-line result is
    /// rasterized hinted into one R8 coverage atlas. `base_px` sets the default line metrics.
    /// Glyph placements are paragraph-local (top-left origin), so one [`GlyphAtlas`] draws the
    /// whole block. This is the dialog/notification text path (title + body + hints in one
    /// box).
    pub fn build_paragraph(
        &mut self,
        gpu: &Gpu,
        pool: vk::CommandPool,
        spans: &[TextSpan],
        wrap_px: f32,
        base_px: f32,
    ) -> Result<GlyphAtlas> {
        let mut buffer = Buffer::new(
            &mut self.fonts,
            Metrics::new(base_px, (base_px * 1.25).round()),
        );
        buffer.set_hinting(Hinting::Enabled);
        {
            let mut b = buffer.borrow_with(&mut self.fonts);
            b.set_size(Some(wrap_px), None);
            let default_attrs = Attrs::new().family(Family::SansSerif);
            // Tag each span with its index (cosmic-text carries it to every laid-out glyph as
            // `metadata`), so `rasterize` can record which span each glyph came from and a caller
            // can recover a span's ink rectangle (e.g. to paint an inline keycap background).
            let rich = spans
                .iter()
                .enumerate()
                .map(|(i, s)| (s.text, s.attrs().metadata(i)));
            b.set_rich_text(rich, &default_attrs, Shaping::Advanced, Some(Align::Center));
            b.shape_until_scroll(false);
        }
        Self::rasterize(&mut self.fonts, &mut self.scale, gpu, pool, &buffer)
    }

    /// Rasterize every glyph of an already-shaped `buffer` (hinted) into one R8 coverage atlas.
    /// Shared by [`Self::build_atlas`] and [`Self::build_paragraph`]; the placements it emits are
    /// buffer-local (top-left), spanning as many lines as the buffer holds.
    fn rasterize(
        fonts: &mut FontSystem,
        ctx: &mut ScaleContext,
        gpu: &Gpu,
        pool: vk::CommandPool,
        buffer: &Buffer,
    ) -> Result<GlyphAtlas> {
        // One R8 coverage bitmap per DISTINCT glyph (a `CacheKey` folds in glyph id, font, size,
        // weight, and subpixel bin) — a repeated character costs one atlas slot, not one per
        // instance. Rasterize each distinct glyph once; every on-screen instance then points at the
        // shared slot. Without this dedup a long line (a pasted / history-recalled command) blows a
        // fixed atlas one slot per character; with it the slot count is bounded by the alphabet.
        struct Raster {
            data: Vec<u8>,
            w: u32,
            h: u32,
            left: i32,
            top: i32,
        }
        // `None` marks a key that draws nothing (whitespace / missing font / empty raster), so we
        // neither re-rasterize it nor emit an instance for it.
        let mut distinct: HashMap<CacheKey, Option<Raster>> = HashMap::new();
        // Per-instance placement (one entry per on-screen glyph): key, x, y, source-span index.
        let mut instances: Vec<(CacheKey, i32, i32, u32)> = Vec::new();

        for run in buffer.layout_runs() {
            let baseline = run.line_y.round() as i32;
            for glyph in run.glyphs {
                // Whole-pixel origin: round X, then physical() truncates Y (its own hinting).
                let mut lg = glyph.clone();
                lg.x = lg.x.round();
                let phys = lg.physical((0.0, 0.0), 1.0);
                let key = phys.cache_key;
                let span = glyph.metadata as u32;

                let raster = distinct.entry(key).or_insert_with(|| {
                    // Same font cosmic-text shaped with (get_font promotes File->SharedFile).
                    let font = fonts.get_font(key.font_id, key.font_weight)?;
                    let mut scaler = ctx
                        .builder(font.as_swash())
                        .size(f32::from_bits(key.font_size_bits))
                        .hint(true)
                        .build();

                    // Subpixel remainder from the cache key (0 once fully snapped).
                    let offset = Vector::new(key.x_bin.as_float(), key.y_bin.as_float());
                    let image = Render::new(&[Source::Outline])
                        .format(Format::Alpha)
                        .offset(offset)
                        .render(&mut scaler, key.glyph_id)?;

                    let (w, h) = (image.placement.width, image.placement.height);
                    if w == 0 || h == 0 {
                        return None; // whitespace: no bitmap, pen still advances
                    }
                    debug_assert!(matches!(image.content, Content::Mask));
                    Some(Raster {
                        data: image.data,
                        w,
                        h,
                        left: image.placement.left,
                        top: image.placement.top,
                    })
                });

                if let Some(raster) = raster {
                    instances.push((
                        key,
                        phys.x + raster.left,
                        baseline + phys.y - raster.top,
                        span,
                    ));
                }
            }
        }

        // Demand-size the atlas: pick the smallest power-of-two square that plausibly holds every
        // distinct slot (each `(w+1)x(h+1)` for a 1px bleed guard), then pack. `pack` returns the
        // per-key slot positions, or `None` if this side couldn't fit them (etagere packing can
        // fall short of the area estimate); grow and retry up to `MAX_SIDE`.
        const MAX_SIDE: u32 = 2048;
        let drawn: Vec<(CacheKey, &Raster)> = distinct
            .iter()
            .filter_map(|(k, r)| r.as_ref().map(|r| (*k, r)))
            .collect();

        let total_area: u64 = drawn
            .iter()
            .map(|(_, r)| u64::from(r.w + 1) * u64::from(r.h + 1))
            .sum();
        let max_slot = drawn
            .iter()
            .map(|(_, r)| (r.w + 1).max(r.h + 1))
            .max()
            .unwrap_or(1);
        // Start from the larger of 256, the biggest single slot, and ~sqrt(area / 0.7 packing).
        let mut side = 256u32.max(max_slot.next_power_of_two());
        while side < MAX_SIDE && u64::from(side) * u64::from(side) * 7 < total_area * 10 {
            side *= 2;
        }

        let pack = |side: u32| -> Option<(Vec<u8>, GlyphSlots)> {
            let mut pixels = vec![0u8; (side * side) as usize];
            let mut atlas = AtlasAllocator::new(size2(side as i32, side as i32));
            let mut slots = HashMap::with_capacity(drawn.len());
            for (key, r) in &drawn {
                let alloc = atlas.allocate(size2(r.w as i32 + 1, r.h as i32 + 1))?;
                let (ax, ay) = (alloc.rectangle.min.x as u32, alloc.rectangle.min.y as u32);
                for row in 0..r.h {
                    let src = &r.data[(row * r.w) as usize..((row + 1) * r.w) as usize];
                    let dst = ((ay + row) * side + ax) as usize;
                    pixels[dst..dst + r.w as usize].copy_from_slice(src);
                }
                slots.insert(*key, (ax, ay));
            }
            Some((pixels, slots))
        };

        let (atlas_pixels, slots) = loop {
            if let Some(packed) = pack(side) {
                break packed;
            }
            if side >= MAX_SIDE {
                bail!(
                    "glyph atlas full: {} distinct glyphs exceed {MAX_SIDE}px",
                    drawn.len()
                );
            }
            side *= 2;
        };

        // Resolve every instance to its shared slot; `spans` stays parallel to `glyphs`.
        let mut glyphs = Vec::with_capacity(instances.len());
        let mut spans = Vec::with_capacity(instances.len());
        for &(key, x, y, span) in &instances {
            let (ax, ay) = slots[&key];
            let r = distinct[&key]
                .as_ref()
                .expect("drawn instance has a raster");
            glyphs.push(PlacedGlyph {
                x,
                y,
                w: r.w,
                h: r.h,
                atlas_x: ax,
                atlas_y: ay,
            });
            spans.push(span);
        }

        // 1:1 glyph-px to screen-px with integer placement, so NEAREST sampling is pixel-exact.
        let texture =
            Texture::from_coverage(gpu, pool, side, side, &atlas_pixels, vk::Filter::NEAREST)?;
        Ok(GlyphAtlas {
            texture,
            glyphs,
            spans,
            side,
        })
    }
}

/// One styled span of a [`TextContext::build_paragraph`] paragraph.
#[derive(Clone, Copy)]
pub struct TextSpan<'a> {
    pub text: &'a str,
    pub family: SpanFamily,
    pub bold: bool,
    /// Font size in pixels-per-em for this span.
    pub px: f32,
}

/// The font family of a [`TextSpan`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SpanFamily {
    Sans,
    Mono,
}

impl TextSpan<'_> {
    fn attrs(&self) -> Attrs<'static> {
        let family = match self.family {
            SpanFamily::Sans => Family::SansSerif,
            SpanFamily::Mono => Family::Monospace,
        };
        let weight = if self.bold {
            Weight::BOLD
        } else {
            Weight::NORMAL
        };
        Attrs::new()
            .family(family)
            .weight(weight)
            .metrics(Metrics::new(self.px, (self.px * 1.25).round()))
    }
}

/// Shape `text` at `px` pixels-per-em, rasterize hinted, and pack into an R8 coverage atlas.
/// One-shot: builds a throwaway [`TextContext`]. Callers drawing repeatedly should hold a
/// `TextContext` and call [`TextContext::build_atlas`] to reuse the font system.
pub fn build_text(gpu: &Gpu, pool: vk::CommandPool, text: &str, px: f32) -> Result<GlyphAtlas> {
    TextContext::new().build_atlas(gpu, pool, text, px)
}

/// Graphics pipeline for glyph quads: `text.vert` + `text.frag`, alpha blending on, one sampler.
pub struct TextRenderer {
    pipeline: vk::Pipeline,
    layout: vk::PipelineLayout,
    vert: vk::ShaderModule,
    frag: vk::ShaderModule,
}

impl TextRenderer {
    pub fn new(
        gpu: &Gpu,
        render_pass: vk::RenderPass,
        extent: vk::Extent2D,
        set_layout: vk::DescriptorSetLayout,
    ) -> Result<Self> {
        let device = &gpu.device;
        let vert = load_module(device, TEXT_VERT)?;
        let frag = load_module(device, TEXT_FRAG)?;

        let stages = [
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::VERTEX)
                .module(vert)
                .name(c"main"),
            vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::FRAGMENT)
                .module(frag)
                .name(c"main"),
        ];
        let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
        let input_assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
        let viewports = [vk::Viewport {
            x: 0.0,
            y: 0.0,
            width: extent.width as f32,
            height: extent.height as f32,
            min_depth: 0.0,
            max_depth: 1.0,
        }];
        let scissors = [vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent,
        }];
        let viewport_state = vk::PipelineViewportStateCreateInfo::default()
            .viewports(&viewports)
            .scissors(&scissors);
        let raster = vk::PipelineRasterizationStateCreateInfo::default()
            .polygon_mode(vk::PolygonMode::FILL)
            .cull_mode(vk::CullModeFlags::NONE)
            .front_face(vk::FrontFace::COUNTER_CLOCKWISE)
            .line_width(1.0);
        let multisample = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1);
        let blend_attachment = vk::PipelineColorBlendAttachmentState::default()
            .blend_enable(true)
            .src_color_blend_factor(vk::BlendFactor::SRC_ALPHA)
            .dst_color_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .color_blend_op(vk::BlendOp::ADD)
            .src_alpha_blend_factor(vk::BlendFactor::ONE)
            .dst_alpha_blend_factor(vk::BlendFactor::ONE_MINUS_SRC_ALPHA)
            .alpha_blend_op(vk::BlendOp::ADD)
            .color_write_mask(vk::ColorComponentFlags::RGBA);
        let blend = vk::PipelineColorBlendStateCreateInfo::default()
            .attachments(std::slice::from_ref(&blend_attachment));

        let push = vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(std::mem::size_of::<TextPush>() as u32);
        let layout_ci = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(std::slice::from_ref(&set_layout))
            .push_constant_ranges(std::slice::from_ref(&push));
        let layout =
            unsafe { device.create_pipeline_layout(&layout_ci, None) }.context("text layout")?;

        let ci = vk::GraphicsPipelineCreateInfo::default()
            .stages(&stages)
            .vertex_input_state(&vertex_input)
            .input_assembly_state(&input_assembly)
            .viewport_state(&viewport_state)
            .rasterization_state(&raster)
            .multisample_state(&multisample)
            .color_blend_state(&blend)
            .layout(layout)
            .render_pass(render_pass)
            .subpass(0);
        let pipeline =
            unsafe { device.create_graphics_pipelines(vk::PipelineCache::null(), &[ci], None) }
                .map_err(|(_, e)| e)
                .context("text pipeline")?[0];

        Ok(TextRenderer {
            pipeline,
            layout,
            vert,
            frag,
        })
    }

    /// Draw every glyph in `atlas`, offset to `(ox, oy)` in a `target`-sized image, in `color`.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &self,
        gpu: &Gpu,
        cbuf: vk::CommandBuffer,
        set: vk::DescriptorSet,
        atlas: &GlyphAtlas,
        origin: (f32, f32),
        target: [f32; 2],
        color: [f32; 4],
    ) {
        let device = &gpu.device;
        let side = atlas.side as f32;
        unsafe {
            device.cmd_bind_pipeline(cbuf, vk::PipelineBindPoint::GRAPHICS, self.pipeline);
            device.cmd_bind_descriptor_sets(
                cbuf,
                vk::PipelineBindPoint::GRAPHICS,
                self.layout,
                0,
                &[set],
                &[],
            );
            for g in &atlas.glyphs {
                let push = TextPush {
                    origin: [origin.0 + g.x as f32, origin.1 + g.y as f32],
                    size: [g.w as f32, g.h as f32],
                    target,
                    uv_origin: [g.atlas_x as f32 / side, g.atlas_y as f32 / side],
                    uv_size: [g.w as f32 / side, g.h as f32 / side],
                    _pad: [0.0, 0.0],
                    color,
                };
                device.cmd_push_constants(
                    cbuf,
                    self.layout,
                    vk::ShaderStageFlags::VERTEX | vk::ShaderStageFlags::FRAGMENT,
                    0,
                    as_bytes(&push),
                );
                device.cmd_draw(cbuf, 6, 1, 0, 0);
            }
        }
    }

    pub fn destroy(&self, gpu: &Gpu) {
        unsafe {
            let d = &gpu.device;
            d.destroy_pipeline(self.pipeline, None);
            d.destroy_pipeline_layout(self.layout, None);
            d.destroy_shader_module(self.vert, None);
            d.destroy_shader_module(self.frag, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::Gpu;
    use crate::render::{sampler_set_layout, RenderTarget};

    const BG: [u8; 4] = [24, 24, 28, 255];
    const FG: [u8; 4] = [235, 235, 235, 255];

    fn unorm(c: [u8; 4]) -> [f32; 4] {
        [
            c[0] as f32 / 255.,
            c[1] as f32 / 255.,
            c[2] as f32 / 255.,
            c[3] as f32 / 255.,
        ]
    }

    /// Render one string through `ctx` into a `tw`x`th` RGBA target and read it back.
    fn render_string(
        gpu: &Gpu,
        pool: vk::CommandPool,
        ctx: &mut TextContext,
        text: &str,
        px: f32,
        tw: u32,
        th: u32,
    ) -> Result<Vec<u8>> {
        let atlas = ctx.build_atlas(gpu, pool, text, px)?;
        anyhow::ensure!(!atlas.glyphs.is_empty(), "no glyphs shaped for {text:?}");
        let pixels = render_atlas(gpu, pool, &atlas, (10.0, 8.0), tw, th);
        atlas.texture.destroy(gpu);
        pixels
    }

    /// Draw a prebuilt `atlas` at `origin` into a `tw`x`th` RGBA target and read it back. The
    /// caller owns `atlas` (its texture is not destroyed here).
    fn render_atlas(
        gpu: &Gpu,
        pool: vk::CommandPool,
        atlas: &GlyphAtlas,
        origin: (f32, f32),
        tw: u32,
        th: u32,
    ) -> Result<Vec<u8>> {
        let target = RenderTarget::new(gpu, tw, th)?;
        let set_layout = sampler_set_layout(gpu)?;
        let renderer = TextRenderer::new(gpu, target.render_pass, target.extent(), set_layout)?;

        let sizes = [vk::DescriptorPoolSize::default()
            .ty(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .descriptor_count(1)];
        let dp_ci = vk::DescriptorPoolCreateInfo::default()
            .max_sets(1)
            .pool_sizes(&sizes);
        let desc_pool = unsafe { gpu.device.create_descriptor_pool(&dp_ci, None) }?;
        let alloc = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(desc_pool)
            .set_layouts(std::slice::from_ref(&set_layout));
        let set = unsafe { gpu.device.allocate_descriptor_sets(&alloc) }?[0];
        let img = vk::DescriptorImageInfo::default()
            .sampler(atlas.texture.sampler)
            .image_view(atlas.texture.view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        let write = vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(std::slice::from_ref(&img));
        unsafe { gpu.device.update_descriptor_sets(&[write], &[]) };

        let dims = [tw as f32, th as f32];
        gpu.run_commands(pool, |cbuf| {
            target.begin(gpu, cbuf, unorm(BG));
            renderer.draw(gpu, cbuf, set, atlas, origin, dims, unorm(FG));
            unsafe { gpu.device.cmd_end_render_pass(cbuf) };
        })?;
        let pixels = target.read_back(gpu, pool)?;

        unsafe {
            gpu.device.destroy_descriptor_pool(desc_pool, None);
            gpu.device.destroy_descriptor_set_layout(set_layout, None);
        }
        renderer.destroy(gpu);
        target.destroy(gpu);
        Ok(pixels)
    }

    /// Count pixels close to white — glyph ink over the dark background.
    fn bright(pixels: &[u8]) -> usize {
        pixels
            .chunks_exact(4)
            .filter(|p| p[0] > 150 && p[1] > 150 && p[2] > 150)
            .count()
    }

    /// GPU-free wrapping: short text passes through, long text wraps to width, a `max_lines`
    /// clamp ellipsizes the last line, and no line ever exceeds the wrap width.
    #[test]
    fn wrap_lines_wraps_and_ellipsizes() {
        let px = 15.0;
        assert_eq!(
            wrap_lines_weighted("hello", px, false, 400., 6),
            vec!["hello"]
        );

        let text = "The quick brown fox jumps over the lazy dog and keeps running on through the quiet woods";
        let lines = wrap_lines_weighted(text, px, false, 200., 32);
        assert!(lines.len() > 1, "expected a wrap: {lines:?}");
        for line in &lines {
            assert!(
                measure_line_width(line, px) <= 201.,
                "line too wide: {line:?}"
            );
        }
        // No words lost or reordered across the breaks.
        assert_eq!(
            lines.join(" ").split_whitespace().collect::<Vec<_>>(),
            text.split_whitespace().collect::<Vec<_>>()
        );

        for max in [1, 2] {
            let clamped = wrap_lines_weighted(text, px, false, 200., max);
            assert_eq!(clamped.len(), max, "clamped: {clamped:?}");
            let last = clamped.last().unwrap();
            assert!(last.ends_with('\u{2026}'), "no ellipsis: {clamped:?}");
            for line in &clamped {
                assert!(
                    measure_line_width(line, px) <= 201.,
                    "clamped line too wide: {line:?}"
                );
            }
            // Clamping only truncates: lines before the last match the unclamped wrap.
            assert_eq!(clamped[..max - 1], lines[..max - 1]);
        }
    }

    /// Bidi bodies (untrusted notification content) must not panic the
    /// wrapper: cosmic-text stores a bidi run's glyphs in visual order with
    /// byte-reversed ranges, so a line of pure RTL inside an LTR paragraph
    /// (and the mirrored case) needs min/max range recovery, not first/last.
    #[test]
    fn wrap_lines_survives_bidi_text() {
        let px = 15.0;
        let rtl_in_ltr = format!(
            "From: {}",
            "\u{5e9}\u{5dc}\u{5d5}\u{5dd} \u{5e2}\u{5d5}\u{5dc}\u{5dd} ".repeat(12)
        );
        let ltr_in_rtl = format!(
            "{} see https://example.com/x for details",
            "\u{645}\u{631}\u{62d}\u{628}\u{627} \u{628}\u{627}\u{644}\u{639}\u{627}\u{644}\u{645} ".repeat(8)
        );
        for text in [rtl_in_ltr, ltr_in_rtl] {
            for max in [1, 3, 32] {
                let lines = wrap_lines_weighted(&text, px, false, 120., max);
                assert!(!lines.is_empty());
                assert!(lines.len() <= max.max(1));
            }
        }
    }

    /// The persistent context rasterizes crisp coverage, and reusing it for a second string —
    /// without rebuilding the font system — still produces ink. Guards the compositor's long-lived
    /// `TextContext` against a stale-cache regression. Skips cleanly on a machine with no Vulkan.
    #[test]
    fn text_context_reuse_rasterizes_coverage() {
        let Ok(gpu) = Gpu::new() else {
            eprintln!("no Vulkan device — skipping text_context_reuse_rasterizes_coverage");
            return;
        };
        let pool_ci = vk::CommandPoolCreateInfo::default().queue_family_index(gpu.queue_family);
        let pool = unsafe { gpu.device.create_command_pool(&pool_ci, None) }.unwrap();

        let mut ctx = TextContext::new();

        // First string.
        let a = render_string(&gpu, pool, &mut ctx, "12:34", 26.0, 128, 40).unwrap();
        // The top-left corner is well clear of the glyph origin (10, 8): still background.
        let corner = a[0..4].to_vec();
        for (c, b) in corner.iter().zip(BG.iter()) {
            assert!(
                (*c as i32 - *b as i32).abs() <= 3,
                "bg corner drifted: {corner:?}"
            );
        }
        let a_bright = bright(&a);
        assert!(a_bright > 40, "first string had too little ink: {a_bright}");

        // Second string through the SAME context (font system reused).
        let b = render_string(&gpu, pool, &mut ctx, "Activities", 22.0, 256, 40).unwrap();
        let b_bright = bright(&b);
        assert!(
            b_bright > 60,
            "reused context produced too little ink: {b_bright}"
        );

        unsafe { gpu.device.destroy_command_pool(pool, None) };
    }

    /// A styled, center-aligned, wrapped paragraph lays out over multiple lines with per-span
    /// families/sizes: the glyphs span a real vertical range and render ink in both the top and
    /// bottom thirds. Guards the dialog/notification text path. Skips cleanly with no Vulkan.
    #[test]
    fn build_paragraph_lays_out_styled_lines() {
        let Ok(gpu) = Gpu::new() else {
            eprintln!("no Vulkan device — skipping build_paragraph_lays_out_styled_lines");
            return;
        };
        let pool_ci = vk::CommandPoolCreateInfo::default().queue_family_index(gpu.queue_family);
        let pool = unsafe { gpu.device.create_command_pool(&pool_ci, None) }.unwrap();

        let mut ctx = TextContext::new();
        let spans = [
            TextSpan {
                text: "Run a Command",
                family: SpanFamily::Sans,
                bold: true,
                px: 18.0,
            },
            TextSpan {
                text: "\n\n",
                family: SpanFamily::Sans,
                bold: false,
                px: 14.0,
            },
            TextSpan {
                text: "echo hi",
                family: SpanFamily::Mono,
                bold: false,
                px: 14.0,
            },
            TextSpan {
                text: "\n\n",
                family: SpanFamily::Sans,
                bold: false,
                px: 14.0,
            },
            TextSpan {
                text: "Press ESC to close",
                family: SpanFamily::Sans,
                bold: false,
                px: 11.0,
            },
        ];
        let atlas = ctx
            .build_paragraph(&gpu, pool, &spans, 360.0, 14.0)
            .unwrap();
        assert!(
            !atlas.glyphs.is_empty(),
            "no glyphs shaped for the paragraph"
        );

        let min_y = atlas.glyphs.iter().map(|g| g.y).min().unwrap();
        let max_y = atlas.glyphs.iter().map(|g| g.y + g.h as i32).max().unwrap();
        assert!(
            max_y - min_y > 30,
            "expected a multi-line paragraph, got vertical span {min_y}..{max_y}",
        );

        let tw = 400u32;
        let th = (max_y as u32) + 40;
        let pixels = render_atlas(&gpu, pool, &atlas, (20.0, 10.0), tw, th).unwrap();
        let band_bright = |y0: u32, y1: u32| -> usize {
            let mut n = 0;
            for y in y0..y1 {
                for x in 0..tw {
                    let i = ((y * tw + x) * 4) as usize;
                    if pixels[i] > 150 && pixels[i + 1] > 150 && pixels[i + 2] > 150 {
                        n += 1;
                    }
                }
            }
            n
        };
        let top = band_bright(0, th / 3);
        let bottom = band_bright(2 * th / 3, th);
        assert!(
            top > 10 && bottom > 10,
            "expected ink in the top ({top}) and bottom ({bottom}) thirds (multi-line)",
        );

        atlas.texture.destroy(&gpu);
        unsafe { gpu.device.destroy_command_pool(pool, None) };
    }

    /// Three atlas-capacity invariants of the paragraph path:
    ///  1. A long, high-DPI command line (the pathological run-dialog case) lays out every glyph
    ///     WITHOUT erroring — an errored draw would make the modal dialog vanish while it still
    ///     owns the keyboard. (Dedup usually keeps it inside the 256px default, which is the
    ///     point.)
    ///  2. When there really are more distinct large glyphs than fit 256px, the atlas GROWS past
    ///     256 instead of erroring.
    ///  3. The same glyph repeated 400x dedups to ONE slot (atlas stays 256), yet every instance is
    ///     still placed.
    #[test]
    fn build_paragraph_atlas_capacity() {
        let Ok(gpu) = Gpu::new() else {
            eprintln!("no Vulkan device — skipping build_paragraph_atlas_capacity");
            return;
        };
        let pool_ci = vk::CommandPoolCreateInfo::default().queue_family_index(gpu.queue_family);
        let pool = unsafe { gpu.device.create_command_pool(&pool_ci, None) }.unwrap();

        let mut ctx = TextContext::new();

        // (1) ~180 chars of a real command line at scale-2 sizes: this overflowed
        // the old fixed 256x256 atlas and returned an error; it must lay out now.
        let long = "firefox --new-window https://example.org/a/very/long/path?\
                    q=alpha+beta+gamma&x=1&y=2&z=3#anchor-position-deep-in-the-doc \
                    && echo done && ls -la /usr/share/applications | grep foo";
        let long_spans = [
            TextSpan {
                text: "Run a Command",
                family: SpanFamily::Sans,
                bold: true,
                px: 28.0,
            },
            TextSpan {
                text: "\n\n",
                family: SpanFamily::Sans,
                bold: false,
                px: 28.0,
            },
            TextSpan {
                text: long,
                family: SpanFamily::Mono,
                bold: false,
                px: 28.0,
            },
        ];
        let atlas = ctx
            .build_paragraph(&gpu, pool, &long_spans, 720.0, 28.0)
            .expect("a long command line must lay out, not error");
        assert!(
            atlas.glyphs.len() > 100,
            "expected the whole long line laid out, got {} glyphs",
            atlas.glyphs.len()
        );
        atlas.texture.destroy(&gpu);

        // (2) Force the grow path: the full printable-ASCII set in TWO styles
        // (mono + bold sans) at a large size is ~180 genuinely distinct large
        // glyphs — more than 256x256 holds even after dedup.
        let ascii: String = (0x21u8..=0x7e).map(char::from).collect();
        let grow_spans = [
            TextSpan {
                text: &ascii,
                family: SpanFamily::Mono,
                bold: false,
                px: 44.0,
            },
            TextSpan {
                text: "\n",
                family: SpanFamily::Sans,
                bold: false,
                px: 44.0,
            },
            TextSpan {
                text: &ascii,
                family: SpanFamily::Sans,
                bold: true,
                px: 44.0,
            },
        ];
        let atlas = ctx
            .build_paragraph(&gpu, pool, &grow_spans, 100_000.0, 44.0)
            .expect("a large distinct-glyph set must grow the atlas, not error");
        assert!(
            atlas.side > 256,
            "expected a grown atlas for ~180 distinct large glyphs, got {}",
            atlas.side
        );
        atlas.texture.destroy(&gpu);

        // (3) Dedup: 400 copies of one glyph share one slot, so the atlas stays 256.
        let repeated = "l".repeat(400);
        let dedup_spans = [TextSpan {
            text: &repeated,
            family: SpanFamily::Mono,
            bold: false,
            px: 14.0,
        }];
        let atlas = ctx
            .build_paragraph(&gpu, pool, &dedup_spans, 100_000.0, 14.0)
            .expect("400 identical glyphs must dedup, not overflow");
        assert_eq!(
            atlas.side, 256,
            "identical glyphs should not grow the atlas (side {})",
            atlas.side
        );
        assert_eq!(
            atlas.glyphs.len(),
            400,
            "every instance is still placed, got {}",
            atlas.glyphs.len()
        );
        atlas.texture.destroy(&gpu);

        unsafe { gpu.device.destroy_command_pool(pool, None) };
    }

    /// `build_paragraph` tags every laid-out glyph with its source-span index (parallel to
    /// `glyphs`), so a caller can recover a span's ink rectangle to paint an inline background.
    /// Three left-to-right spans on one line must come back in x-order 0 < 1 < 2, and the
    /// middle span's ink must be a non-empty strict horizontal subset of the whole run.
    #[test]
    fn build_paragraph_tags_spans() {
        let Ok(gpu) = Gpu::new() else {
            eprintln!("no Vulkan device — skipping build_paragraph_tags_spans");
            return;
        };
        let pool_ci = vk::CommandPoolCreateInfo::default().queue_family_index(gpu.queue_family);
        let pool = unsafe { gpu.device.create_command_pool(&pool_ci, None) }.unwrap();

        let mut ctx = TextContext::new();
        let spans = [
            TextSpan {
                text: "AAAA ",
                family: SpanFamily::Sans,
                bold: false,
                px: 20.0,
            },
            TextSpan {
                text: "MMMM",
                family: SpanFamily::Mono,
                bold: true,
                px: 20.0,
            },
            TextSpan {
                text: " ZZZZ",
                family: SpanFamily::Sans,
                bold: false,
                px: 20.0,
            },
        ];
        let atlas = ctx
            .build_paragraph(&gpu, pool, &spans, 100_000.0, 20.0)
            .unwrap();
        assert_eq!(
            atlas.spans.len(),
            atlas.glyphs.len(),
            "spans must be parallel to glyphs"
        );

        // Horizontal ink extent of the glyphs tagged with a given span index.
        let span_x = |idx: u32| -> Option<(i32, i32)> {
            atlas
                .glyphs
                .iter()
                .zip(&atlas.spans)
                .filter(|(_, s)| **s == idx)
                .map(|(g, _)| (g.x, g.x + g.w as i32))
                .reduce(|(a0, a1), (b0, b1)| (a0.min(b0), a1.max(b1)))
        };
        let s0 = span_x(0).expect("span 0 has ink");
        let s1 = span_x(1).expect("span 1 has ink");
        let s2 = span_x(2).expect("span 2 has ink");

        assert!(
            s0.1 <= s1.0,
            "span 0 must end before span 1 ({s0:?} vs {s1:?})"
        );
        assert!(
            s1.1 <= s2.0,
            "span 1 must end before span 2 ({s1:?} vs {s2:?})"
        );
        // Middle span is a proper subset of the whole run's horizontal extent.
        assert!(
            s0.0 < s1.0 && s1.1 < s2.1,
            "span 1 ink {s1:?} must be inside the run ({s0:?}..{s2:?})"
        );

        atlas.texture.destroy(&gpu);
        unsafe { gpu.device.destroy_command_pool(pool, None) };
    }
}
