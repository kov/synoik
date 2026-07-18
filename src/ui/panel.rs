//! The GNOME top panel.
//!
//! A persistent bar drawn in-compositor at the top of each output while the
//! session is in GNOME (floating) windowing mode. It draws the bar chrome, a
//! left-hand **workspace indicator** (the graphical dots that replaced GNOME's
//! old "Activities" text button — click toggles the overview, scroll switches
//! workspace) and a centered clock; the panel also reserves a top strut so
//! windows never sit under it (see `layout::workspace::compute_working_area`).
//!
//! The bar is drawn entirely on the GPU through the owned Vulkan renderer: an
//! offscreen `VkTexture` is cleared to the bar background, the clock glyph run
//! is drawn with the [`render_glyphs`](VulkanFrame::render_glyphs) material, and
//! the result is composited as a `TextureRenderElement` — no cairo/pango raster.
//! The workspace dots are a small CPU-rasterized bitmap (a capsule distance
//! field, like the screenshot shutter) composited as a second element on top of
//! the bar, because rounded shapes don't render reliably inside a hand-bound
//! offscreen.
//!
//! ## Extension-representable structure
//!
//! The panel's *logical* model — which named items live in which of the three
//! boxes, and each item's screen rectangle and state — is kept separate from the
//! per-frame render path. GNOME extensions address the panel through exactly this
//! surface (`Main.panel.statusArea[role]`, the left/center/right boxes), so we
//! model it the same way ([`PanelBox`] / [`PanelItem`], roles [`ROLE_ACTIVITIES`]
//! and [`ROLE_DATE_MENU`]) even though the extension host itself is deferred. The
//! goal is a stable role→box→rect map an extension host can bind to, *not* a
//! widget tree; rendering consumes the model but never the other way around.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ptr::null_mut;
use std::time::Duration;

use ordered_float::NotNan;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::{
    Bind, Color32F, ContextId, Frame as _, Offscreen, Renderer, Texture,
};
use smithay::output::Output;
use smithay::utils::{
    Buffer as BufferCoord, Logical, Physical, Point, Rectangle, Scale, Size, Transform,
};

use crate::gnome::ClockFormat;
use crate::render_helpers::memory::MemoryBuffer;
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

/// Base workspace-dot diameter, logical px. GNOME: `$scalable_icon_size (16) * 0.5`
/// (`gnome-shell-sass/widgets/_panel.scss`), fully rounded (`$forced_circular_radius`).
const DOT_DIAMETER: f64 = 8.;

/// Gap between dots, logical px (`panel.js` `WorkspaceIndicators` box `spacing`).
const DOT_SPACING: f64 = 5.;

/// Horizontal padding on each side of the whole dot row, logical px. GNOME's dot
/// box has `0 $scaled_padding*0.5` (3px); we keep a little extra so the row isn't
/// jammed against the screen edge and stays a comfortable click target.
const INDICATOR_H_PADDING: f64 = 6.;

/// Inactive dots are drawn at 0.75× and half-opacity (`panel.js`
/// `INACTIVE_WORKSPACE_DOT_SCALE`, `WorkspaceDot._updateVisuals`).
const INACTIVE_DOT_SCALE: f64 = 0.75;
const INACTIVE_DOT_OPACITY: f64 = 0.5;

/// Horizontal padding on each side of the dateMenu (clock) button, logical px.
const H_PADDING: f64 = 12.;

/// Bar background (opaque black — GNOME's dark panel), straight RGBA.
const BAR_BG: [f32; 4] = [0., 0., 0., 1.];

/// The checked highlight (white at 0.15 over the black bar), pre-mixed to the
/// opaque grey it composites to so a single opaque clear draws it.
const HIGHLIGHT: [f32; 4] = [0.15, 0.15, 0.15, 1.];

/// Text color (opaque white); the glyph coverage modulates the alpha.
const TEXT: [f32; 4] = [1., 1., 1., 1.];

/// Role of the left-hand workspace indicator (GNOME's `activities` panel role).
pub const ROLE_ACTIVITIES: &str = "activities";
/// Role of the centered clock (GNOME's `dateMenu` panel role).
pub const ROLE_DATE_MENU: &str = "dateMenu";

/// One of the panel's three boxes, mirroring GNOME's `_leftBox`/`_centerBox`/`_rightBox`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelBox {
    Left,
    Center,
    Right,
}

