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
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use anyhow::{bail, Context, Result};
use ash::vk;
use cosmic_text::{
    Align, Attrs, Buffer, CacheKey, Family, FeatureTag, FontFeatures, FontSystem, Hinting, Metrics,
    Shaping, Weight,
};
use etagere::{size2, AtlasAllocator};
use swash::scale::image::Content;
use swash::scale::{Render, ScaleContext, Source};
use swash::zeno::{Format, Vector};

use crate::gpu::Gpu;
use crate::render::{as_bytes, load_module, TextPush};
use crate::shaders::{TEXT_FRAG, TEXT_VERT};
use crate::texture::{CoverageRegion, Texture};

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

/// A shaped, rasterized run, with every glyph resolved to a slot in the caller's persistent
/// atlas. Holds no GPU resource: the atlas image belongs to whoever owns
/// [`GlyphAtlasIndex`]'s companion texture, and outlives any one run.
pub struct ShapedRun {
    pub glyphs: Vec<PlacedGlyph>,
    /// Source-span index of each glyph in [`Self::glyphs`], parallel to it. For
    /// [`TextContext::shape_paragraph`] this is the position in its `spans` slice; for a single
    /// run it is all zeroes. Lets a caller recover a span's ink rectangle to paint an inline
    /// background (e.g. a keycap).
    pub spans: Vec<u32>,
    /// Line-box metrics of the run's first line, in physical px, for baseline (line-box) vertical
    /// centering — what GNOME/Pango do (center the font's ascent+descent extents, reserving
    /// descent space) rather than ink centering. `baseline` is the run-local y of the first
    /// line's baseline (the same origin the glyph placements are measured from);
    /// `ascent`/`descent` are the font's extents above/below it. All zero for an empty
    /// (whitespace-only) run.
    pub baseline: i32,
    pub ascent: f32,
    pub descent: f32,
}

/// Where one glyph lives in the atlas, plus the ink offset from its pen position (swash's
/// `placement.left`/`top`). Cached so a glyph is rasterized once for the life of the atlas.
#[derive(Clone, Copy, Debug)]
struct GlyphSlot {
    atlas_x: u32,
    atlas_y: u32,
    w: u32,
    h: u32,
    left: i32,
    top: i32,
}

/// The largest atlas we will grow to, per side. 2048² R8 is 4 MiB.
pub const MAX_ATLAS_SIDE: u32 = 2048;

/// Where a fresh atlas starts. 512² R8 is 256 KiB and comfortably holds the Latin alphabet at
/// every size and weight the chrome uses, so in practice it never grows.
pub const INITIAL_ATLAS_SIDE: u32 = 512;

/// CPU-side residency map for a **persistent** glyph atlas: which glyph occupies which slot, and
/// where the next one can go.
///
/// Owns no GPU resource. The atlas image belongs to the renderer — that is where texture
/// ownership and uploads live — and the two are kept in step by [`Self::generation`]: when it
/// changes, the image must be recreated at [`Self::side`] and every run built against the old one
/// keeps working, because it holds its own reference to the old image.
///
/// This is the whole point of the persistent atlas: a run whose glyphs are already resident needs
/// no rasterization and no upload, so redrawing changing text (a clock ticking seconds) costs a
/// shape and nothing else. The per-run atlas it replaced allocated a fresh image and paid an
/// upload — a full GPU round trip — for every string, every frame.
pub struct GlyphAtlasIndex {
    side: u32,
    allocator: AtlasAllocator,
    /// `None` marks a key that draws nothing (whitespace, missing font, empty raster), remembered
    /// so we neither re-rasterize it nor emit an instance for it.
    slots: HashMap<CacheKey, Option<GlyphSlot>>,
    generation: u64,
}

impl GlyphAtlasIndex {
    fn new(side: u32) -> Self {
        GlyphAtlasIndex {
            side,
            allocator: AtlasAllocator::new(size2(side as i32, side as i32)),
            slots: HashMap::new(),
            generation: 0,
        }
    }

    /// Side length of the atlas image this index describes.
    pub fn side(&self) -> u32 {
        self.side
    }

    /// Bumped whenever the image must be recreated (only on growth). The owner of the image
    /// compares against its own copy to decide whether to reallocate.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Forget every slot at the current size, bumping the generation so the image is recreated and
    /// every glyph re-rasterizes on demand.
    ///
    /// For the owner of the image to call when an upload **failed**. A glyph is recorded resident
    /// the moment it is rasterized, before its bytes reach the GPU (see
    /// [`resolve_once`](TextContext::resolve_once)), so a lost upload leaves the index claiming
    /// residency for glyphs no image contains — and because a hit emits no `PendingGlyph`, nothing
    /// would ever upload them again. Text would stay blank for the life of the atlas. This is the
    /// way back: throw the residency away and let it rebuild.
    pub fn invalidate(&mut self) {
        self.allocator = AtlasAllocator::new(size2(self.side as i32, self.side as i32));
        self.slots.clear();
        self.generation += 1;
    }

    /// Double the atlas and forget every slot, so the run that ran out is re-rasterized into the
    /// larger image. Returns `false` at [`MAX_ATLAS_SIDE`], where the caller must fail the run.
    ///
    /// Wholesale reset rather than a repack: the glyphs are cheap to rasterize again, a repack
    /// would have to rewrite every already-built run's coordinates, and growth is a once-ever
    /// event in practice.
    fn grow(&mut self) -> bool {
        if self.side >= MAX_ATLAS_SIDE {
            return false;
        }
        self.side *= 2;
        self.allocator = AtlasAllocator::new(size2(self.side as i32, self.side as i32));
        self.slots.clear();
        self.generation += 1;
        true
    }

