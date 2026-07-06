//! The GNOME top panel.
//!
//! A persistent bar drawn in-compositor at the top of each output while the
//! session is in GNOME (floating) windowing mode. This first slice draws the
//! bar chrome, a left "Activities" button (clickable, toggles the overview) and
//! a centered clock; the panel also reserves a top strut so windows never sit
//! under it (see `layout::workspace::compute_working_area`).
//!
//! Rendering mirrors the other in-compositor overlays (see
//! `config_error_notification.rs`): pango/cairo paints the bar into an
//! `ImageSurface`, which is uploaded to a `TextureBuffer` and wrapped in a
//! `PrimaryGpuTextureRenderElement`. The panel's *logical* state (the clock
//! string, whether Activities is checked, the hit rectangles) is kept separate
//! from that render path so headless tests can assert it without a GPU.

use std::cell::RefCell;
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::ptr::null_mut;

use ordered_float::NotNan;
use pangocairo::cairo::{self, ImageSurface};
use pangocairo::pango::FontDescription;
use smithay::backend::renderer::element::Kind;
use smithay::output::Output;
use smithay::reexports::gbm::Format as Fourcc;
use smithay::utils::{Logical, Point, Rectangle, Size, Transform};

use crate::render_helpers::memory::MemoryBuffer;
use crate::render_helpers::renderer::NiriRenderer;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::utils::{output_size, to_physical_precise_round};

/// Logical height of the panel. GNOME's is `2.2em` at an `11pt` base font,
/// i.e. ~32px at scale 1 (`gnome-shell-sass/widgets/_panel.scss`).
pub const PANEL_HEIGHT: f64 = 32.;

/// Panel font. Absolute px so it scales cleanly with the output scale (the same
/// approach as `config_error_notification`); sized to sit comfortably in the bar.
const FONT: &str = "sans 13px";

/// Horizontal padding inside the Activities button, logical px.
const H_PADDING: f64 = 12.;

/// Label of the left-hand overview toggle.
const ACTIVITIES: &str = "Activities";

/// A clickable region of the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelItem {
    /// The left-hand button that toggles the overview.
    Activities,
}

/// Cached bar textures keyed by (fractional scale, physical width). The panel
/// spans the output width, so width is part of the key (unlike the fixed-size
/// overlays, which key by scale alone).
type ScaledBuffers = HashMap<(NotNan<f64>, i32), Option<MemoryBuffer>>;

pub struct Panel {
    /// Current clock string, e.g. "14:30". Recomputed on the minute tick.
    clock_text: String,
    /// Whether the overview is open (drives the Activities checked highlight).
    activities_checked: bool,

    /// Hit rectangle of the Activities button, in output-local logical coords.
    /// Left-anchored, so it is the same on every output.
    activities_rect: Rectangle<f64, Logical>,

    /// Cached textures, cleared whenever the drawn content (clock text or
    /// checked state) changes.
    buffers: RefCell<ScaledBuffers>,
}

impl Panel {
    pub fn new() -> Self {
        let activities_w = measure_logical_width(ACTIVITIES) + H_PADDING * 2.;
        let activities_rect = Rectangle::new(
            Point::from((0., 0.)),
            Size::from((activities_w, PANEL_HEIGHT)),
        );

        Self {
            clock_text: format_clock(unsafe { libc::time(null_mut()) }),
            activities_checked: false,
            activities_rect,
            buffers: RefCell::new(HashMap::new()),
        }
    }

