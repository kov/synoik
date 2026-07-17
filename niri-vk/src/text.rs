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

use anyhow::{Context, Result};
use ash::vk;
use cosmic_text::{Attrs, Buffer, Family, FontSystem, Hinting, Metrics, Shaping};
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
    pub side: u32,
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

    /// Shape `text` at `px` pixels-per-em, rasterize hinted, and pack into an R8 coverage atlas,
    /// reusing this context's font system and scaler cache.
    pub fn build_atlas(
        &mut self,
        gpu: &Gpu,
        pool: vk::CommandPool,
        text: &str,
        px: f32,
    ) -> Result<GlyphAtlas> {
        let fonts = &mut self.fonts;
        let ctx = &mut self.scale;

        let mut buffer = Buffer::new(fonts, Metrics::new(px, (px * 1.25).round()));
        buffer.set_hinting(Hinting::Enabled);
        {
            let mut b = buffer.borrow_with(fonts);
            b.set_size(None, None);
            let attrs = Attrs::new().family(Family::SansSerif);
            b.set_text(text, &attrs, Shaping::Advanced, None);
            b.shape_until_scroll(false);
        }

        let side: u32 = 256;
        let mut atlas_pixels = vec![0u8; (side * side) as usize];
        let mut atlas = AtlasAllocator::new(size2(side as i32, side as i32));
        let mut glyphs = Vec::new();

        for run in buffer.layout_runs() {
            let baseline = run.line_y.round() as i32;
            for glyph in run.glyphs {
                // Whole-pixel origin: round X, then physical() truncates Y (its own hinting).
                let mut lg = glyph.clone();
                lg.x = lg.x.round();
                let phys = lg.physical((0.0, 0.0), 1.0);
                let key = phys.cache_key;

                // Same font cosmic-text shaped with (get_font promotes File->SharedFile).
                let Some(font) = fonts.get_font(key.font_id, key.font_weight) else {
                    continue;
                };
                let mut scaler = ctx
                    .builder(font.as_swash())
                    .size(f32::from_bits(key.font_size_bits))
                    .hint(true)
                    .build();

                // Subpixel remainder from the cache key (0 once fully snapped).
                let offset = Vector::new(key.x_bin.as_float(), key.y_bin.as_float());
                let rendered = Render::new(&[Source::Outline])
                    .format(Format::Alpha)
                    .offset(offset)
                    .render(&mut scaler, key.glyph_id);
                let Some(image) = rendered else { continue };

                let (w, h) = (image.placement.width, image.placement.height);
                if w == 0 || h == 0 {
                    continue; // whitespace: no bitmap, pen still advances via the next glyph
                }
                debug_assert!(matches!(image.content, Content::Mask));

                // +1px padding around each slot to keep neighbours from bleeding.
                let alloc = atlas
                    .allocate(size2(w as i32 + 1, h as i32 + 1))
                    .context("glyph atlas full")?;
                let (ax, ay) = (alloc.rectangle.min.x as u32, alloc.rectangle.min.y as u32);
                for row in 0..h {
                    let src = &image.data[(row * w) as usize..((row + 1) * w) as usize];
                    let dst = ((ay + row) * side + ax) as usize;
                    atlas_pixels[dst..dst + w as usize].copy_from_slice(src);
                }

                glyphs.push(PlacedGlyph {
                    x: phys.x + image.placement.left,
                    y: baseline + phys.y - image.placement.top,
                    w,
                    h,
                    atlas_x: ax,
                    atlas_y: ay,
                });
            }
        }

        // 1:1 glyph-px to screen-px with integer placement, so NEAREST sampling is pixel-exact.
        let texture =
            Texture::from_coverage(gpu, pool, side, side, &atlas_pixels, vk::Filter::NEAREST)?;
        Ok(GlyphAtlas {
            texture,
            glyphs,
            side,
        })
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
            renderer.draw(gpu, cbuf, set, &atlas, (10.0, 8.0), dims, unorm(FG));
            unsafe { gpu.device.cmd_end_render_pass(cbuf) };
        })?;
        let pixels = target.read_back(gpu, pool)?;

        unsafe {
            gpu.device.destroy_descriptor_pool(desc_pool, None);
            gpu.device.destroy_descriptor_set_layout(set_layout, None);
        }
        renderer.destroy(gpu);
        atlas.texture.destroy(gpu);
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
}
