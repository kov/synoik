//! The GNOME top panel.
//!
//! A persistent bar drawn in-compositor at the top of each output while the
//! session is in GNOME (floating) windowing mode. This first slice draws the
//! bar chrome, a left "Activities" button (clickable, toggles the overview) and
//! a centered clock; the panel also reserves a top strut so windows never sit
//! under it (see `layout::workspace::compute_working_area`).
//!
//! The bar is drawn entirely on the GPU through the owned Vulkan renderer: an
//! offscreen `VkTexture` is cleared to the bar background, the Activities/clock
//! glyph runs are drawn with the [`render_glyphs`](VulkanFrame::render_glyphs)
//! material, and the result is composited as a `TextureRenderElement` — no
//! cairo/pango raster, no CPU upload. The panel's *logical* state (the clock
//! string, whether Activities is checked, the hit rectangles) is kept separate
//! from that render path so headless tests can assert it without a GPU.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ptr::null_mut;

use ordered_float::NotNan;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::{
    Bind, Color32F, ContextId, Frame as _, Offscreen, Renderer, Texture,
};
use smithay::output::Output;
use smithay::utils::{Buffer as BufferCoord, Logical, Physical, Point, Rectangle, Size, Transform};

use crate::render_helpers::renderer::OffscreenRenderer;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::utils::{output_size, to_physical_precise_round};

/// Logical height of the panel. GNOME's is `2.2em` at an `11pt` base font,
/// i.e. ~32px at scale 1 (`gnome-shell-sass/widgets/_panel.scss`).
pub const PANEL_HEIGHT: f64 = 32.;

/// Panel font size in logical pixels-per-em. Scaled by the output scale to the
/// physical em the glyph rasterizer shapes at.
const FONT_PX: f64 = 13.;

/// Horizontal padding inside the Activities button, logical px.
const H_PADDING: f64 = 12.;

/// Label of the left-hand overview toggle.
const ACTIVITIES: &str = "Activities";

/// Bar background (opaque black — GNOME's dark panel), straight RGBA.
const BAR_BG: [f32; 4] = [0., 0., 0., 1.];

/// The Activities checked highlight (white at 0.15 over the black bar), pre-mixed
/// to the opaque grey it composites to so a single opaque clear draws it.
const HIGHLIGHT: [f32; 4] = [0.15, 0.15, 0.15, 1.];

/// Text color (opaque white); the glyph coverage modulates the alpha.
const TEXT: [f32; 4] = [1., 1., 1., 1.];

/// A clickable region of the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelItem {
    /// The left-hand button that toggles the overview.
    Activities,
}

/// Cached bar textures keyed by (fractional scale, physical width). The panel
/// spans the output width, so width is part of the key (unlike the fixed-size
/// overlays, which key by scale alone). Tied to a renderer context: dropped
/// wholesale when the renderer changes.
struct BarCache {
    context: Option<ContextId<VkTexture>>,
    textures: HashMap<(NotNan<f64>, i32), VkTexture>,
}

impl BarCache {
    fn new() -> Self {
        Self {
            context: None,
            textures: HashMap::new(),
        }
    }

    /// Drop every cached bar (content or renderer changed).
    fn clear(&mut self) {
        self.textures.clear();
    }
}

pub struct Panel {
    /// Current clock string, e.g. "14:30". Recomputed on the minute tick.
    clock_text: String,
    /// Whether the overview is open (drives the Activities checked highlight).
    activities_checked: bool,

    /// Hit rectangle of the Activities button, in output-local logical coords.
    /// Left-anchored, so it is the same on every output.
    activities_rect: Rectangle<f64, Logical>,

    /// Cached GPU bar textures, cleared whenever the drawn content (clock text
    /// or checked state) changes.
    cache: RefCell<BarCache>,
}

impl Panel {
    pub fn new() -> Self {
        // Measure the label GPU-free (cosmic-text shaping, no renderer needed) so the hit
        // rectangle is known at construction time — the same width the bar draws the run to.
        let activities_w = activities_logical_width();
        let activities_rect = Rectangle::new(
            Point::from((0., 0.)),
            Size::from((activities_w, PANEL_HEIGHT)),
        );

        Self {
            clock_text: format_clock(unsafe { libc::time(null_mut()) }),
            activities_checked: false,
            activities_rect,
            cache: RefCell::new(BarCache::new()),
        }
    }

    /// Recompute the clock from the wall clock. Returns whether it changed (so
    /// the caller can queue a redraw). `now` is epoch seconds — injectable so
    /// tests are deterministic.
    pub fn update_clock_at(&mut self, now: libc::time_t) -> bool {
        let text = format_clock(now);
        if text != self.clock_text {
            self.clock_text = text;
            self.cache.borrow_mut().clear();
            true
        } else {
            false
        }
    }