    /// Recompute the clock from the wall clock. Returns whether it changed (so
    /// the caller can queue a redraw). `now` is epoch seconds — injectable so
    /// tests are deterministic.
    pub fn update_clock_at(&mut self, now: libc::time_t) -> bool {
        let text = format_clock(now);
        if text != self.clock_text {
            self.clock_text = text;
            self.buffers.borrow_mut().clear();
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
            self.buffers.borrow_mut().clear();
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

    pub fn render<R: NiriRenderer>(
        &self,
        renderer: &mut R,
        output: &Output,
    ) -> Option<TextureRenderElement<R::TextureId>> {
        let scale = output.current_scale().fractional_scale();
        let width = output_size(output).w;
        let width_px = to_physical_precise_round(scale, width);
        let key = (NotNan::new(scale).unwrap(), width_px);

        let mut buffers = self.buffers.borrow_mut();
        let buffer = buffers.entry(key).or_insert_with(|| {
            // The bar renders CPU-side into a renderer-neutral buffer; degrade a paint panic to a
            // missing bar rather than aborting scanout.
            std::panic::catch_unwind(AssertUnwindSafe(|| {
                render_bar(scale, width_px, &self.clock_text, self.activities_checked).ok()
            }))
            .unwrap_or_else(|_| {
                tracing::error!("panic while painting the panel");
                None
            })
        });
        let buffer = buffer.as_ref()?;

        // Upload the CPU-rendered bar through the active renderer's `ImportMem`, so it draws on any
        // renderer (including the owned Vulkan one) rather than only GLES.
        let buffer: TextureBuffer<R::TextureId> =
            TextureBuffer::from_memory_buffer(renderer, buffer).ok()?;

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

/// Measure a string's width at logical scale (for hit rectangles), in px.
fn measure_logical_width(text: &str) -> f64 {
    let Ok(surface) = ImageSurface::create(cairo::Format::ARgb32, 0, 0) else {
        return 0.;
    };
    let Ok(cr) = cairo::Context::new(&surface) else {
        return 0.;
    };
    let layout = pangocairo::functions::create_layout(&cr);
    layout.set_font_description(Some(&make_font(1.)));
    layout.set_text(text);
    f64::from(layout.pixel_size().0)
}

/// The panel font at a given scale (absolute px, scaled like the other overlays).
fn make_font(scale: f64) -> FontDescription {
    let mut font = FontDescription::from_string(FONT);
    font.set_absolute_size(to_physical_precise_round(scale, font.size()));
    font
}

/// Paint the whole bar into a texture: opaque black background, left Activities
/// button, centered clock.
fn render_bar(
    scale: f64,
    width_px: i32,
    clock: &str,
    checked: bool,
) -> anyhow::Result<MemoryBuffer> {
    let _span = tracy_client::span!("panel::render_bar");

    let width_px = width_px.max(1);
    let height_px: i32 = to_physical_precise_round(scale, PANEL_HEIGHT);
    let height_px = height_px.max(1);
    let surface = draw_bar(scale, width_px, height_px, clock, checked)?;

    let data = surface.take_data().unwrap();
    let buffer = MemoryBuffer::new(
        data.to_vec(),
        Fourcc::Argb8888,
        (width_px, height_px),
        scale,
        Transform::Normal,
    );

    Ok(buffer)
}

/// Draw the bar into a CPU cairo surface (no GPU). Split out of `render_bar` so
/// the pango/cairo drawing can be exercised in headless tests.
fn draw_bar(
    scale: f64,
    width_px: i32,
    height_px: i32,
    clock: &str,
    checked: bool,
) -> anyhow::Result<ImageSurface> {
    let h_padding: i32 = to_physical_precise_round(scale, H_PADDING);
    let font = make_font(scale);

    let surface = ImageSurface::create(cairo::Format::ARgb32, width_px, height_px)?;
    let cr = cairo::Context::new(&surface)?;

    // Opaque panel background (dark theme: #000000).
    cr.set_source_rgb(0., 0., 0.);
    cr.paint()?;

    // Left: the Activities button.
    let activities = pangocairo::functions::create_layout(&cr);
    activities.context().set_round_glyph_positions(false);
    activities.set_font_description(Some(&font));
    activities.set_text(ACTIVITIES);
    let (aw, ah) = activities.pixel_size();

    if checked {
        // Subtle highlight while the overview is open.
        cr.set_source_rgba(1., 1., 1., 0.15);
        cr.rectangle(0., 0., f64::from(aw + h_padding * 2), f64::from(height_px));
        cr.fill()?;
    }

    cr.move_to(f64::from(h_padding), f64::from((height_px - ah) / 2));
    cr.set_source_rgb(1., 1., 1.);
    pangocairo::functions::show_layout(&cr, &activities);

    // Center: the clock.
    let clock_layout = pangocairo::functions::create_layout(&cr);
    clock_layout.context().set_round_glyph_positions(false);
    clock_layout.set_font_description(Some(&font));
    clock_layout.set_text(clock);
    let (cw, ch) = clock_layout.pixel_size();
    cr.move_to(
        f64::from((width_px - cw) / 2),
        f64::from((height_px - ch) / 2),
    );
    cr.set_source_rgb(1., 1., 1.);
    pangocairo::functions::show_layout(&cr, &clock_layout);

    drop(cr);

    Ok(surface)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exercise the real pango/cairo drawing headlessly (no GPU): the bar must
    /// be the right size, fully opaque, and actually render bright text pixels.
    #[test]
    fn draws_an_opaque_bar_with_text() {
        let width = 400;
        let height: i32 = PANEL_HEIGHT as i32;
        let mut surface = draw_bar(1., width, height, "12:34", false).unwrap();

        assert_eq!(surface.width(), width);
        assert_eq!(surface.height(), height);

        let stride = surface.stride() as usize;
        let data = surface.data().unwrap();
        let mut all_opaque = true;
        let mut any_bright = false;
        for y in 0..height as usize {
            for x in 0..width as usize {
                let px = y * stride + x * 4;
                // cairo ARgb32 is native-endian; on little-endian: [B, G, R, A].
                if data[px + 3] != 255 {
                    all_opaque = false;
                }
                if data[px] > 200 && data[px + 1] > 200 && data[px + 2] > 200 {
                    any_bright = true;
                }
            }
        }
        assert!(all_opaque, "the panel background must be fully opaque");
        assert!(
            any_bright,
            "the clock/Activities text must draw bright pixels"
        );
    }
}