    /// Reserve a slot for a rasterized glyph, or `None` if the atlas is out of room. The `+1` on
    /// each side is a bleed guard, so NEAREST sampling at the slot edge cannot pick up a
    /// neighbour.
    fn allocate(&mut self, key: CacheKey, raster: &GlyphRaster) -> Option<GlyphSlot> {
        let alloc = self
            .allocator
            .allocate(size2(raster.w as i32 + 1, raster.h as i32 + 1))?;
        let slot = GlyphSlot {
            atlas_x: alloc.rectangle.min.x as u32,
            atlas_y: alloc.rectangle.min.y as u32,
            w: raster.w,
            h: raster.h,
            left: raster.left,
            top: raster.top,
        };
        self.slots.insert(key, Some(slot));
        Some(slot)
    }
}

/// One glyph's hinted R8 coverage bitmap, as swash rasterized it.
struct GlyphRaster {
    data: Vec<u8>,
    w: u32,
    h: u32,
    left: i32,
    top: i32,
}

/// A glyph that just became resident and still has to reach the atlas image. The caller uploads
/// these (one round trip for the batch) before drawing the run.
pub struct PendingGlyph {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub coverage: Vec<u8>,
}

impl PendingGlyph {
    /// Borrow as an upload region for [`Texture::upload_coverage_regions`].
    pub fn region(&self) -> CoverageRegion<'_> {
        CoverageRegion {
            x: self.x,
            y: self.y,
            w: self.w,
            h: self.h,
            coverage: &self.coverage,
        }
    }
}

/// Lock **the** font database — one per process, shared by the GPU-free measurement helpers and
/// by every [`TextContext`] shape.
///
/// Global because building one scans and parses the system fonts, which is the expensive half of
/// the text stack (see [`prewarm`]), and because a second copy buys nothing: measurement and
/// drawing want the same faces. It is also what lets the startup prewarm warm the *actual* object
/// the renderer will shape through instead of only the page cache underneath it.
///
/// **Not reentrant.** Anything that already holds this guard must pass `&mut FontSystem` down
/// rather than call back into a helper that locks — see [`TextContext::resolve`].
///
/// The lock is free today: only the compositor thread shapes, plus the prewarm thread once at
/// startup. If shaping ever moves off-thread (a text-heavy bake on a worker), this becomes a
/// serialization point and the answer is a per-thread `FontSystem` over a shared `fontdb`, not a
/// bigger critical section.
fn fonts() -> MutexGuard<'static, FontSystem> {
    static FONTS: OnceLock<Mutex<FontSystem>> = OnceLock::new();
    let mutex = FONTS.get_or_init(|| Mutex::new(FontSystem::new()));
    // Recover from poisoning rather than propagate it. A shape that panics while holding this
    // guard would otherwise brick *all* text in the process for the rest of the session — every
    // later lock panics, and this module's contract is that a failure degrades to no glyphs, not
    // a panic. What the guard protects is a cache: the worst a mid-shape panic leaves behind is a
    // missing entry, which the next shape refills.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Build the font database and resolve SansSerif, so the first frame does not have to.
///
/// Intended to run on a throwaway thread at startup, alongside the event loop and backend being
/// built. Nothing here is needed by the caller — the work lands in [`fonts`]'s `OnceLock`, which
/// is the same database every later shape uses, and in the kernel's page cache underneath it.
///
/// The live seat's very first frame spent **32.91ms shaping 4 runs**; later frames shape 2 runs in
/// 0.34ms. Measured against a warm page cache the same construct-and-shape sequence is 6.3ms for
/// `FontSystem::new()` plus 3.4ms for the first four runs, and a genuinely cold one made a single
/// `measure_line_width` call take **408ms** — so the cost is font *file I/O and first parse*, not
/// shaping. Prewarming on a thread took the seat's first frame from 57.81ms to 26.18ms and its
/// shaping clause from 32.91ms to 10.75ms; the rest of that clause was the renderer's *own*
/// `FontSystem` repeating the parse, which is why there is only one now. Behind this call,
/// [`TextContext::new`] is 1µs and the first frame's four runs shape in 0.18ms.
///
/// Idempotent and cheap to call twice; a second call is a cache hit.
pub fn prewarm() {
    // A shape, not just construction: `FontSystem::new()` enumerates, but resolving SansSerif is
    // what reads and parses the face we actually draw with.
    let _ = measure_line_width("Ag", 16.0);
}

/// Logical width, in pixels, of a single-line `text` shaped SansSerif at `px` pixels-per-em — the
/// advance the renderer lays the run out to. GPU-free (shaping only), so callers can size a hit
/// rectangle at construction time, before a renderer exists. Matches the width `build_glyph_run`
/// would produce at the same `px` (both shape SansSerif through cosmic-text).
pub fn measure_line_width(text: &str, px: f32) -> f64 {
    measure_line_width_weighted(text, px, false)
}