/// A named panel component and where it currently sits. The addressable surface a
/// future extension host binds to (see the module docs); rendering is separate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PanelItem {
    /// Stable GNOME role name, e.g. [`ROLE_ACTIVITIES`].
    pub role: &'static str,
    /// Which box the item lives in.
    pub r#box: PanelBox,
    /// The item's rectangle in output-local logical coords.
    pub rect: Rectangle<f64, Logical>,
}

/// Per-output workspace snapshot that drives the dot indicator (GNOME's
/// `WorkspacesAdjustment`: `count` = `upper`, `active` = `value`). The panel is a
/// single global object rendered per output, so the caller passes the snapshot
/// for the output being drawn/hit-tested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceState {
    pub count: usize,
    pub active: usize,
}

impl WorkspaceState {
    /// Clamp `active` into range so an out-of-range index never drops the wide pill.
    fn active(self) -> usize {
        self.active.min(self.count.saturating_sub(1))
    }
}

/// The dot row's `widthMultiplier` (`panel.js` `WorkspaceIndicators._updateExpansion`):
/// the active pill is this many base-diameters wide.
fn width_multiplier(count: usize) -> f64 {
    if count <= 2 {
        3.625
    } else if count <= 5 {
        3.25
    } else {
        2.75
    }
}

/// Logical width of the whole indicator button (padding + dots + gaps). Independent
/// of *which* dot is active (exactly one is the wide pill), so hit rect, checked
/// highlight, and the drawn bitmap all agree.
fn indicator_logical_width(count: usize) -> f64 {
    if count == 0 {
        return 2. * INDICATOR_H_PADDING;
    }
    let mult = width_multiplier(count);
    let dots = (count as f64 - 1.) * DOT_DIAMETER + DOT_DIAMETER * mult;
    let gaps = (count as f64 - 1.) * DOT_SPACING;
    2. * INDICATOR_H_PADDING + dots + gaps
}

/// Cached bar textures and the uploaded dot-strip, keyed so a content change
/// misses. Tied to a renderer context: dropped wholesale when the renderer changes.
struct BarCache {
    context: Option<ContextId<VkTexture>>,
    /// Bar chrome keyed by (scale, physical width, workspace count) — the count
    /// sets the checked-highlight width.
    textures: HashMap<(NotNan<f64>, i32, usize), VkTexture>,
    /// The workspace-dot strip, keyed by (scale, count, active).
    indicator: HashMap<(NotNan<f64>, usize, usize), TextureBuffer<VkTexture>>,
}

impl BarCache {
    fn new() -> Self {
        Self {
            context: None,
            textures: HashMap::new(),
            indicator: HashMap::new(),
        }
    }

    /// Drop everything cached (content or renderer changed).
    fn clear(&mut self) {
        self.textures.clear();
        self.indicator.clear();
    }
}

pub struct Panel {
    /// Current clock string, e.g. "14:30". Recomputed on each clock tick.
    clock_text: String,
    /// How the clock label is formatted (from `org.gnome.desktop.interface`).
    clock_format: ClockFormat,
    /// Whether the overview is open (drives the indicator's checked highlight).
    activities_checked: bool,

    /// Cached GPU chrome, cleared whenever the drawn content changes.
    cache: RefCell<BarCache>,
}

impl Panel {
    pub fn new() -> Self {
        let clock_format = ClockFormat::default();
        Self {
            clock_text: format_clock(unsafe { libc::time(null_mut()) }, clock_format),
            clock_format,
            activities_checked: false,
            cache: RefCell::new(BarCache::new()),
        }
    }