    /// Recompute the clock from the current wall-clock time.
    pub fn update_clock(&mut self) -> bool {
        self.update_clock_at(unsafe { libc::time(null_mut()) })
    }

    /// Reflect the overview open/closed state on the Activities button.
    pub fn set_overview_open(&mut self, open: bool) {
        if open != self.activities_checked {
            self.activities_checked = open;
            self.cache.borrow_mut().clear();
        }
    }

    /// The current clock string (for tests / introspection).
    pub fn clock_text(&self) -> &str {
        &self.clock_text
    }

    /// Whether the Activities button is highlighted (for tests / introspection).
    pub fn activities_checked(&self) -> bool {
        self.activities_checked
    }

    /// Which panel item, if any, sits at an output-local logical position.
    pub fn hit_test(&self, pos: Point<f64, Logical>) -> Option<PanelItem> {
        if self.activities_rect.contains(pos) {
            Some(PanelItem::Activities)
        } else {
            None
        }
    }

    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        output: &Output,
    ) -> Option<TextureRenderElement<VkTexture>> {
        let scale = output.current_scale().fractional_scale();
        let width = output_size(output).w;
        let width_px: i32 = to_physical_precise_round(scale, width);
        let width_px = width_px.max(1);
        let key = (NotNan::new(scale).ok()?, width_px);

        let texture = {
            let mut cache = self.cache.borrow_mut();

            // The cached textures belong to one renderer context; drop them all if it changed.
            let context = renderer.context_id();
            if cache.context.as_ref() != Some(&context) {
                cache.clear();
                cache.context = Some(context);
            }

            // Not the `entry` API: the build is fallible and borrows `renderer`, which the
            // closure-based `or_insert_with` can't express.
            #[allow(clippy::map_entry)]
            if !cache.textures.contains_key(&key) {
                match draw_bar_texture(
                    renderer,
                    scale,
                    width_px,
                    &self.clock_text,
                    self.activities_checked,
                ) {
                    Ok(texture) => {
                        cache.textures.insert(key, texture);
                    }
                    Err(err) => {
                        tracing::error!("error drawing the panel bar: {err:#}");
                        return None;
                    }
                }
            }
            cache.textures.get(&key)?.clone()
        };

        // The whole bar is opaque, so let the compositor skip drawing behind it.
        let opaque = vec![Rectangle::from_size(texture.size())];
        let buffer =
            TextureBuffer::from_texture(renderer, texture, scale, Transform::Normal, opaque);

        let elem = TextureRenderElement::from_texture_buffer(
            buffer,
            Point::from((0., 0.)),
            1.,
            None,
            None,
            Kind::Unspecified,
        );
        Some(elem)
    }
}

impl Default for Panel {
    fn default() -> Self {
        Self::new()
    }
}

/// Seconds until the next wall-clock minute boundary (1..=60), so the clock
/// tick can align to when the displayed minute actually changes.
pub fn secs_until_next_minute() -> u64 {
    // SAFETY: read the static tm immediately and copy the one field out.
    let sec = unsafe {
        let now = libc::time(null_mut());
        let tm = libc::localtime(&now);
        if tm.is_null() {
            0
        } else {
            (*tm).tm_sec
        }
    };
    (60 - i64::from(sec)).clamp(1, 60) as u64
}

/// Format epoch seconds as a local `HH:MM` string.
fn format_clock(now: libc::time_t) -> String {
    // SAFETY: localtime returns a pointer into a static buffer; we read it
    // immediately and copy the fields out before any other libc time call.
    unsafe {
        let tm = libc::localtime(&now);
        if tm.is_null() {
            return String::new();
        }
        let tm = &*tm;
        format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
    }
}

/// The Activities button's logical width: the shaped label plus a horizontal
/// padding on each side. Measured GPU-free (cosmic-text shaping) so it is the
/// same at construction time (the hit rectangle) and at draw time (the checked
/// highlight), matching what the bar draws the glyph run to.
fn activities_logical_width() -> f64 {
    niri_vk::text::measure_line_width(ACTIVITIES, FONT_PX as f32) + H_PADDING * 2.
}