/// Cache for [`measure_line_width_weighted`], keyed by the whole argument list.
///
/// A measurement is a full cosmic-text shape whose only output is one `f64`, and
/// layout code calls it the way it would call a getter — the panel alone measures
/// its clock four times per frame (`ui/panel.rs`, `date_menu_rect` via the role,
/// hit-test and element paths). Memoizing turns a repeat call into a hash lookup.
///
/// Bounded by clearing wholesale at [`MEASURE_CACHE_CAP`]: with seconds shown, the
/// clock contributes one never-reused key per second, so this must not grow
/// forever. Dropping everything costs a re-measure of the live labels, which is
/// what an unbounded cache was paying every frame anyway.
fn measure_cache() -> &'static Mutex<MeasureCache> {
    static CACHE: OnceLock<Mutex<MeasureCache>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Measured line widths keyed by `(text, px bits, bold)`.
type MeasureCache = HashMap<(String, u32, bool), f64>;

const MEASURE_CACHE_CAP: usize = 1024;

/// Like [`measure_line_width`], but shapes at [`Weight::BOLD`] when `bold` — the width a bold run
/// (e.g. the panel clock, which draws `font-weight: bold` like GNOME's `panel_button`) lays out to,
/// so its hit rectangle and centering match the glyphs `build_glyph_run_weighted` rasterizes.
///
/// Memoized (see [`measure_cache`]); a hit does no shaping at all.
pub fn measure_line_width_weighted(text: &str, px: f32, bold: bool) -> f64 {
    // `px` is a float only because font sizes scale; its bits key exactly, and
    // two sizes that differ below representable precision are the same size.
    let key = (text.to_owned(), px.to_bits(), bold);
    if let Some(width) = measure_cache().lock().unwrap().get(&key) {
        return *width;
    }

    let width = measure_line_width_uncached(text, px, bold);

    let mut cache = measure_cache().lock().unwrap();
    if cache.len() >= MEASURE_CACHE_CAP {
        cache.clear();
    }
    cache.insert(key, width);
    width
}