    /// Recompute the clock from the wall clock. Returns whether it changed (so
    /// the caller can queue a redraw). `now` is epoch seconds — injectable so
    /// tests are deterministic.
    pub fn update_clock_at(&mut self, now: libc::time_t) -> bool {
        let text = format_clock(now, self.clock_format);
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

    /// Adopt a clock label format (from a gsettings change). Reformats the label
    /// immediately; returns whether the displayed string changed.
    pub fn set_clock_format(&mut self, format: ClockFormat) -> bool {
        if format == self.clock_format {
            return false;
        }
        self.clock_format = format;
        self.update_clock()
    }

    /// How long until the clock label needs redrawing: every second when it shows
    /// seconds, otherwise on the next minute boundary. The wake source that ticks
    /// the clock uses this so an idle desktop wakes no more than it must.
    pub fn clock_tick_interval(&self) -> Duration {
        if self.clock_format.show_seconds {
            Duration::from_secs(1)
        } else {
            Duration::from_secs(secs_until_next_minute())
        }
    }

    /// Reflect the overview open/closed state on the indicator.
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

    /// Whether the indicator button is highlighted (for tests / introspection).
    pub fn activities_checked(&self) -> bool {
        self.activities_checked
    }

    /// The workspace-indicator button rect (left-anchored, so the same on every
    /// output). Width grows with the workspace count.
    pub fn activities_rect(&self, ws: WorkspaceState) -> Rectangle<f64, Logical> {
        Rectangle::new(
            Point::from((0., 0.)),
            Size::from((indicator_logical_width(ws.count), PANEL_HEIGHT)),
        )
    }

    /// The dateMenu (clock) button rect: the shaped label plus a padding on each
    /// side, centered on the output. `output_width` is the output's logical width.
    pub fn date_menu_rect(&self, output_width: f64) -> Rectangle<f64, Logical> {
        let clock_w = niri_vk::text::measure_line_width(&self.clock_text, FONT_PX as f32);
        let w = clock_w + H_PADDING * 2.;
        Rectangle::new(
            Point::from(((output_width - w) / 2., 0.)),
            Size::from((w, PANEL_HEIGHT)),
        )
    }

    /// The panel's items with their current rectangles, for introspection and the
    /// (deferred) extension host. `output_width` is the output's logical width, used
    /// to center the clock.
    pub fn items(&self, output_width: f64, ws: WorkspaceState) -> Vec<PanelItem> {
        vec![
            PanelItem {
                role: ROLE_ACTIVITIES,
                r#box: PanelBox::Left,
                rect: self.activities_rect(ws),
            },
            PanelItem {
                role: ROLE_DATE_MENU,
                r#box: PanelBox::Center,
                rect: self.date_menu_rect(output_width),
            },
        ]
    }

    /// Which panel *role*, if any, sits at an output-local logical position.
    /// `output_width` is needed to place the centered dateMenu.
    pub fn hit_test(
        &self,
        pos: Point<f64, Logical>,
        output_width: f64,
        ws: WorkspaceState,
    ) -> Option<&'static str> {
        if self.activities_rect(ws).contains(pos) {
            Some(ROLE_ACTIVITIES)
        } else if self.date_menu_rect(output_width).contains(pos) {
            Some(ROLE_DATE_MENU)
        } else {
            None
        }
    }

    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        output: &Output,
        ws: WorkspaceState,
    ) -> Vec<TextureRenderElement<VkTexture>> {
        let scale = output.current_scale().fractional_scale();
        let width = output_size(output).w;
        let width_px: i32 = to_physical_precise_round(scale, width);
        let width_px = width_px.max(1);
        let Some(scale_key) = NotNan::new(scale).ok() else {
            return Vec::new();
        };

        let mut cache = self.cache.borrow_mut();

        // The cached textures belong to one renderer context; drop them all if it changed.
        let context = renderer.context_id();
        if cache.context.as_ref() != Some(&context) {
            cache.clear();
            cache.context = Some(context);
        }

        let mut elements = Vec::with_capacity(2);

        // The workspace dots sit on top of the bar. It is pushed first, and the
        // element list is consumed in reverse, so first-pushed is topmost.
        if let Some(strip) = self.indicator_element(renderer, &mut cache, scale, scale_key, ws) {
            elements.push(strip);
        }

        // The bar chrome (opaque background, checked highlight, centered clock).
        let bar_key = (scale_key, width_px, ws.count);
        #[allow(clippy::map_entry)]
        if !cache.textures.contains_key(&bar_key) {
            match draw_bar_texture(
                renderer,
                scale,
                width_px,
                &self.clock_text,
                self.activities_checked,
                ws.count,
            ) {
                Ok(texture) => {
                    cache.textures.insert(bar_key, texture);
                }
                Err(err) => {
                    tracing::error!("error drawing the panel bar: {err:#}");
                    return elements;
                }
            }
        }
        if let Some(texture) = cache.textures.get(&bar_key).cloned() {
            // The whole bar is opaque, so let the compositor skip drawing behind it.
            let opaque = vec![Rectangle::from_size(texture.size())];
            let buffer =
                TextureBuffer::from_texture(renderer, texture, scale, Transform::Normal, opaque);
            elements.push(TextureRenderElement::from_texture_buffer(
                buffer,
                Point::from((0., 0.)),
                1.,
                None,
                None,
                Kind::Unspecified,
            ));
        }

        elements
    }

    /// Build (or reuse) the workspace-dot strip element, composited on top of the
    /// bar at the left, vertically centered.
    fn indicator_element(
        &self,
        renderer: &mut VulkanRenderer,
        cache: &mut BarCache,
        scale: f64,
        scale_key: NotNan<f64>,
        ws: WorkspaceState,
    ) -> Option<TextureRenderElement<VkTexture>> {
        if ws.count == 0 {
            return None;
        }
        let key = (scale_key, ws.count, ws.active());

        #[allow(clippy::map_entry)]
        if !cache.indicator.contains_key(&key) {
            let bitmap = indicator_bitmap(scale, ws)?;
            match TextureBuffer::from_memory_buffer(renderer, &bitmap) {
                Ok(tb) => {
                    cache.indicator.insert(key, tb);
                }
                Err(err) => {
                    tracing::error!("error uploading the panel workspace indicator: {err:#}");
                    return None;
                }
            }
        }
        let buffer = cache.indicator.get(&key)?.clone();

        // Vertically center the dot band (DOT_DIAMETER tall) in the bar.
        let location = Point::from((0., (PANEL_HEIGHT - DOT_DIAMETER) / 2.));
        Some(TextureRenderElement::from_texture_buffer(
            buffer,
            location,
            1.,
            None,
            None,
            Kind::Unspecified,
        ))
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

/// The `strftime` format string for a clock label, assembled the way
/// gnome-shell's `GnomeDesktop.WallClock` does from the interface keys: an
/// optional weekday and date prefix, then the 12h/24h time with optional seconds.
fn strftime_format(fmt: ClockFormat) -> &'static str {
    match (
        fmt.show_weekday,
        fmt.show_date,
        fmt.hour24,
        fmt.show_seconds,
    ) {
        // 24-hour
        (false, false, true, false) => "%H:%M",
        (false, false, true, true) => "%H:%M:%S",
        (true, false, true, false) => "%a %H:%M",
        (true, false, true, true) => "%a %H:%M:%S",
        (false, true, true, false) => "%b %-e %H:%M",
        (false, true, true, true) => "%b %-e %H:%M:%S",
        (true, true, true, false) => "%a %b %-e %H:%M",
        (true, true, true, true) => "%a %b %-e %H:%M:%S",
        // 12-hour (%-l drops the leading space on the hour)
        (false, false, false, false) => "%-l:%M %p",
        (false, false, false, true) => "%-l:%M:%S %p",
        (true, false, false, false) => "%a %-l:%M %p",
        (true, false, false, true) => "%a %-l:%M:%S %p",
        (false, true, false, false) => "%b %-e %-l:%M %p",
        (false, true, false, true) => "%b %-e %-l:%M:%S %p",
        (true, true, false, false) => "%a %b %-e %-l:%M %p",
        (true, true, false, true) => "%a %b %-e %-l:%M:%S %p",
    }
}