/// Draw the whole bar into an offscreen [`VkTexture`] on the GPU: clear the
/// opaque background, draw the Activities checked highlight, then the Activities
/// and clock glyph runs. The returned texture is `SHADER_READ_ONLY` (sampleable)
/// so the caller can composite it directly.
fn draw_bar_texture(
    renderer: &mut VulkanRenderer,
    scale: f64,
    width_px: i32,
    clock: &str,
    checked: bool,
) -> anyhow::Result<VkTexture> {
    let _span = tracy_client::span!("panel::draw_bar_texture");

    let width_px = width_px.max(1);
    let height_px: i32 = to_physical_precise_round(scale, PANEL_HEIGHT);
    let height_px = height_px.max(1);
    let px = (FONT_PX * scale) as f32;

    // Shape both runs (reuses the renderer's long-lived font system).
    let activities = renderer.build_glyph_run(ACTIVITIES, px)?;
    let clock_run = renderer.build_glyph_run(clock, px)?;

    let h_padding: i32 = to_physical_precise_round(scale, H_PADDING);

    // Left-align Activities to the padding, vertically centered on its ink.
    let (a_ix, a_iy, _a_iw, a_ih) = activities.ink_bounds();
    let a_origin = Point::<i32, Physical>::from((h_padding - a_ix, (height_px - a_ih) / 2 - a_iy));

    // Center the clock's ink in the bar.
    let (c_ix, c_iy, c_iw, c_ih) = clock_run.ink_bounds();
    let c_origin =
        Point::<i32, Physical>::from(((width_px - c_iw) / 2 - c_ix, (height_px - c_ih) / 2 - c_iy));

    // The highlight matches the Activities button hit rectangle (label + padding),
    // measured GPU-free so it agrees with `activities_rect`.
    let highlight_w: i32 = to_physical_precise_round(scale, activities_logical_width());
    let highlight_w = highlight_w.clamp(1, width_px);

    let size = Size::<i32, Physical>::from((width_px, height_px));
    let mut target = renderer.create_buffer(
        Fourcc::Abgr8888,
        Size::<i32, BufferCoord>::from((width_px, height_px)),
    )?;

    {
        let mut fb = renderer.bind(&mut target)?;
        let mut frame = renderer.render(&mut fb, size, Transform::Normal)?;
        let full = Rectangle::from_size(size);

        frame.clear(Color32F::from(BAR_BG), &[full])?;
        if checked {
            let hl = Rectangle::new(Point::from((0, 0)), Size::from((highlight_w, height_px)));
            frame.clear(Color32F::from(HIGHLIGHT), &[hl])?;
        }
        frame.render_glyphs(&activities, a_origin, TEXT, full, &[full])?;
        frame.render_glyphs(&clock_run, c_origin, TEXT, full, &[full])?;
        // finish() submits and fence-waits synchronously, so the sync point is already signaled.
        let _sync = frame.finish()?;
    }

    // The bar is sampled by its own render element; transition it to shader-read.
    renderer.make_offscreen_sampleable(&target)?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use smithay::backend::renderer::ExportMem;

    use super::*;

    /// Drive the GPU bar into an offscreen and read it back: it must have an
    /// opaque dark background, the checked highlight on the left, and bright
    /// glyph ink for the clock/Activities text. Skips cleanly with no device.
    #[test]
    fn draws_a_bar_with_glyph_coverage() {
        let mut vk = match VulkanRenderer::new() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping draws_a_bar_with_glyph_coverage: no Vulkan device ({e})");
                return;
            }
        };

        let width_px = 400;
        let height_px = PANEL_HEIGHT as i32;
        let mut tex = draw_bar_texture(&mut vk, 1., width_px, "12:34", true).expect("bar texture");

        let fb = vk.bind(&mut tex).expect("bind for readback");
        let region = Rectangle::<i32, BufferCoord>::from_size(Size::from((width_px, height_px)));
        let mapping = vk
            .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
            .expect("copy_framebuffer");
        let pixels = vk.map_texture(&mapping).expect("map_texture").to_vec();

        let px_at = |x: i32, y: i32| -> [u8; 4] {
            let i = ((y * width_px + x) * 4) as usize;
            [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
        };

        // A pixel deep in the right half (away from any text) is the opaque dark bar.
        let bg = px_at(width_px - 4, height_px / 2);
        assert_eq!(bg[3], 255, "the bar must be opaque, got {bg:?}");
        assert!(
            bg[0] < 40 && bg[1] < 40 && bg[2] < 40,
            "bar bg not dark: {bg:?}"
        );

        // The checked highlight tints the top-left corner grey (brighter than black).
        let hl = px_at(2, 2);
        assert!(
            hl[0] > 20 && hl[0] < 80,
            "expected the checked highlight grey at top-left, got {hl:?}",
        );

        // Bright glyph ink somewhere (the clock and Activities text).
        let bright = pixels
            .chunks_exact(4)
            .filter(|p| p[0] > 150 && p[1] > 150 && p[2] > 150)
            .count();
        assert!(bright > 40, "expected visible glyph ink, got {bright}");
    }
}