fn measure_line_width_uncached(text: &str, px: f32, bold: bool) -> f64 {
    let _timed = crate::stats::shape();
    let mut fonts = fonts();
    let mut buffer = Buffer::new(&mut fonts, Metrics::new(px, (px * 1.25).round()));
    {
        let mut b = buffer.borrow_with(&mut fonts);
        b.set_size(None, None);
        let attrs = sans_label_attrs(bold);
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
        let mut attrs = Attrs::new().family(SANS_FAMILY);
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
///
/// Memoized (see [`wrap_cache`]); a hit does no shaping at all.
pub fn wrap_lines_weighted(
    text: &str,
    px: f32,
    bold: bool,
    wrap_px: f64,
    max_lines: usize,
) -> Vec<String> {
    let max_lines = max_lines.max(1);
    if text.is_empty() {
        return Vec::new();
    }
    let key = (
        text.to_owned(),
        px.to_bits(),
        bold,
        wrap_px.to_bits(),
        max_lines,
    );
    if let Some(lines) = wrap_cache().lock().unwrap().get(&key) {
        return lines.clone();
    }

    let lines = wrap_lines_uncached(text, px, bold, wrap_px, max_lines);

    let mut cache = wrap_cache().lock().unwrap();
    if cache.len() >= WRAP_CACHE_CAP {
        cache.clear();
    }
    cache.insert(key, lines.clone());
    lines
}

/// Cache for [`wrap_lines_weighted`], keyed by the whole argument list.
///
/// A wrap is strictly more expensive than a measure — one shape per attempt, and the
/// ellipsis retreat re-shapes in a loop — and its callers ask for it the way they would
/// read a field: a notification card lays itself out once per hit-test and again per
/// render-element collection, so a pointer crossing the message list re-wraps every
/// visible card's body on every motion event. Nothing about the body changed.
///
/// Bounded the same way [`measure_cache`] is, and for the same reason: a body can carry a
/// live counter ("3 new messages"), which would otherwise contribute a never-reused key
/// per update. Clearing wholesale costs a re-wrap of what is on screen.
fn wrap_cache() -> &'static Mutex<WrapCache> {
    static CACHE: OnceLock<Mutex<WrapCache>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Wrapped lines keyed by `(text, px bits, bold, wrap px bits, max lines)`.
type WrapCache = HashMap<(String, u32, bool, u64, usize), Vec<String>>;

/// Smaller than [`MEASURE_CACHE_CAP`]: the values are whole line vectors, and the keys are
/// message bodies rather than short labels.
const WRAP_CACHE_CAP: usize = 256;

fn wrap_lines_uncached(
    text: &str,
    px: f32,
    bold: bool,
    wrap_px: f64,
    max_lines: usize,
) -> Vec<String> {
    const ELLIPSIS: char = '\u{2026}';
    let _timed = crate::stats::shape();
    let mut fonts = fonts();
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

/// The renderer-scoped pieces of the text stack: `ScaleContext` caches per-font scaler state, and
/// the residency map for the **persistent** glyph atlas ([`GlyphAtlasIndex`]) — glyphs are
/// rasterized once and stay, so re-shaping changing text costs a shape and nothing more. The
/// compositor holds ONE of these for the life of the renderer.
///
/// The font database is **not** here: it is the process-global [`fonts`], shared with the
/// measurement path. Two `FontSystem`s meant scanning and parsing the system fonts twice — the
/// expensive half of the text stack, and the whole of what [`prewarm`] exists to move off the
/// first frame — for two copies of the same data. Sharing also means the prewarm warms the exact
/// object the renderer shapes through, rather than only the page cache underneath it.
///
/// What stays per-renderer is what *has* to: the atlas index describes a specific atlas image, and
/// the two are kept in step by a generation counter rather than by identity. A global index would
/// outlive the image it describes — a recreated renderer starts with no atlas image while the
/// index still claims glyphs are resident in it, and the mismatch is silent (blank or wrong
/// glyphs), not a panic.
pub struct TextContext {
    scale: ScaleContext,
    atlas: GlyphAtlasIndex,
}

impl Default for TextContext {
    fn default() -> Self {
        Self::new()
    }
}

impl TextContext {
    pub fn new() -> Self {
        TextContext {
            scale: ScaleContext::new(),
            atlas: GlyphAtlasIndex::new(INITIAL_ATLAS_SIDE),
        }
    }

    /// Shape `text` at `px` pixels-per-em (SansSerif) and resolve every glyph into the persistent
    /// atlas, rasterizing and returning only the glyphs that were not already resident.
    ///
    /// The returned [`PendingGlyph`]s must be uploaded into the atlas image before the run is
    /// drawn; an empty list — the steady state, once the alphabet in use is resident — means the
    /// run needs no GPU work at all.
    pub fn shape_line(&mut self, text: &str, px: f32) -> Result<(ShapedRun, Vec<PendingGlyph>)> {
        self.shape_line_weighted(text, px, false)
    }

    /// Like [`Self::shape_line`], but shapes and rasterizes at [`Weight::BOLD`] when `bold` — the
    /// bold panel-clock path (GNOME's `panel_button` is `font-weight: bold`).
    pub fn shape_line_weighted(
        &mut self,
        text: &str,
        px: f32,
        bold: bool,
    ) -> Result<(ShapedRun, Vec<PendingGlyph>)> {
        let _timed = crate::stats::shape();
        let mut fonts = fonts();
        let mut buffer = Buffer::new(&mut fonts, Metrics::new(px, (px * 1.25).round()));
        buffer.set_hinting(Hinting::Enabled);
        {
            let mut b = buffer.borrow_with(&mut fonts);
            b.set_size(None, None);
            let attrs = sans_label_attrs(bold);
            b.set_text(text, &attrs, Shaping::Advanced, None);
            b.shape_until_scroll(false);
        }
        self.resolve(&mut fonts, &buffer)
    }

    /// Lay out a styled, center-aligned paragraph wrapped to `wrap_px` pixels: each [`TextSpan`]
    /// carries its own family/weight/size (via cosmic-text rich text), and the multi-line result is
    /// resolved into the same persistent atlas. `base_px` sets the default line metrics.
    /// Glyph placements are paragraph-local (top-left origin), so one atlas draws the whole block.
    /// This is the dialog/notification text path (title + body + hints in one box).
    pub fn shape_paragraph(
        &mut self,
        spans: &[TextSpan],
        wrap_px: f32,
        base_px: f32,
    ) -> Result<(ShapedRun, Vec<PendingGlyph>)> {
        let _timed = crate::stats::shape();
        let mut fonts = fonts();
        let mut buffer = Buffer::new(&mut fonts, Metrics::new(base_px, (base_px * 1.25).round()));
        buffer.set_hinting(Hinting::Enabled);
        {
            let mut b = buffer.borrow_with(&mut fonts);
            b.set_size(Some(wrap_px), None);
            let default_attrs = Attrs::new().family(SANS_FAMILY);
            // Tag each span with its index (cosmic-text carries it to every laid-out glyph as
            // `metadata`), so `resolve` can record which span each glyph came from and a caller
            // can recover a span's ink rectangle (e.g. to paint an inline keycap background).
            let rich = spans
                .iter()
                .enumerate()
                .map(|(i, s)| (s.text, s.attrs().metadata(i)));
            b.set_rich_text(rich, &default_attrs, Shaping::Advanced, Some(Align::Center));
            b.shape_until_scroll(false);
        }
        self.resolve(&mut fonts, &buffer)
    }

    /// The atlas residency map, for the owner of the atlas image (see [`GlyphAtlasIndex`]).
    pub fn atlas(&self) -> &GlyphAtlasIndex {
        &self.atlas
    }

    /// The residency index, mutably — for [`GlyphAtlasIndex::invalidate`] after a failed upload.
    pub fn atlas_mut(&mut self) -> &mut GlyphAtlasIndex {
        &mut self.atlas
    }

    /// Resolve every glyph of an already-shaped `buffer` into the persistent atlas. Shared by
    /// [`Self::shape_line_weighted`] and [`Self::shape_paragraph`]; the placements it emits are
    /// buffer-local (top-left), spanning as many lines as the buffer holds.
    ///
    /// Grows the atlas and retries whenever a glyph does not fit, which discards residency — so
    /// the next pass re-rasterizes this run's glyphs, and any earlier run keeps drawing from the
    /// image it was built against.
    ///
    /// Loops rather than retrying once: a single doubling need not be enough (a full ASCII set at
    /// 44px overflows both a 64px atlas and the 128px one it first grows to), and stopping early
    /// would fail a run the atlas can in fact hold.
    ///
    /// Takes `fonts` rather than locking: the caller is already holding the guard across its
    /// shape, and re-locking here would deadlock on a non-reentrant mutex.
    fn resolve(
        &mut self,
        fonts: &mut FontSystem,
        buffer: &Buffer,
    ) -> Result<(ShapedRun, Vec<PendingGlyph>)> {
        loop {
            if let Some(resolved) = self.resolve_once(fonts, buffer) {
                return Ok(resolved);
            }
            if !self.atlas.grow() {
                bail!("glyph atlas full at {MAX_ATLAS_SIDE}px");
            }
        }
    }

    /// One resolution pass. `None` means the atlas ran out of room, which is the caller's cue to
    /// grow and retry — not an error.
    fn resolve_once(
        &mut self,
        fonts: &mut FontSystem,
        buffer: &Buffer,
    ) -> Option<(ShapedRun, Vec<PendingGlyph>)> {
        // Line-box metrics for baseline centering: the font's ascent/descent (px) at this size,
        // taken from the first line's first glyph, plus that line's baseline. Pango/St center a
        // single-line label on this box (ascent+descent) — reserving descent space — not on the
        // ink, so caps sit a hair higher than ink centering. Zero for a glyph-less run.
        let ppem = buffer.metrics().font_size;
        let mut baseline = 0i32;
        let mut ascent = 0.0f32;
        let mut descent = 0.0f32;
        'metrics: for run in buffer.layout_runs() {
            for glyph in run.glyphs {
                let key = glyph.physical((0.0, 0.0), 1.0).cache_key;
                if let Some(font) = fonts.get_font(key.font_id, key.font_weight) {
                    let m = font.as_swash().metrics(&[]).scale(ppem);
                    baseline = run.line_y.round() as i32;
                    ascent = m.ascent;
                    descent = m.descent;
                    break 'metrics;
                }
            }
        }

        let mut glyphs = Vec::new();
        let mut spans = Vec::new();
        let mut pending = Vec::new();

        for run in buffer.layout_runs() {
            let line_baseline = run.line_y.round() as i32;
            for glyph in run.glyphs {
                // Whole-pixel origin: round X, then physical() truncates Y (its own hinting).
                let mut lg = glyph.clone();
                lg.x = lg.x.round();
                let phys = lg.physical((0.0, 0.0), 1.0);
                let key = phys.cache_key;

                // One coverage bitmap per DISTINCT glyph (a `CacheKey` folds in glyph id, font,
                // size, weight, and subpixel bin), and now for the life of the atlas rather than
                // of one run — so a repeated character, a repeated label, and the same digit next
                // second all share the slot.
                let slot = match self.atlas.slots.get(&key) {
                    Some(cached) => *cached,
                    None => {
                        let raster = Self::rasterize_glyph(fonts, &mut self.scale, key);
                        match raster {
                            Some(raster) => {
                                let slot = self.atlas.allocate(key, &raster)?;
                                pending.push(PendingGlyph {
                                    x: slot.atlas_x,
                                    y: slot.atlas_y,
                                    w: slot.w,
                                    h: slot.h,
                                    coverage: raster.data,
                                });
                                Some(slot)
                            }
                            None => {
                                // Whitespace / missing glyph: no bitmap, pen still advances.
                                self.atlas.slots.insert(key, None);
                                None
                            }
                        }
                    }
                };

                if let Some(slot) = slot {
                    glyphs.push(PlacedGlyph {
                        x: phys.x + slot.left,
                        y: line_baseline + phys.y - slot.top,
                        w: slot.w,
                        h: slot.h,
                        atlas_x: slot.atlas_x,
                        atlas_y: slot.atlas_y,
                    });
                    spans.push(glyph.metadata as u32);
                }
            }
        }

        Some((
            ShapedRun {
                glyphs,
                spans,
                baseline,
                ascent,
                descent,
            },
            pending,
        ))
    }

    /// Rasterize one glyph, hinted. `None` for a glyph with no ink (whitespace) or a font that no
    /// longer resolves.
    fn rasterize_glyph(
        fonts: &mut FontSystem,
        ctx: &mut ScaleContext,
        key: CacheKey,
    ) -> Option<GlyphRaster> {
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
            return None;
        }
        debug_assert!(matches!(image.content, Content::Mask));
        Some(GlyphRaster {
            data: image.data,
            w,
            h,
            left: image.placement.left,
            top: image.placement.top,
        })
    }
}

/// One styled span of a [`TextContext::shape_paragraph`] paragraph.
#[derive(Clone, Copy)]
pub struct TextSpan<'a> {
    pub text: &'a str,
    pub family: SpanFamily,
    pub bold: bool,
    /// Font size in pixels-per-em for this span.
    pub px: f32,
}