/// Format epoch seconds as a local clock label per `fmt`, via locale-aware
/// `strftime` (like GNOME's WallClock).
fn format_clock(now: libc::time_t, fmt: ClockFormat) -> String {
    // SAFETY: localtime returns a pointer into a static buffer; we pass it
    // straight to strftime before any other libc time call touches it.
    unsafe {
        let tm = libc::localtime(&now);
        if tm.is_null() {
            return String::new();
        }
        let Ok(format) = std::ffi::CString::new(strftime_format(fmt)) else {
            return String::new();
        };
        let mut buf = [0u8; 128];
        let n = libc::strftime(
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            format.as_ptr(),
            tm,
        );
        // strftime returns 0 on overflow (never for these short labels).
        String::from_utf8_lossy(&buf[..n]).trim().to_string()
    }
}

/// Rasterize the workspace-dot strip into a premultiplied-white `MemoryBuffer`
/// (transparent background, one capsule per workspace): the active dot is a wide
/// full-opacity pill, the others are small half-opacity circles. A pure CPU
/// distance field (no cairo), like the screenshot shutter.
///
/// The pixels are physical-sized (padding/dots scaled by `scale`), so the buffer
/// is tagged at the output `scale`, never `1.` — otherwise it composites `scale`×
/// too big on a HiDPI output (the buffer-scale-tag trap that bit the shutter).
fn indicator_bitmap(scale: f64, ws: WorkspaceState) -> Option<MemoryBuffer> {
    let count = ws.count;
    if count == 0 {
        return None;
    }
    let active = ws.active();
    let mult = width_multiplier(count);

    let width_px: i32 =
        to_physical_precise_round::<i32>(scale, indicator_logical_width(count)).max(1);
    let height_px: i32 = to_physical_precise_round::<i32>(scale, DOT_DIAMETER).max(1);
    let (w, h) = (width_px as usize, height_px as usize);
    let cy = height_px as f64 / 2.;

    // Physical capsule per dot: two circle centers on the band midline, radius, opacity.
    let mut caps: Vec<(f64, f64, f64, f64)> = Vec::with_capacity(count);
    let mut x = INDICATOR_H_PADDING; // logical cursor at the left edge of the current slot
    for i in 0..count {
        let slot_w = if i == active {
            DOT_DIAMETER * mult
        } else {
            DOT_DIAMETER
        };
        let (draw_w, draw_d, opacity) = if i == active {
            (slot_w, DOT_DIAMETER, 1.0)
        } else {
            (
                DOT_DIAMETER * INACTIVE_DOT_SCALE,
                DOT_DIAMETER * INACTIVE_DOT_SCALE,
                INACTIVE_DOT_OPACITY,
            )
        };
        let slot_cx = x + slot_w / 2.;
        let r = draw_d / 2.;
        let half_span = (draw_w / 2. - r).max(0.);
        caps.push((
            (slot_cx - half_span) * scale,
            (slot_cx + half_span) * scale,
            r * scale,
            opacity,
        ));
        x += slot_w + DOT_SPACING;
    }

    let mut data = vec![0u8; w * h * 4];
    for py in 0..h {
        for px in 0..w {
            let fx = px as f64 + 0.5;
            let fy = py as f64 + 0.5;
            let mut cov = 0.0_f64;
            for &(x0, x1, r, opacity) in &caps {
                // Distance from the pixel to the horizontal segment [x0, x1] at y = cy.
                let qx = fx.clamp(x0, x1);
                let dx = fx - qx;
                let dy = fy - cy;
                let dist = (dx * dx + dy * dy).sqrt();
                // 1 inside the capsule, 0 outside, ~1px physical AA transition.
                let c = (0.5 - (dist - r)).clamp(0., 1.) * opacity;
                cov = cov.max(c);
            }
            // Premultiplied opaque white × coverage → all four channels equal.
            let v = (cov * 255.).round().clamp(0., 255.) as u8;
            let idx = (py * w + px) * 4;
            data[idx] = v;
            data[idx + 1] = v;
            data[idx + 2] = v;
            data[idx + 3] = v;
        }
    }

    Some(MemoryBuffer::new(
        data,
        Fourcc::Argb8888,
        Size::from((width_px, height_px)),
        Scale::from(scale),
        Transform::Normal,
    ))
}

/// Draw the bar chrome into an offscreen [`VkTexture`]: clear the opaque
/// background, draw the checked highlight over the indicator button, then the
/// centered clock glyph run. The returned texture is `SHADER_READ_ONLY`
/// (sampleable) so the caller can composite it directly. The workspace dots are
/// composited separately, on top.
fn draw_bar_texture(
    renderer: &mut VulkanRenderer,
    scale: f64,
    width_px: i32,
    clock: &str,
    checked: bool,
    ws_count: usize,
) -> anyhow::Result<VkTexture> {
    let _span = tracy_client::span!("panel::draw_bar_texture");

    let width_px = width_px.max(1);
    let height_px: i32 = to_physical_precise_round(scale, PANEL_HEIGHT);
    let height_px = height_px.max(1);
    let px = (FONT_PX * scale) as f32;

    let clock_run = renderer.build_glyph_run(clock, px)?;

    // Center the clock's ink in the bar.
    let (c_ix, c_iy, c_iw, c_ih) = clock_run.ink_bounds();
    let c_origin =
        Point::<i32, Physical>::from(((width_px - c_iw) / 2 - c_ix, (height_px - c_ih) / 2 - c_iy));

    // The highlight matches the indicator button rect (padding + dots), so it
    // agrees with `activities_rect`.
    let highlight_w: i32 = to_physical_precise_round(scale, indicator_logical_width(ws_count));
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

    /// The indicator button widens as workspaces are added (structural — no GPU).
    #[test]
    fn indicator_width_grows_with_workspace_count() {
        let w2 = indicator_logical_width(2);
        let w3 = indicator_logical_width(3);
        let w6 = indicator_logical_width(6);
        assert!(w3 > w2, "3 workspaces should be wider than 2: {w3} vs {w2}");
        assert!(w6 > w3, "6 workspaces should be wider than 3: {w6} vs {w3}");

        let panel = Panel::new();
        let r2 = panel.activities_rect(WorkspaceState {
            count: 2,
            active: 0,
        });
        let r4 = panel.activities_rect(WorkspaceState {
            count: 4,
            active: 1,
        });
        assert!(r4.size.w > r2.size.w);
        // A click at the left edge lands on the indicator; the far right does not.
        assert_eq!(
            panel.hit_test(
                Point::from((4., 10.)),
                1920.,
                WorkspaceState {
                    count: 3,
                    active: 1
                }
            ),
            Some(ROLE_ACTIVITIES)
        );
        assert_eq!(
            panel.hit_test(
                Point::from((10_000., 10.)),
                1920.,
                WorkspaceState {
                    count: 3,
                    active: 1
                }
            ),
            None
        );
    }

    /// The panel exposes both roles in their boxes (extension-representable model).
    #[test]
    fn items_expose_roles_and_boxes() {
        let panel = Panel::new();
        let items = panel.items(
            1920.,
            WorkspaceState {
                count: 3,
                active: 1,
            },
        );
        let activities = items.iter().find(|i| i.role == ROLE_ACTIVITIES).unwrap();
        let date = items.iter().find(|i| i.role == ROLE_DATE_MENU).unwrap();
        assert_eq!(activities.r#box, PanelBox::Left);
        assert_eq!(date.r#box, PanelBox::Center);
        // The clock is roughly centered on the output.
        let center = date.rect.loc.x + date.rect.size.w / 2.;
        assert!((center - 960.).abs() < 1.);
    }

    /// The rasterized dot strip: the active pill spans more columns of bright
    /// coverage than an inactive dot, and it is opaque while inactive dots are dim.
    #[test]
    fn indicator_bitmap_active_pill_is_wider_and_brighter() {
        let ws = WorkspaceState {
            count: 3,
            active: 1,
        };
        let bmp = indicator_bitmap(2.0, ws).expect("bitmap");
        let w = bmp.size().w as usize;
        let h = bmp.size().h as usize;
        let data = bmp.data();
        // Peak alpha in each column (channels are equal: premultiplied white).
        let col_peak: Vec<u8> = (0..w)
            .map(|x| (0..h).map(|y| data[(y * w + x) * 4 + 3]).max().unwrap_or(0))
            .collect();
        // Count columns whose peak crosses a mid threshold — a proxy for the drawn
        // width. The active pill (full opacity, wide) beats an inactive dot.
        let bright_cols = col_peak.iter().filter(|&&v| v > 200).count();
        let dim_present = col_peak.iter().any(|&v| v > 90 && v <= 200);
        assert!(
            bright_cols >= (DOT_DIAMETER * 2.0) as usize,
            "expected a wide bright pill, only {bright_cols} bright columns"
        );
        assert!(
            dim_present,
            "expected dimmer inactive dots (half-opacity coverage)"
        );
    }

    /// The buffer-scale-tag guard for the hand-rasterized dot strip (see the
    /// `vulkan-buffer-scale-tag-trap`): its pixels are physical, so the tag must be
    /// the output scale. If it were `1.`, the *logical* size would balloon with the
    /// scale and the strip would composite `scale`× too big on a HiDPI seat —
    /// invisible in the scale-1 suite. Asserting logical size is scale-invariant is
    /// exactly what the compositor consumes (`logical = pixels / tag`).
    #[test]
    fn indicator_bitmap_is_tagged_at_output_scale() {
        let ws = WorkspaceState {
            count: 3,
            active: 1,
        };
        let expected_w = indicator_logical_width(ws.count);
        for scale in [1.0, 1.5, 2.0] {
            let bmp = indicator_bitmap(scale, ws).expect("bitmap");
            assert!(
                (bmp.scale().x - scale).abs() < 1e-9 && (bmp.scale().y - scale).abs() < 1e-9,
                "strip tagged at {:?}, not the output scale {scale}",
                bmp.scale(),
            );
            // Physical pixels grow with scale, but the logical size must not.
            let logical = bmp.logical_size();
            assert!(
                (logical.w - expected_w).abs() < 1.0,
                "logical width {} drifts from {expected_w} at scale {scale} — physical pixels \
                 tagged at the wrong scale",
                logical.w,
            );
            assert!(
                (logical.h - DOT_DIAMETER).abs() < 1.0,
                "logical height {} drifts from {DOT_DIAMETER} at scale {scale}",
                logical.h,
            );
        }
    }

    /// The clock's `strftime` format is assembled from the interface keys the
    /// same way GNOME's WallClock does (`dateMenu.js`). Locale-independent.
    #[test]
    fn clock_strftime_format_matches_the_interface_keys() {
        let f = |hour24, wd, date, sec| {
            strftime_format(ClockFormat {
                hour24,
                show_weekday: wd,
                show_date: date,
                show_seconds: sec,
            })
        };
        assert_eq!(f(true, false, false, false), "%H:%M");
        assert_eq!(f(true, true, false, false), "%a %H:%M");
        assert_eq!(f(true, false, false, true), "%H:%M:%S");
        assert_eq!(f(true, true, true, true), "%a %b %-e %H:%M:%S");
        assert_eq!(f(false, false, false, false), "%-l:%M %p");
        assert_eq!(f(false, true, true, false), "%a %b %-e %-l:%M %p");
    }

    /// The rendered label reflects the format: seconds add a field, and a weekday
    /// or date prefix turns the leading digit into a letter. TZ/locale-robust.
    #[test]
    fn clock_label_reflects_the_format() {
        let base = ClockFormat {
            hour24: true,
            show_weekday: false,
            show_date: false,
            show_seconds: false,
        };
        let hhmm = format_clock(0, base);
        assert_eq!(hhmm.len(), 5, "expected HH:MM, got {hhmm:?}");
        assert_eq!(hhmm.as_bytes()[2], b':', "expected HH:MM, got {hhmm:?}");

        let with_secs = format_clock(
            0,
            ClockFormat {
                show_seconds: true,
                ..base
            },
        );
        assert_eq!(
            with_secs.matches(':').count(),
            2,
            "seconds must add a field, got {with_secs:?}"
        );

        let with_weekday = format_clock(
            0,
            ClockFormat {
                show_weekday: true,
                ..base
            },
        );
        assert!(
            with_weekday
                .chars()
                .next()
                .is_some_and(|c| c.is_alphabetic()),
            "a weekday prefix must lead with a letter, got {with_weekday:?}"
        );

        let with_date = format_clock(
            0,
            ClockFormat {
                show_date: true,
                ..base
            },
        );
        assert!(
            with_date.len() > hhmm.len(),
            "a date prefix must lengthen the label, got {with_date:?}"
        );
    }

    /// Showing seconds tightens the clock tick to one second; otherwise it waits
    /// for the minute boundary.
    #[test]
    fn clock_tick_interval_tightens_with_seconds() {
        let mut panel = Panel::new(); // default format shows no seconds
        let minute = panel.clock_tick_interval();
        assert!(
            minute > Duration::ZERO && minute <= Duration::from_secs(60),
            "the minute tick must land on the next minute boundary, got {minute:?}"
        );
        panel.set_clock_format(ClockFormat {
            hour24: true,
            show_weekday: false,
            show_date: false,
            show_seconds: true,
        });
        assert_eq!(panel.clock_tick_interval(), Duration::from_secs(1));
    }

    /// Drive the GPU bar into an offscreen and read it back: an opaque dark
    /// background, the checked highlight on the left, and bright clock glyph ink.
    /// Skips cleanly with no device.
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
        let mut tex =
            draw_bar_texture(&mut vk, 1., width_px, "12:34", true, 3).expect("bar texture");

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

        // Bright glyph ink somewhere (the clock text).
        let bright = pixels
            .chunks_exact(4)
            .filter(|p| p[0] > 150 && p[1] > 150 && p[2] > 150)
            .count();
        assert!(bright > 40, "expected visible glyph ink, got {bright}");
    }
}