/// GNOME's default UI font is Cantarell (`org.gnome.desktop.interface font-name` = "Cantarell 11").
/// The generic `Family::SansSerif` resolves through fontconfig to whatever `sans` maps to (Noto
/// Sans on this system), whose glyph shapes and metrics differ from GNOME's — so name Cantarell.
/// If it is missing, cosmic-text falls back to the fontdb default. TODO: read the `font-name`
/// gsetting instead of hardcoding, per the fork's "GNOME's way" tenet.
pub const SANS_FAMILY: Family<'static> = Family::Name("Cantarell");

/// Sans attrs for a single-line LABEL: Cantarell, optional bold, and **tabular figures** (`tnum`).
/// GNOME applies `%numeric` (`tnum`) to the panel clock and calendar numbers so a digit run keeps a
/// constant advance — Cantarell's default figures are proportional (`1` is narrower than `8`),
/// which would jitter the advance-centered clock every second. Labels are exactly GNOME's numeric
/// surfaces (clock, dates, counts); body paragraphs keep proportional figures (they don't set
/// this).
fn sans_label_attrs(bold: bool) -> Attrs<'static> {
    let mut features = FontFeatures::new();
    features.enable(FeatureTag::new(b"tnum"));
    let mut attrs = Attrs::new().family(SANS_FAMILY).font_features(features);
    if bold {
        attrs = attrs.weight(Weight::BOLD);
    }
    attrs
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
            SpanFamily::Sans => SANS_FAMILY,
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

/// Shape `text` at `px` pixels-per-em into a freshly allocated coverage atlas, returning the atlas
/// image, its side, and the run.
///
/// One-shot: builds a throwaway [`TextContext`], so it also throws away glyph residency. Callers
/// drawing repeatedly should hold a `TextContext` and call [`TextContext::shape_line`], which is
/// the whole point of the persistent atlas.
pub fn build_text(
    gpu: &Arc<Gpu>,
    pool: vk::CommandPool,
    text: &str,
    px: f32,
) -> Result<(Texture, u32, ShapedRun)> {
    let mut ctx = TextContext::new();
    let (run, pending) = ctx.shape_line(text, px)?;
    let side = ctx.atlas().side();
    let texture = Texture::new_coverage_atlas(gpu, pool, side)?;
    let regions: Vec<_> = pending.iter().map(PendingGlyph::region).collect();
    match texture.upload_coverage_regions(gpu, pool, &regions) {
        Ok(()) => Ok((texture, side, run)),
        Err(err) => {
            texture.destroy(gpu);
            Err(err)
        }
    }
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
        // Premultiplied-over, like every other material: `text.frag` scales the premultiplied tint
        // by the atlas coverage.
        let blend_attachment = vk::PipelineColorBlendAttachmentState::default()
            .blend_enable(true)
            .src_color_blend_factor(vk::BlendFactor::ONE)
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

    /// Draw every glyph of `run`, offset to `(ox, oy)` in a `target`-sized image, in `color`
    /// (**premultiplied** — the glyph material blends premultiplied-over like every other).
    /// `atlas_side` is the side of the atlas `set` samples, for the UV divide.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(
        &self,
        gpu: &Gpu,
        cbuf: vk::CommandBuffer,
        set: vk::DescriptorSet,
        run: &ShapedRun,
        atlas_side: u32,
        origin: (f32, f32),
        target: [f32; 2],
        color: [f32; 4],
    ) {
        let device = &gpu.device;
        let side = atlas_side as f32;
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
            for g in &run.glyphs {
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
                crate::stats::draw(u64::from(g.w) * u64::from(g.h));
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

    /// Owns the atlas image for a [`TextContext`], recreating it when the residency index's
    /// generation moves and uploading each run's newly resident glyphs. This is the same handshake
    /// the compositor's renderer keeps, deliberately — including the ordering that matters:
    /// **check the generation before uploading**, because a run that grew the atlas mid-resolve
    /// returns slots for the new, larger image.
    struct TestAtlas {
        texture: Texture,
        generation: u64,
        side: u32,
    }

    impl TestAtlas {
        fn new(gpu: &Gpu, pool: vk::CommandPool, ctx: &TextContext) -> Result<Self> {
            let side = ctx.atlas().side();
            Ok(TestAtlas {
                texture: Texture::new_coverage_atlas(gpu, pool, side)?,
                generation: ctx.atlas().generation(),
                side,
            })
        }

        fn absorb(
            &mut self,
            gpu: &Arc<Gpu>,
            pool: vk::CommandPool,
            ctx: &TextContext,
            pending: &[PendingGlyph],
        ) -> Result<()> {
            if ctx.atlas().generation() != self.generation {
                self.texture.destroy(gpu);
                self.side = ctx.atlas().side();
                self.texture = Texture::new_coverage_atlas(gpu, pool, self.side)?;
                self.generation = ctx.atlas().generation();
            }
            let regions: Vec<_> = pending.iter().map(PendingGlyph::region).collect();
            self.texture.upload_coverage_regions(gpu, pool, &regions)
        }
    }

    /// Shape `text` through `ctx` into a fresh atlas image and return both.
    fn shape_into_atlas(
        gpu: &Arc<Gpu>,
        pool: vk::CommandPool,
        ctx: &mut TextContext,
        text: &str,
        px: f32,
    ) -> Result<(ShapedRun, TestAtlas)> {
        let (run, pending) = ctx.shape_line(text, px)?;
        let mut atlas = TestAtlas::new(gpu, pool, ctx)?;
        atlas.absorb(gpu, pool, ctx, &pending)?;
        Ok((run, atlas))
    }

    /// Render one string through `ctx` into a `tw`x`th` RGBA target and read it back.
    fn render_string(
        gpu: &Arc<Gpu>,
        pool: vk::CommandPool,
        ctx: &mut TextContext,
        text: &str,
        px: f32,
        tw: u32,
        th: u32,
    ) -> Result<Vec<u8>> {
        let (run, atlas) = shape_into_atlas(gpu, pool, ctx, text, px)?;
        anyhow::ensure!(!run.glyphs.is_empty(), "no glyphs shaped for {text:?}");
        let pixels = render_run(gpu, pool, &atlas, &run, (10.0, 8.0), tw, th);
        atlas.texture.destroy(gpu);
        pixels
    }

    /// Draw a shaped `run` from `atlas` at `origin` into a `tw`x`th` RGBA target and read it back.
    /// The caller owns `atlas` (its texture is not destroyed here).
    fn render_run(
        gpu: &Gpu,
        pool: vk::CommandPool,
        atlas: &TestAtlas,
        run: &ShapedRun,
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
        gpu.run_commands(pool, crate::stats::SubmitSite::OffscreenFrame, |cbuf| {
            target.begin(gpu, cbuf, unorm(BG));
            renderer.draw(gpu, cbuf, set, run, atlas.side, origin, dims, unorm(FG));
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
        let Ok(gpu) = Gpu::new().map(Arc::new) else {
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
        let Ok(gpu) = Gpu::new().map(Arc::new) else {
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
        let (run, pending) = ctx.shape_paragraph(&spans, 360.0, 14.0).unwrap();
        let mut atlas = TestAtlas::new(&gpu, pool, &ctx).unwrap();
        atlas.absorb(&gpu, pool, &ctx, &pending).unwrap();
        assert!(!run.glyphs.is_empty(), "no glyphs shaped for the paragraph");

        let min_y = run.glyphs.iter().map(|g| g.y).min().unwrap();
        let max_y = run.glyphs.iter().map(|g| g.y + g.h as i32).max().unwrap();
        assert!(
            max_y - min_y > 30,
            "expected a multi-line paragraph, got vertical span {min_y}..{max_y}",
        );

        let tw = 400u32;
        let th = (max_y as u32) + 40;
        let pixels = render_run(&gpu, pool, &atlas, &run, (20.0, 10.0), tw, th).unwrap();
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

    /// Residency invariants of the persistent atlas — the properties the whole design exists for:
    ///  1. A long, high-DPI command line (the pathological run-dialog case) lays out every glyph
    ///     WITHOUT erroring — an errored draw would make the modal dialog vanish while it still
    ///     owns the keyboard.
    ///  2. Repeated glyphs share one slot, so 400 copies of a character cost one upload, not 400.
    ///  3. **Re-shaping text whose glyphs are already resident uploads nothing.** This is the
    ///     frame-cost property: a clock ticking seconds re-shapes every second and must not touch
    ///     the GPU for it.
    ///  4. Running out of room grows the atlas and still resolves, bumping the generation so the
    ///     image's owner knows to reallocate — never an error while under `MAX_ATLAS_SIDE`.
    #[test]
    fn the_atlas_keeps_glyphs_resident_across_runs() {
        let Ok(gpu) = Gpu::new().map(Arc::new) else {
            eprintln!("no Vulkan device — skipping the_atlas_keeps_glyphs_resident_across_runs");
            return;
        };
        let pool_ci = vk::CommandPoolCreateInfo::default().queue_family_index(gpu.queue_family);
        let pool = unsafe { gpu.device.create_command_pool(&pool_ci, None) }.unwrap();
        let mut ctx = TextContext::new();

        // (1) Capacity: a long line at a large size lays out without erroring.
        let long = "cargo build --workspace --all-targets 2>&1 | grep -E error".repeat(3);
        let long_spans = [TextSpan {
            text: &long,
            family: SpanFamily::Mono,
            bold: false,
            px: 28.0,
        }];
        let (run, _) = ctx
            .shape_paragraph(&long_spans, 720.0, 28.0)
            .expect("a long high-DPI line must lay out, not error");
        assert!(
            run.glyphs.len() > 100,
            "expected a long wrapped paragraph, got {} glyphs",
            run.glyphs.len()
        );

        // (2) Dedup: 400 copies of one glyph share one slot.
        let repeated = "l".repeat(400);
        let dedup_spans = [TextSpan {
            text: &repeated,
            family: SpanFamily::Mono,
            bold: false,
            px: 14.0,
        }];
        let (run, pending) = ctx
            .shape_paragraph(&dedup_spans, 100_000.0, 14.0)
            .expect("400 identical glyphs must dedup, not overflow");
        assert_eq!(
            run.glyphs.len(),
            400,
            "every instance is still placed, got {}",
            run.glyphs.len()
        );
        assert!(
            pending.len() <= 1,
            "400 copies of one glyph should need at most one upload, got {}",
            pending.len()
        );

        // (3) Residency: shaping the same text again uploads nothing at all.
        let generation = ctx.atlas().generation();
        let (again, pending) = ctx
            .shape_paragraph(&dedup_spans, 100_000.0, 14.0)
            .expect("a re-shape must not fail");
        assert!(
            pending.is_empty(),
            "re-shaping resident text uploaded {} glyph(s) — the atlas is not persisting",
            pending.len()
        );
        assert_eq!(
            ctx.atlas().generation(),
            generation,
            "a re-shape must not disturb the atlas"
        );
        assert_eq!(
            again.glyphs.len(),
            run.glyphs.len(),
            "the re-shaped run differs from the first"
        );

        // (4) Growth: enough large distinct glyphs to exhaust a fresh atlas still resolve, and
        // say so through the generation.
        let mut small = TextContext::new();
        small.atlas = GlyphAtlasIndex::new(64);
        let ascii: String = (33u8..127).map(char::from).collect();
        let grow_spans = [TextSpan {
            text: &ascii,
            family: SpanFamily::Sans,
            bold: false,
            px: 44.0,
        }];
        let (run, pending) = small
            .shape_paragraph(&grow_spans, 100_000.0, 44.0)
            .expect("a full atlas must grow, not error");
        assert!(
            small.atlas().generation() > 0,
            "the atlas should have grown past its 64px start"
        );
        assert!(
            small.atlas().side() > 64,
            "grown side should exceed 64, got {}",
            small.atlas().side()
        );
        assert!(!run.glyphs.is_empty(), "the grown run placed no glyphs");
        assert_eq!(
            pending.len(),
            run.glyphs.len(),
            "after growth every glyph is freshly resident, so all must be uploaded"
        );

        unsafe { gpu.device.destroy_command_pool(pool, None) };
    }

    /// `build_paragraph` tags every laid-out glyph with its source-span index (parallel to
    /// `glyphs`), so a caller can recover a span's ink rectangle to paint an inline background.
    /// Three left-to-right spans on one line must come back in x-order 0 < 1 < 2, and the
    /// middle span's ink must be a non-empty strict horizontal subset of the whole run.
    #[test]
    fn build_paragraph_tags_spans() {
        let Ok(gpu) = Gpu::new().map(Arc::new) else {
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
        let (run, pending) = ctx.shape_paragraph(&spans, 100_000.0, 20.0).unwrap();
        let mut atlas = TestAtlas::new(&gpu, pool, &ctx).unwrap();
        atlas.absorb(&gpu, pool, &ctx, &pending).unwrap();
        assert_eq!(
            run.spans.len(),
            run.glyphs.len(),
            "spans must be parallel to glyphs"
        );

        // Horizontal ink extent of the glyphs tagged with a given span index.
        let span_x = |idx: u32| -> Option<(i32, i32)> {
            run.glyphs
                .iter()
                .zip(&run.spans)
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
    /// A repeated measurement must not re-shape: the same width comes back with the
    /// shape counter untouched. Layout code calls this like a getter (the panel
    /// measures its clock several times per frame), so a miss-every-time cache is a
    /// full cosmic-text shape per call.
    #[test]
    fn a_repeated_measurement_is_cached() {
        let first = measure_line_width_weighted("cached measurement", 16.0, false);
        assert!(first > 0., "an empty width means nothing was shaped");

        let shapes = crate::stats::shapes();
        let again = measure_line_width_weighted("cached measurement", 16.0, false);
        assert_eq!(
            again, first,
            "the cached width differs from the measured one"
        );
        assert_eq!(
            crate::stats::shapes(),
            shapes,
            "a repeated measurement re-shaped instead of hitting the cache"
        );

        // Weight and size are part of the key: bold is wider at the same size.
        let bold = measure_line_width_weighted("cached measurement", 16.0, true);
        assert!(
            bold > first,
            "bold measured {bold} vs regular {first} — the weight is not in the key"
        );
        assert!(
            crate::stats::shapes() > shapes,
            "the bold variant was not shaped"
        );
    }

    /// One shared [`FontSystem`] means one shared mutex, so a shape that panics while holding it
    /// poisons the database for everyone. Propagating that would turn a single bad run into "this
    /// session can never draw text again" — every later lock panicking in turn. Recovery keeps the
    /// contract that a text failure costs glyphs, not the compositor.
    ///
    /// The panic this deliberately provokes prints a backtrace during the run; that is the test
    /// working, not a failure.
    #[test]
    fn a_panic_while_shaping_does_not_brick_text_for_the_rest_of_the_process() {
        let died = std::thread::spawn(|| {
            let _guard = fonts();
            panic!("pretend a shape blew up mid-buffer");
        })
        .join();
        assert!(died.is_err(), "the thread was supposed to panic");

        // An unmeasured string, so this goes through the lock rather than the memo cache.
        let width = measure_line_width("after the poisoning", 16.0);
        assert!(
            width > 0.,
            "the font database stayed poisoned — later text got no glyphs"
        );
    }

    /// A repeated wrap must not re-shape either. This one matters more than the
    /// measurement: a wrap costs one shape per attempt plus a re-shaping loop to fit the
    /// ellipsis, and a notification card runs it once per hit-test and again per
    /// render-element collection — so a pointer crossing the message list used to re-wrap
    /// every visible body on every motion event.
    #[test]
    fn a_repeated_wrap_is_cached() {
        let body = "a notification body long enough that it has to wrap onto more than \
                    one line and then be ellipsized when the budget runs out";
        let first = wrap_lines_weighted(body, 14.0, false, 180., 2);
        assert_eq!(first.len(), 2, "the body should have filled the budget");
        assert!(
            first.last().is_some_and(|l| l.ends_with('\u{2026}')),
            "the clamped last line should be ellipsized: {first:?}"
        );

        let shapes = crate::stats::shapes();
        let again = wrap_lines_weighted(body, 14.0, false, 180., 2);
        assert_eq!(again, first, "the cached wrap differs from the shaped one");
        assert_eq!(
            crate::stats::shapes(),
            shapes,
            "a repeated wrap re-shaped instead of hitting the cache"
        );

        // The width is part of the key — the same body in a wider column wraps
        // differently, and must not come back from the cache.
        let wider = wrap_lines_weighted(body, 14.0, false, 360., 2);
        assert_ne!(wider, first, "the wrap width is not in the key");
        assert!(
            crate::stats::shapes() > shapes,
            "the wider wrap was not shaped"
        );

        // As is the line budget: the same text and width, more room, no ellipsis.
        let shapes = crate::stats::shapes();
        let taller = wrap_lines_weighted(body, 14.0, false, 180., 6);
        assert!(
            taller.len() > first.len(),
            "the line budget is not in the key: {taller:?}"
        );
        assert!(
            crate::stats::shapes() > shapes,
            "the taller wrap was not shaped"
        );
    }
}
