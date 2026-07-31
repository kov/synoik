//! Icon rasterization — symbolic (recolored) and full-color (app) icons.
//!
//! Two caches share this module and the `resvg`/`MemoryBuffer` core:
//!
//! - [`IconCache`] draws GNOME's *symbolic* icons: monochrome SVGs from the icon theme
//!   (`foo-symbolic`), recolored to the current foreground color by using the render's **alpha as a
//!   coverage mask** — the same tint model as our glyphs. Its resolver is a deliberate spec-subset
//!   (a direct `<theme>/{symbolic,scalable}/<category>/` walk, no `index.theme` inheritance or size
//!   directories), which covers the panel's symbolic set.
//! - [`AppIconCache`] draws *full-color application icons* ([`AppIconRef`] from
//!   `g_app_info_get_icon()`): resolved through the `freedesktop-icons` crate (real theme
//!   inheritance + size directories) and decoded **keeping their own colors** — raster (PNG/…) via
//!   the `image` crate, SVG via `resvg`. Falls back to `application-x-executable` (GNOME's
//!   `St.Icon` fallback).
//! - [`ImageCache`] draws images an *app* pointed us at ([`ImageSource`]): album art, local or
//!   fetched. Same decode core, but a separate cache because almost everything around it differs —
//!   no themed fallback, its own worker so a slow fetch cannot stall icon decodes, and an
//!   open-ended key space that has to be evicted.
//!
//! Both return a premultiplied `Abgr8888` [`MemoryBuffer`] tagged at the output
//! scale (never `1.` — the buffer-scale-tag trap) that the caller composites like
//! any other CPU bitmap.
//!
//! Accepted limitation: `freedesktop-icons` scans installed themes once per
//! process, so a *theme installed mid-session* is invisible until restart (new
//! *icons inside* existing themes are found via per-lookup probing); GNOME
//! rescans live.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;

use anyhow::Context as _;
use calloop::channel::Sender as CalloopSender;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::{ContextId, Renderer as _};
use smithay::utils::{Scale, Size, Transform};

use crate::app_system::AppIconRef;
use crate::image_source::{
    remote_fetch_enabled, remote_is_permitted, ImageSource, FETCH_TIMEOUT, MAX_IMAGE_BYTES,
};
use crate::render_helpers::memory::MemoryBuffer;
use crate::render_helpers::texture::TextureBuffer;
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::utils::to_physical_precise_round;

/// The icon-theme subdirectories that hold symbolic icons, and the categories
/// within them. Searched in order; the first match wins.
const SUBDIRS: &[&str] = &["symbolic", "scalable"];
const CATEGORIES: &[&str] = &[
    "status",
    "actions",
    "devices",
    "ui",
    "categories",
    "legacy",
    "apps",
    "places",
    "emblems",
    "mimetypes",
];

/// Hands each [`IconCache`] a distinct `generation`. See the field for what it guards.
static NEXT_ICON_CACHE_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Resolves + rasterizes symbolic icons on demand, caching the resolved paths and
/// the uploaded textures.
pub struct IconCache {
    /// Themes to search, in order (e.g. the configured theme, then Adwaita/hicolor).
    themes: Vec<String>,
    /// Unique to this instance, stamped on every request. An icon-theme change replaces the whole
    /// `IconCache` rather than invalidating it, so a rasterization started by the *previous* cache
    /// can still be delivered to the loop afterwards — and would otherwise be filed, under a key
    /// that matches, into a cache built for a different theme.
    generation: u64,
    resolved: RefCell<HashMap<String, Option<PathBuf>>>,
    /// Rasterized pixels waiting to be uploaded, and negatives (an icon no theme provides), so a
    /// miss is not re-queued every frame. Populated by [`apply_rasterized`]; drained by
    /// [`texture`], which is the only place a `VulkanRenderer` exists.
    ///
    /// [`apply_rasterized`]: Self::apply_rasterized
    /// [`texture`]: Self::texture
    buffers: RefCell<HashMap<SymbolicKey, Option<MemoryBuffer>>>,
    /// Keys currently on the worker, so a miss is queued once rather than once a frame.
    in_flight: RefCell<HashSet<SymbolicKey>>,
    /// Request sink to the rasterize worker; `None` in headless tests, where
    /// [`texture`](Self::texture) falls back to rasterizing inline.
    raster_tx: Option<mpsc::Sender<SymbolicRequest>>,
    /// Uploaded icons, keyed by [`icon_key`]. Symbolic icons are *elements*, rebuilt
    /// from scratch on every frame that draws them, and each upload is a synchronous
    /// submit + fence-wait — ~1.7ms apiece on the Venus queue, independent of size.
    /// One quick-settings popover frame asks for nine, so an open popover paid ~13ms
    /// a frame to re-upload identical pixels. Caching the texture makes a redraw free.
    ///
    /// The rasterized pixels live in `buffers` above rather than here: they arrive from the
    /// worker and are consumed by the first upload, so the two maps hold different stages of the
    /// same icon and never both hold one.
    textures: RefCell<HashMap<SymbolicKey, TextureBuffer<VkTexture>>>,
    /// The *previous* cache's uploads, kept servable until each is re-rasterized.
    ///
    /// An icon-theme change replaces the whole cache, and re-rasterizing goes through the
    /// worker — so a brand-new cache has nothing to draw and every symbolic icon on screen
    /// (panel status, quick-settings toggles, calendar chevrons) vanishes for a frame or
    /// more. Inheriting the outgoing textures ([`adopt_textures_from`]) makes the artifact
    /// briefly-old icons instead of briefly-absent ones. Entries leave as their replacements
    /// upload.
    ///
    /// [`adopt_textures_from`]: Self::adopt_textures_from
    stale_textures: RefCell<HashMap<SymbolicKey, TextureBuffer<VkTexture>>>,
    /// Identifies the renderer `textures` were uploaded to; a mismatch drops them all
    /// (they belong to a device that is gone). Same guard the widget bake caches use.
    context: RefCell<Option<ContextId<VkTexture>>>,
}

impl IconCache {
    /// Search `theme` first, then the usual `Adwaita`/`hicolor` fallbacks.
    pub fn new(theme: impl Into<String>) -> Self {
        let mut themes = vec![theme.into()];
        for fallback in ["Adwaita", "hicolor"] {
            if !themes.iter().any(|t| t == fallback) {
                themes.push(fallback.to_string());
            }
        }
        Self {
            themes,
            generation: NEXT_ICON_CACHE_GENERATION.fetch_add(1, Ordering::Relaxed),
            resolved: RefCell::new(HashMap::new()),
            buffers: RefCell::new(HashMap::new()),
            in_flight: RefCell::new(HashSet::new()),
            raster_tx: None,
            textures: RefCell::new(HashMap::new()),
            stale_textures: RefCell::new(HashMap::new()),
            context: RefCell::new(None),
        }
    }

    /// Take over `previous`'s uploaded icons as stale ones, so a theme change keeps
    /// drawing the old pixels until each replacement rasterizes — see `stale_textures`.
    /// The textures belong to a renderer, so the context they were uploaded to comes
    /// with them; a later mismatch drops both maps together.
    pub fn adopt_textures_from(&mut self, previous: &IconCache) {
        *self.stale_textures.borrow_mut() = previous.textures.borrow().clone();
        self.context
            .borrow_mut()
            .clone_from(&previous.context.borrow());
    }

    /// Route rasterization to `tx`'s worker instead of doing it inline.
    ///
    /// Separate from construction because an icon-theme change *replaces* the whole cache and the
    /// worker outlives it — one thread for the session, re-pointed at each new cache.
    pub fn set_worker(&mut self, tx: mpsc::Sender<SymbolicRequest>) {
        self.raster_tx = Some(tx);
    }

    /// File a finished rasterization. `true` when it changed anything, i.e. when a redraw is worth
    /// queueing. Results stamped with another cache's generation are dropped — see `generation`.
    pub fn apply_rasterized(&mut self, done: SymbolicRasterized) -> bool {
        if done.generation != self.generation {
            return false;
        }
        self.in_flight.get_mut().remove(&done.key);
        self.buffers.get_mut().insert(done.key, done.buffer);
        true
    }

    /// Route rasterization to a channel the test drains itself, so the async miss path
    /// (the one that can draw nothing) is exercised without a real worker thread.
    #[cfg(test)]
    pub(crate) fn wire_test_worker(&mut self) -> mpsc::Receiver<SymbolicRequest> {
        let (tx, rx) = mpsc::channel();
        self.raster_tx = Some(tx);
        rx
    }

    /// How many icons this cache has uploaded, and how many it inherited from the cache it
    /// replaced. Tests only: a theme change leaving both at zero is the blank frame.
    #[cfg(test)]
    pub(crate) fn texture_counts(&self) -> (usize, usize) {
        (
            self.textures.borrow().len(),
            self.stale_textures.borrow().len(),
        )
    }

    /// Queue `key` for the worker, once. No-op without a worker, and the caller then rasterizes
    /// inline.
    fn request(&self, name: &str, key: &SymbolicKey, scale: f64, color: [f32; 4]) {
        let Some(tx) = self.raster_tx.as_ref() else {
            return;
        };
        if !self.in_flight.borrow_mut().insert(key.clone()) {
            return;
        }
        let req = SymbolicRequest {
            key: key.clone(),
            name: name.to_owned(),
            themes: self.themes.clone(),
            scale,
            color,
            generation: self.generation,
        };
        if tx.send(req).is_err() {
            // The worker is gone; stop pretending it will answer, or this key never retries.
            self.in_flight.borrow_mut().remove(key);
        }
    }

    /// The file path for a symbolic icon `name` (with or without the trailing
    /// `-symbolic`), or `None` if the theme doesn't provide it.
    ///
    /// Interior-mutable (the cache is shared behind `&`, so both the panel and the
    /// popover can rasterize from the render path without a `&mut` borrow).
    pub fn resolve(&self, name: &str) -> Option<PathBuf> {
        if let Some(hit) = self.resolved.borrow().get(name) {
            return hit.clone();
        }
        let path = resolve_symbolic(name, &self.themes);
        self.resolved
            .borrow_mut()
            .insert(name.to_string(), path.clone());
        path
    }

    /// Whether this cache can draw symbolic icon `name` at all — the theme provides it, or it is
    /// one of our embedded ones.
    ///
    /// Use this, never [`texture`](Self::texture), to *choose between candidate names*. A
    /// `texture` miss is ambiguous: with a raster worker it also means "queued, not here yet", so
    /// picking by it makes the first paint fall through to a later candidate and swap once the
    /// earlier one uploads. This is a synchronous, memoized path probe with no such state.
    pub fn provides(&self, name: &str) -> bool {
        embedded_icon(name).is_some() || self.resolve(name).is_some()
    }

    /// Rasterize a recolored icon: `name` at `px` physical pixels (square), tinted to
    /// straight-RGBA `color`. `None` if the theme doesn't provide it.
    ///
    /// Uncached by design — see the note on [`textures`](Self::textures). The only
    /// caller is [`texture`](Self::texture), on a miss, so an icon already on the GPU
    /// never reaches here.
    fn rasterize(&self, name: &str, px: u32, scale: f64, color: [f32; 4]) -> Option<MemoryBuffer> {
        if embedded_icon(name).is_none() {
            // Populate the path cache on the way through, which is what `resolve` is for.
            self.resolve(name)?;
        }
        rasterize_symbolic_in(name, &self.themes, px, color, scale)
    }

    /// The uploaded texture for a symbolic icon — the form every caller wants, since
    /// a symbolic icon is only ever composited. Cached, so drawing the same icon on a
    /// later frame costs no submit at all. `None` if it can't be resolved or the
    /// upload fails (never cached, so the next frame retries).
    pub fn texture(
        &self,
        renderer: &mut VulkanRenderer,
        name: &str,
        logical_px: f64,
        scale: f64,
        color: [f32; 4],
    ) -> Option<TextureBuffer<VkTexture>> {
        let context = renderer.context_id();
        if self.context.borrow().as_ref() != Some(&context) {
            self.textures.borrow_mut().clear();
            // The inherited ones belong to that same dead device.
            self.stale_textures.borrow_mut().clear();
            *self.context.borrow_mut() = Some(context);
        }

        let key = icon_key(name, logical_px, scale, color);
        if let Some(tb) = self.textures.borrow().get(&key) {
            return Some(tb.clone());
        }

        // Rasterized pixels waiting for a renderer: this is the only place one exists.
        let ready = self.buffers.borrow_mut().remove(&key);
        let buffer = match ready {
            // A negative — no theme provides it. Put it back so the miss below does not re-queue.
            Some(None) => {
                self.buffers.borrow_mut().insert(key, None);
                return None;
            }
            Some(Some(buffer)) => buffer,
            None if self.raster_tx.is_some() => {
                // Off to the worker; nothing to draw this frame. GNOME does the same — a
                // `St.Icon` miss returns an *invisible actor of the right size* and fills the
                // texture in asynchronously (`st-texture-cache.c` `st_texture_cache_load_gicon`,
                // "the texture will be filled asynchronously"), deduping outstanding requests the
                // way `in_flight` does. Callers size themselves from `logical_px`, never from us,
                // so only the pixels arrive late — the layout does not move.
                self.request(name, &key, scale, color);
                // Not blank while we wait: if a previous cache had drawn this icon, keep
                // showing those pixels until the replacement uploads.
                return self.stale_textures.borrow().get(&key).cloned();
            }
            // No worker (headless tests): the old inline path, unchanged.
            None => self.rasterize(name, key.1, scale, color)?,
        };

        match TextureBuffer::from_memory_buffer(renderer, &buffer) {
            Ok(tb) => {
                self.stale_textures.borrow_mut().remove(&key);
                self.textures.borrow_mut().insert(key, tb.clone());
                Some(tb)
            }
            Err(err) => {
                tracing::error!("error uploading icon {name:?}: {err:#}");
                None
            }
        }
    }
}

/// A symbolic icon's identity: the name, the *physical* pixel size (so scale is folded in), and
/// the quantized tint — the three things that change the pixels.
pub type SymbolicKey = (String, u32, u32);

/// Handed to the rasterize worker. Carries the theme list rather than borrowing the cache, so the
/// worker outlives an icon-theme change without holding it alive.
pub struct SymbolicRequest {
    key: SymbolicKey,
    name: String,
    themes: Vec<String>,
    scale: f64,
    color: [f32; 4],
    generation: u64,
}

/// A finished rasterization on its way back to the loop, for [`IconCache::apply_rasterized`].
/// `None` means no theme provides the icon; it is cached as a negative so the probe — up to a few
/// hundred `stat`s across themes and categories — happens once rather than every frame.
pub struct SymbolicRasterized {
    key: SymbolicKey,
    buffer: Option<MemoryBuffer>,
    generation: u64,
}

#[cfg(test)]
impl SymbolicRequest {
    pub(crate) fn key(&self) -> SymbolicKey {
        self.key.clone()
    }
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }
}

#[cfg(test)]
impl SymbolicRasterized {
    pub(crate) fn for_test(
        key: SymbolicKey,
        buffer: Option<MemoryBuffer>,
        generation: u64,
    ) -> Self {
        Self {
            key,
            buffer,
            generation,
        }
    }
}

/// Hands `IconCache` a thread that resolves and rasterizes symbolic icons, returning the request
/// sink for [`IconCache::set_worker`]. Finished icons arrive on `result_tx`, which the caller
/// registers as a calloop source feeding [`IconCache::apply_rasterized`].
///
/// This exists because resolving *and* rasterizing used to happen inline on the compositor thread,
/// inside element collection: an unresolvable name costs `base_dirs × themes × 2 × 10` `stat`s
/// before it gives up, and a resolved one adds a file read plus an SVG parse (~53 µs measured per
/// icon warm, ~1.3 ms for a quick-settings popover's worth, and page-cache-cold reads are much
/// worse — the font prewarm found a 35× swing). None of it belongs on the thread that has 16.67 ms
/// to hand a frame to KMS.
pub fn spawn_symbolic_worker(
    result_tx: CalloopSender<SymbolicRasterized>,
) -> Option<mpsc::Sender<SymbolicRequest>> {
    let (req_tx, req_rx) = mpsc::channel::<SymbolicRequest>();
    let spawned = std::thread::Builder::new()
        .name("symbolic-icon".to_owned())
        .spawn(move || {
            // Ends when the sender (held by `IconCache`) is dropped.
            for req in req_rx {
                // Theme SVGs are not ours; a malformed one that panics the rasterizer must become
                // a negative result, not a dead worker. Same rule as the app-icon worker.
                let buffer = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    rasterize_symbolic_in(&req.name, &req.themes, req.key.1, req.color, req.scale)
                }))
                .unwrap_or(None);
                let done = SymbolicRasterized {
                    key: req.key,
                    buffer,
                    generation: req.generation,
                };
                if result_tx.send(done).is_err() {
                    break;
                }
            }
        });
    match spawned {
        Ok(_) => Some(req_tx),
        Err(err) => {
            tracing::warn!("error spawning the symbolic-icon worker, rasterizing inline: {err:?}");
            None
        }
    }
}

/// Resolve `name` against `themes` and rasterize it. The whole of what the worker does, and what
/// [`IconCache::rasterize`] does inline when there is no worker.
fn rasterize_symbolic_in(
    name: &str,
    themes: &[String],
    px: u32,
    color: [f32; 4],
    scale: f64,
) -> Option<MemoryBuffer> {
    let result = if let Some(bytes) = embedded_icon(name) {
        rasterize_symbolic_bytes(bytes, px, color, scale)
    } else {
        rasterize_symbolic(&resolve_symbolic(name, themes)?, px, color, scale)
    };
    result
        .map_err(|err| tracing::warn!("failed to rasterize icon {name:?}: {err:#}"))
        .ok()
}

/// [`IconCache::textures`]'s key: the name, the *physical* pixel size (so scale is
/// folded in), and the quantized tint — the three things that change the pixels.
fn icon_key(name: &str, logical_px: f64, scale: f64, color: [f32; 4]) -> SymbolicKey {
    let px: u32 = to_physical_precise_round::<i32>(scale, logical_px).max(1) as u32;
    (name.to_string(), px, color_key(color))
}

/// Icons gnome-shell ships inside its own gresource — they exist in NO icon
/// theme on disk, so they are bundled from the 50.1 reference checkout
/// (`data/icons/scalable/`, GPLv2+) into `resources/icons/`.
/// `notification-collapse-symbolic` is our derived name for the expand chevron
/// rotated 180° — gnome-shell rotates the button actor instead
/// (`js/ui/messageList.js:635-638`); we bake the rotation into the SVG.
fn embedded_icon(name: &str) -> Option<&'static [u8]> {
    match name {
        "no-notifications-symbolic" => Some(include_bytes!(
            "../../resources/icons/no-notifications-symbolic.svg"
        )),
        "notification-expand-symbolic" => Some(include_bytes!(
            "../../resources/icons/notification-expand-symbolic.svg"
        )),
        "notification-collapse-symbolic" => Some(include_bytes!(
            "../../resources/icons/notification-collapse-symbolic.svg"
        )),
        "message-indicator-symbolic" => Some(include_bytes!(
            "../../resources/icons/message-indicator-symbolic.svg"
        )),
        "group-collapse-symbolic" => Some(include_bytes!(
            "../../resources/icons/group-collapse-symbolic.svg"
        )),
        "carousel-arrow-next-symbolic" => Some(include_bytes!(
            "../../resources/icons/carousel-arrow-next-symbolic.svg"
        )),
        "carousel-arrow-previous-symbolic" => Some(include_bytes!(
            "../../resources/icons/carousel-arrow-previous-symbolic.svg"
        )),
        "preview-close-symbolic" => Some(include_bytes!(
            "../../resources/icons/preview-close-symbolic.svg"
        )),
        _ => None,
    }
}

/// Quantize a straight-RGBA color to a cache key.
fn color_key(color: [f32; 4]) -> u32 {
    let q = |v: f32| (v.clamp(0., 1.) * 255.).round() as u32;
    (q(color[0]) << 24) | (q(color[1]) << 16) | (q(color[2]) << 8) | q(color[3])
}

/// Search the icon themes for a symbolic icon file.
pub fn resolve_symbolic(name: &str, themes: &[String]) -> Option<PathBuf> {
    let file = if name.ends_with(".svg") {
        name.to_string()
    } else {
        format!("{name}.svg")
    };
    for base in icon_base_dirs() {
        for theme in themes {
            for sub in SUBDIRS {
                for cat in CATEGORIES {
                    let path = base.join(theme).join(sub).join(cat).join(&file);
                    if path.is_file() {
                        return Some(path);
                    }
                }
            }
        }
    }
    None
}

/// Base directories that hold icon themes, most-specific first (per the
/// freedesktop icon-theme spec, minus the `index.theme` inheritance handling).
fn icon_base_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        dirs.push(home.join(".local/share/icons"));
        dirs.push(home.join(".icons"));
    }
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());
    for dir in data_dirs.split(':').filter(|d| !d.is_empty()) {
        dirs.push(PathBuf::from(dir).join("icons"));
    }
    dirs.push(PathBuf::from("/usr/share/pixmaps"));
    dirs
}

/// Rasterize a symbolic SVG at `px` (square, physical) and recolor it to `color`
/// (straight RGBA) using the render's alpha as coverage. The returned buffer is
/// premultiplied `Abgr8888` ([R,G,B,A] in memory) and — because its pixels are
/// physical — tagged at the output `scale`, never `1.` (the buffer-scale-tag trap).
pub fn rasterize_symbolic(
    path: &Path,
    px: u32,
    color: [f32; 4],
    scale: f64,
) -> anyhow::Result<MemoryBuffer> {
    let data = std::fs::read(path).with_context(|| format!("reading icon {}", path.display()))?;
    rasterize_symbolic_bytes(&data, px, color, scale)
}

/// [`rasterize_symbolic`] for in-memory SVG data (the bundled gresource icons).
pub fn rasterize_symbolic_bytes(
    data: &[u8],
    px: u32,
    color: [f32; 4],
    scale: f64,
) -> anyhow::Result<MemoryBuffer> {
    let px = px.max(1);
    let pixmap = render_svg_pixmap(data, px, SvgFit::Stretch)?;

    // Recolor: the render is a monochrome shape; take its (premultiplied) alpha as
    // coverage and paint the target color through it.
    let src = pixmap.data();
    let mut out = vec![0u8; (px * px * 4) as usize];
    for (dst, s) in out.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
        let cov = f32::from(s[3]) / 255. * color[3];
        dst[0] = (color[0] * cov * 255.).round().clamp(0., 255.) as u8;
        dst[1] = (color[1] * cov * 255.).round().clamp(0., 255.) as u8;
        dst[2] = (color[2] * cov * 255.).round().clamp(0., 255.) as u8;
        dst[3] = (cov * 255.).round().clamp(0., 255.) as u8;
    }

    Ok(MemoryBuffer::new(
        out,
        Fourcc::Abgr8888,
        Size::from((px as i32, px as i32)),
        Scale::from(scale),
        Transform::Normal,
    ))
}

/// How to fit an SVG's viewBox into the target square.
#[derive(Clone, Copy)]
enum SvgFit {
    /// Fill the square on both axes independently (symbolic icons are square, so
    /// this is a no-op stretch that preserves the existing behavior).
    Stretch,
    /// Uniform scale + center (`min(sx, sy)`), so non-square app icons keep their
    /// aspect ratio instead of being distorted.
    Contain,
}

/// Parse and render an SVG into a `px`×`px` premultiplied-RGBA pixmap. Shared by
/// the symbolic (recolored) and app (full-color) paths.
fn render_svg_pixmap(
    data: &[u8],
    px: u32,
    fit: SvgFit,
) -> anyhow::Result<resvg::tiny_skia::Pixmap> {
    use resvg::{tiny_skia, usvg};

    let px = px.max(1);
    let opt = usvg::Options::default();
    let tree = usvg::Tree::from_data(data, &opt).context("parsing icon SVG")?;

    let mut pixmap = tiny_skia::Pixmap::new(px, px).context("allocating icon pixmap")?;
    let size = tree.size();
    let transform = match fit {
        SvgFit::Stretch => {
            tiny_skia::Transform::from_scale(px as f32 / size.width(), px as f32 / size.height())
        }
        SvgFit::Contain => {
            let s = (px as f32 / size.width()).min(px as f32 / size.height());
            let tx = (px as f32 - size.width() * s) / 2.;
            let ty = (px as f32 - size.height() * s) / 2.;
            tiny_skia::Transform::from_row(s, 0., 0., s, tx, ty)
        }
    };
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    Ok(pixmap)
}

/// The cache key for a decoded app icon: descriptor + logical + physical size.
type IconKey = (AppIconRef, u16, u32);

/// A decode request handed to the worker thread — all fields are `Send`.
pub struct IconRequest {
    key: IconKey,
    icon: AppIconRef,
    logical_px: f64,
    scale: f64,
    theme: String,
    generation: u64,
}

/// A finished decode delivered from the worker back to the main loop, routed to
/// [`AppIconCache::apply_decoded`]. `None` is a resolve/decode failure — cached as a
/// negative so a broken icon isn't re-requested every frame.
pub struct IconDecoded {
    key: IconKey,
    buffer: Option<MemoryBuffer>,
    generation: u64,
}

/// Resolves + decodes **full-color application icons**, caching the decoded buffers
/// (positives *and* negatives, so a missing icon isn't re-probed every frame).
/// Resolution goes through the `freedesktop-icons` crate (real theme inheritance +
/// size directories); decode keeps the icon's own colors.
///
/// The decode (SVG render / raster decode at the target size) is the expensive part
/// and is offloaded to a worker thread ([`spawn_worker`](Self::spawn_worker)): a miss
/// enqueues a request and returns `None` (the caller draws no icon this frame), and
/// the finished buffer lands via [`apply_decoded`](Self::apply_decoded) on the main
/// loop, which queues a redraw. Without a worker (headless tests) the decode is
/// synchronous. A `generation` counter, bumped on every cache-invalidating change,
/// drops results that a theme swap / `clear` outran.
pub struct AppIconCache {
    /// The current icon theme (`org.gnome.desktop.interface icon-theme`).
    theme: String,
    buffers: RefCell<HashMap<IconKey, Option<MemoryBuffer>>>,
    /// What `buffers` held before the last invalidation. An invalidated icon is not
    /// gone, it is *out of date*: the replacement decodes off-thread, and drawing
    /// nothing until it lands blanks every tile for a frame or more. So the old
    /// pixels stay servable until their replacement arrives — the visible artifact
    /// is briefly-old icons rather than briefly-absent ones. Entries leave here as
    /// each decode lands ([`apply_decoded`](Self::apply_decoded)).
    stale: RefCell<HashMap<IconKey, Option<MemoryBuffer>>>,
    /// Bumped on every cache-invalidating change (theme swap / `clear`); stamped on
    /// each request so a result that lands after an invalidation is dropped.
    generation: u64,
    /// Requests currently on the worker, so a miss isn't re-queued every frame.
    /// Cleared on invalidation (else a key stuck here would never re-resolve).
    in_flight: RefCell<HashSet<IconKey>>,
    /// Memoized [`provides`](Self::provides) answers. The probe walks the theme's size
    /// directories, so a per-frame caller (the notification header picks its icon on every
    /// card's every render) must not repeat it. Invalidated with everything else.
    provided: RefCell<HashMap<(AppIconRef, u16), bool>>,
    /// Request sink to the decode worker; `None` before [`spawn_worker`] (headless
    /// tests), where decoding falls back to synchronous.
    ///
    /// [`spawn_worker`]: Self::spawn_worker
    decode_tx: Option<mpsc::Sender<IconRequest>>,
}

impl AppIconCache {
    pub fn new(theme: impl Into<String>) -> Self {
        Self {
            theme: theme.into(),
            buffers: RefCell::new(HashMap::new()),
            stale: RefCell::new(HashMap::new()),
            generation: 0,
            in_flight: RefCell::new(HashSet::new()),
            provided: RefCell::new(HashMap::new()),
            decode_tx: None,
        }
    }

    /// Start the decode worker; give it the sink that delivers finished decodes back
    /// to the main loop (register `result_tx`'s receiver as a calloop source calling
    /// [`apply_decoded`](Self::apply_decoded)). Until this is called, [`buffer`] decodes
    /// synchronously.
    ///
    /// [`buffer`]: Self::buffer
    pub fn spawn_worker(&mut self, result_tx: CalloopSender<IconDecoded>) {
        let (req_tx, req_rx) = mpsc::channel::<IconRequest>();
        self.decode_tx = Some(req_tx);
        if let Err(err) = std::thread::Builder::new()
            .name("app-icon-decode".to_owned())
            .spawn(move || {
                // Ends when the request sender (held by `AppIconCache`) is dropped.
                for req in req_rx {
                    // Icons are semi-untrusted app content; a malformed SVG that
                    // panics the rasterizer must not take down the worker (it becomes
                    // a negative result instead).
                    let buffer = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        render_icon(&req.theme, &req.icon, req.logical_px, req.scale, req.key.2)
                    }))
                    .unwrap_or(None);
                    let decoded = IconDecoded {
                        key: req.key,
                        buffer,
                        generation: req.generation,
                    };
                    if result_tx.send(decoded).is_err() {
                        break;
                    }
                }
            })
        {
            tracing::warn!(
                "could not spawn the app-icon decode thread: {err}; decoding synchronously"
            );
            self.decode_tx = None;
        }
    }

    /// Insert a finished decode. Returns whether the cache changed (→ redraw). A
    /// result whose generation the cache has moved past (a theme swap / `clear` fired
    /// while it was in flight) is dropped, but its in-flight slot is always freed so
    /// the icon re-resolves.
    pub fn apply_decoded(&mut self, decoded: IconDecoded) -> Option<IconKey> {
        self.in_flight.get_mut().remove(&decoded.key);
        if decoded.generation != self.generation {
            return None;
        }
        // This key is current again, so the superseded pixels must go — otherwise a
        // later invalidation would demote the *fresh* buffer and find the ancient one
        // still sitting underneath it.
        self.stale.get_mut().remove(&decoded.key);
        self.buffers
            .get_mut()
            .insert(decoded.key.clone(), decoded.buffer);
        Some(decoded.key)
    }

    /// Bump the generation and drop everything in-flight — every cache-invalidating
    /// change routes through here, so a decode started before it lands stale.
    fn invalidate(&mut self) {
        self.generation += 1;
        // Demote rather than drop, so `buffer` can keep serving the old pixels until
        // the replacement decode lands. Extending (not replacing) keeps the oldest
        // still-unreplaced entry across back-to-back invalidations.
        let outgoing = std::mem::take(self.buffers.get_mut());
        self.stale.get_mut().extend(outgoing);
        self.in_flight.get_mut().clear();
        self.provided.get_mut().clear();
    }

    /// Swap the icon theme, clearing the cache if it actually changed.
    pub fn set_theme(&mut self, theme: &str) {
        if self.theme != theme {
            self.theme = theme.to_string();
            self.invalidate();
        }
    }

    /// Drop all cached buffers — e.g. on `installed-changed`, since a newly
    /// installed app's icon (or its previously-cached negative) may now resolve.
    pub fn clear(&mut self) {
        self.invalidate();
    }

    /// Whether the decode worker is wired ([`spawn_worker`](Self::spawn_worker) ran).
    /// A prewarm pass gates on this: before the worker exists, [`buffer`](Self::buffer)
    /// would decode inline on the main thread — the very stall prewarming avoids.
    pub fn has_worker(&self) -> bool {
        self.decode_tx.is_some()
    }

    /// A full-color icon buffer for `icon` at `logical_px` (square), rendered at the
    /// output `scale` with the icon's own colors, falling back to
    /// `application-x-executable`. Cached by (descriptor, logical size, physical size).
    ///
    /// With a worker wired, a miss is decoded off-thread: this returns `None` (the
    /// caller draws no icon this frame) and the buffer lands later via
    /// [`apply_decoded`](Self::apply_decoded). Without a worker it decodes inline.
    /// Interior-mutable like [`IconCache::buffer`] so the render path and UI can
    /// rasterize from a shared `&`.
    pub fn buffer(&self, icon: &AppIconRef, logical_px: f64, scale: f64) -> Option<MemoryBuffer> {
        self.decode(icon, logical_px, scale)
    }

    /// Whether the theme provides `icon` **itself** — i.e. [`buffer`](Self::buffer) would draw it
    /// rather than silently substituting `application-x-executable`.
    ///
    /// [`buffer`] never returns `None` for a missing themed name (that is the point of the
    /// fallback), so a caller that wants to know "did this app's own icon resolve?" cannot learn
    /// it from the buffer. The notification header needs exactly that: St tries the bare name and
    /// only then its own *symbolic* fallback, which is a different glyph from this cache's
    /// full-colour one.
    ///
    /// Memoized — the underlying probe walks the theme's size directories.
    pub fn provides(&self, icon: &AppIconRef, logical_px: f64, scale: f64) -> bool {
        let key = (icon.clone(), (logical_px.round() as u16).max(1));
        if let Some(hit) = self.provided.borrow().get(&key) {
            return *hit;
        }
        let found = resolve_icon(&self.theme, icon, logical_px, scale).is_some();
        self.provided.borrow_mut().insert(key, found);
        found
    }

    fn decode(&self, icon: &AppIconRef, logical_px: f64, scale: f64) -> Option<MemoryBuffer> {
        // Keying on both logical and physical size avoids two different logical
        // sizes at different scales colliding on the same physical px and picking
        // a wrong-resolution theme source. Like the symbolic cache, two very close
        // fractional scales can still alias to one entry (<1px logical drift).
        let logical = (logical_px.round() as u16).max(1);
        let px = to_physical_precise_round::<i32>(scale, logical_px).max(1) as u32;
        let key = (icon.clone(), logical, px);
        if let Some(cached) = self.buffers.borrow().get(&key) {
            return cached.clone();
        }
        match &self.decode_tx {
            // Async: hand the decode to the worker (once — dedup on the in-flight
            // set), draw nothing until it lands.
            Some(tx) => {
                if self.in_flight.borrow_mut().insert(key.clone()) {
                    let req = IconRequest {
                        key: key.clone(),
                        icon: icon.clone(),
                        logical_px,
                        scale,
                        theme: self.theme.clone(),
                        generation: self.generation,
                    };
                    if tx.send(req).is_err() {
                        // Worker gone: decode this one synchronously and cache it.
                        self.in_flight.borrow_mut().remove(&key);
                        let result = render_icon(&self.theme, icon, logical_px, scale, px);
                        self.stale.borrow_mut().remove(&key);
                        self.buffers.borrow_mut().insert(key, result.clone());
                        return result;
                    }
                }
                // Not blank while we wait: if this icon was drawn before an
                // invalidation, keep drawing those pixels until the new ones land.
                self.stale.borrow().get(&key).cloned().flatten()
            }
            // No worker (tests): decode inline, caching the result (incl. negatives).
            None => {
                let result = render_icon(&self.theme, icon, logical_px, scale, px);
                self.buffers.borrow_mut().insert(key, result.clone());
                result
            }
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.buffers.borrow().len()
    }

    /// Wire the async path to a plain channel (no worker thread) so a test can inspect
    /// the requests and hand back decoded results via [`apply_decoded`](Self::apply_decoded).
    #[cfg(test)]
    pub(crate) fn wire_test_channel(&mut self) -> mpsc::Receiver<IconRequest> {
        let (tx, rx) = mpsc::channel();
        self.decode_tx = Some(tx);
        rx
    }
}

#[cfg(test)]
impl IconRequest {
    /// The logical (pre-scale) icon size requested — lets a test tell the dash's 64px
    /// warm apart from the grid's 96px.
    pub(crate) fn logical_px(&self) -> f64 {
        self.logical_px
    }

    /// The output scale requested — the other half of the decode key, and what a
    /// scale change re-warms.
    pub(crate) fn scale(&self) -> f64 {
        self.scale
    }
}

/// Resolve + decode an icon in `theme` (a free function so it runs on the decode
/// worker as well as inline): try the icon's own file first, and only on a
/// resolve/decode failure fall back to `application-x-executable` (so a resolvable
/// icon never pays for the fallback's multi-theme sweep).
fn render_icon(
    theme: &str,
    icon: &AppIconRef,
    logical_px: f64,
    scale: f64,
    px: u32,
) -> Option<MemoryBuffer> {
    if let Some(path) = resolve_icon(theme, icon, logical_px, scale) {
        match decode_icon(&path, px, scale) {
            Ok(buf) => return Some(buf),
            Err(err) => tracing::warn!("failed to decode app icon {}: {err:#}", path.display()),
        }
    }
    let fallback = resolve_named_in_theme(theme, "application-x-executable", logical_px, scale)?;
    match decode_icon(&fallback, px, scale) {
        Ok(buf) => Some(buf),
        Err(err) => {
            tracing::warn!(
                "failed to decode fallback icon {}: {err:#}",
                fallback.display()
            );
            None
        }
    }
}

/// The file for an icon descriptor, or `None` (the caller then tries the fallback).
/// A themed icon tries each name in priority order; a file icon resolves directly.
fn resolve_icon(theme: &str, icon: &AppIconRef, logical_px: f64, scale: f64) -> Option<PathBuf> {
    match icon {
        AppIconRef::Themed(names) => names
            .iter()
            .find_map(|name| resolve_named_in_theme(theme, name, logical_px, scale)),
        AppIconRef::File(path) => path.is_file().then(|| path.clone()),
        AppIconRef::Fallback => None,
    }
}

/// Resolve a themed icon name to a file via the freedesktop-icons crate (theme
/// inheritance, size dirs, hicolor fallback). We do **not** use the crate's own cache
/// — it caches negatives process-globally with no invalidation, so a freshly
/// installed app's icon would stay missing until restart; our buffer cache subsumes
/// the win and we control its lifetime.
fn resolve_named_in_theme(theme: &str, name: &str, logical_px: f64, scale: f64) -> Option<PathBuf> {
    // GNOME passes an *integer* scale to its lookup and paints fractionally; `ceil`
    // errs toward a larger source asset (we always resample to exact physical px
    // afterward, so this only affects source quality).
    freedesktop_icons::lookup(name)
        .with_size((logical_px.round() as u16).max(1))
        .with_scale((scale.ceil() as u16).max(1))
        .with_theme(theme)
        .find()
}

// --- images an app pointed us at -----------------------------------------------------------

/// The cache key for a loaded image: source + logical + physical size.
type ImageKey = (ImageSource, u16, u32);

/// A load request handed to the image worker — all fields are `Send`.
pub struct ImageRequest {
    key: ImageKey,
    source: ImageSource,
    scale: f64,
}

/// A finished load delivered back to the main loop, routed to [`ImageCache::apply_loaded`].
/// `None` is a fetch/decode failure, cached as a negative so a broken cover isn't re-fetched
/// every frame — which for a remote source would mean re-hitting the network.
pub struct ImageLoaded {
    key: ImageKey,
    buffer: Option<MemoryBuffer>,
}

/// Loads images an *app* chose ([`ImageSource`]): album art today, local or remote.
///
/// Shares the decode core with [`AppIconCache`] and almost nothing else, which is why it is its
/// own type:
///
/// - **No themed fallback.** A source that will not load stays `None`, so the caller draws its own
///   (the media card's `audio-x-generic-symbolic`). An `application-x-executable` in an album-art
///   slot would silently displace it.
/// - **Its own worker.** A remote fetch can block for the full [`FETCH_TIMEOUT`], and the app-icon
///   worker must never queue behind it — a hung cover server would otherwise hold up the dash and
///   app grid, looking for all the world like a renderer stall.
/// - **An open-ended key space.** One entry per cover *played*, versus the bounded installed-app
///   set, so it must be evicted ([`retain`](Self::retain)).
/// - **No theme generation.** An icon-theme change does not change a cover.
///
/// Without a worker (headless tests) loading is synchronous, exactly like [`AppIconCache`] — note
/// that this means tests never exercise the not-yet-loaded frame.
#[derive(Default)]
pub struct ImageCache {
    buffers: RefCell<HashMap<ImageKey, Option<MemoryBuffer>>>,
    in_flight: RefCell<HashSet<ImageKey>>,
    load_tx: Option<mpsc::Sender<ImageRequest>>,
}

impl ImageCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start the load worker; `result_tx`'s receiver belongs on the main loop calling
    /// [`apply_loaded`](Self::apply_loaded).
    pub fn spawn_worker(&mut self, result_tx: CalloopSender<ImageLoaded>) {
        let (req_tx, req_rx) = mpsc::channel::<ImageRequest>();
        self.load_tx = Some(req_tx);
        if let Err(err) = std::thread::Builder::new()
            .name("image-load".to_owned())
            .spawn(move || {
                for req in req_rx {
                    // App-chosen bytes from an app-chosen server: a malformed image that panics
                    // the decoder must become a negative result, not a dead worker.
                    let buffer = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        load_image(&req.source, req.key.2, req.scale)
                    }))
                    .unwrap_or(None);
                    if result_tx
                        .send(ImageLoaded {
                            key: req.key,
                            buffer,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
        {
            tracing::warn!("could not spawn the image-load thread: {err}; loading synchronously");
            self.load_tx = None;
        }
    }

    /// Insert a finished load. Returns the source it was for, so the caller can invalidate
    /// whatever drew a fallback while it was in flight.
    pub fn apply_loaded(&mut self, loaded: ImageLoaded) -> Option<ImageSource> {
        self.in_flight.get_mut().remove(&loaded.key);
        let source = loaded.key.0.clone();
        self.buffers.get_mut().insert(loaded.key, loaded.buffer);
        Some(source)
    }

    /// The pixels for `source` at `logical_px` on its longest side, aspect-fit and centred on a
    /// transparent square, tagged at the output `scale`. `None` while a load is in flight, or if it
    /// failed.
    pub fn buffer(
        &self,
        source: &ImageSource,
        logical_px: f64,
        scale: f64,
    ) -> Option<MemoryBuffer> {
        let logical = (logical_px.round() as u16).max(1);
        let px = to_physical_precise_round::<i32>(scale, logical_px).max(1) as u32;
        let key = (source.clone(), logical, px);
        if let Some(cached) = self.buffers.borrow().get(&key) {
            return cached.clone();
        }
        match &self.load_tx {
            Some(tx) => {
                if self.in_flight.borrow_mut().insert(key.clone()) {
                    let req = ImageRequest {
                        key: key.clone(),
                        source: source.clone(),
                        scale,
                    };
                    if tx.send(req).is_err() {
                        self.in_flight.borrow_mut().remove(&key);
                        let result = load_image(source, px, scale);
                        self.buffers.borrow_mut().insert(key, result.clone());
                        return result;
                    }
                }
                None
            }
            // No worker (tests): load inline, caching the result (incl. negatives).
            None => {
                let result = load_image(source, px, scale);
                self.buffers.borrow_mut().insert(key, result.clone());
                result
            }
        }
    }

    /// Start loading `source` without wanting the pixels yet — GNOME builds the `MediaMessage` (and
    /// so resolves its icon) when the *player* appears, not when the message list is opened
    /// (`js/ui/messageList.js:1780-1784`), and on a slow link a fetch that only starts when the
    /// popover opens shows the fallback for as long as the round trip takes.
    pub fn warm(&self, source: &ImageSource, logical_px: f64, scale: f64) {
        let _ = self.buffer(source, logical_px, scale);
    }

    /// Drop every entry whose source `keep` rejects. Nothing else evicts these.
    ///
    /// In-flight loads are deliberately left alone: dropping the slot would let the next miss queue
    /// the same load — and, for a remote source, the same network request — while the first is
    /// still running. It lands, is inserted, and the next call evicts it.
    pub fn retain(&mut self, keep: impl Fn(&ImageSource) -> bool) {
        self.buffers
            .get_mut()
            .retain(|(source, _, _), _| keep(source));
    }

    /// Whether `source` is already loaded at this size — a read-only probe, so a test can tell a
    /// warm that *ran* from one it would trigger itself just by asking (without a worker,
    /// [`buffer`](Self::buffer) loads inline, which makes any check through it vacuous).
    pub fn is_loaded(&self, source: &ImageSource, logical_px: f64, scale: f64) -> bool {
        let logical = (logical_px.round() as u16).max(1);
        let px = to_physical_precise_round::<i32>(scale, logical_px).max(1) as u32;
        matches!(
            self.buffers.borrow().get(&(source.clone(), logical, px)),
            Some(Some(_))
        )
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.buffers.borrow().len()
    }
}

/// Load one image: read or fetch the bytes, then decode them at `px`.
fn load_image(source: &ImageSource, px: u32, scale: f64) -> Option<MemoryBuffer> {
    let (data, is_svg) = match source {
        ImageSource::File(path) => {
            let data = match read_capped(path) {
                Ok(data) => data,
                Err(err) => {
                    tracing::warn!("could not read image {}: {err:#}", path.display());
                    return None;
                }
            };
            // Only a hint: Chromium publishes its art as an extensionless temp file
            // (`/tmp/.org.chromium.Chromium.XXXXXX`), so the sniff in `decode_image_bytes` is what
            // actually decides for most real players.
            let is_svg = path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("svg"));
            (data, is_svg)
        }
        ImageSource::Remote(url) => match fetch_remote(source, url) {
            Ok(data) => (data, false),
            Err(err) => {
                tracing::warn!("could not fetch image {url}: {err:#}");
                return None;
            }
        },
    };
    match decode_image_bytes(&data, px, scale, is_svg) {
        Ok(buffer) => Some(buffer),
        Err(err) => {
            tracing::warn!("could not decode image: {err:#}");
            None
        }
    }
}

/// Read a local image, refusing what a media player has no business pointing us at.
///
/// The path comes from an app, so it gets the same two limits the network path has: it must be a
/// **regular file** — `file:///dev/zero` is a URI a player can publish, and `std::fs::read` on it
/// returns when the machine runs out of memory — and it must fit [`MAX_IMAGE_BYTES`]. The length is
/// re-checked while reading rather than trusted from the metadata, since a file can grow between
/// the two.
fn read_capped(path: &Path) -> anyhow::Result<Vec<u8>> {
    use std::io::Read as _;

    let file = std::fs::File::open(path).context("opening")?;
    let meta = file.metadata().context("stat")?;
    if !meta.is_file() {
        anyhow::bail!("not a regular file");
    }
    if meta.len() > MAX_IMAGE_BYTES as u64 {
        anyhow::bail!("larger than the {MAX_IMAGE_BYTES} byte cap");
    }
    let mut data = Vec::with_capacity(meta.len() as usize);
    // `take` one past the cap so hitting it is distinguishable from a file that merely ends there.
    file.take(MAX_IMAGE_BYTES as u64 + 1)
        .read_to_end(&mut data)
        .context("reading")?;
    if data.len() > MAX_IMAGE_BYTES {
        anyhow::bail!("grew past the {MAX_IMAGE_BYTES} byte cap while reading");
    }
    Ok(data)
}

/// Fetch a remote image. **The one place the transport lives** — swapping gvfs for an owned
/// implementation is this function and nothing else.
///
/// gvfs is what GNOME itself uses (`Gio.File.new_for_uri`), so this inherits its proxy and
/// authentication integration. What it does not give us is control over redirects: see
/// [`remote_is_permitted`] for the gap that leaves. Runs on the image worker, so blocking is fine.
fn fetch_remote(source: &ImageSource, url: &str) -> anyhow::Result<Vec<u8>> {
    use gio::prelude::*;

    if !remote_fetch_enabled() {
        anyhow::bail!("remote art is disabled (set NIRI_REMOTE_ART=1 to allow fetching {url})");
    }
    if !remote_is_permitted(source) {
        anyhow::bail!("refusing to fetch {url}: it does not resolve to a public address");
    }

    let cancellable = gio::Cancellable::new();
    // gvfs has no timeout of its own, and a server that accepts and then stalls would otherwise
    // hold this worker forever. The watchdog outlives a fast fetch by design — cancelling an
    // already-finished operation is a no-op — and one sleeping thread per cover is cheap next to
    // the request itself.
    let watchdog = cancellable.clone();
    std::thread::Builder::new()
        .name("image-fetch-timeout".to_owned())
        .spawn(move || {
            std::thread::sleep(FETCH_TIMEOUT);
            watchdog.cancel();
        })
        .context("spawning the fetch watchdog")?;

    let file = gio::File::for_uri(url);
    let stream = file.read(Some(&cancellable)).context("opening")?;
    let mut out = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let read = stream
            .read(&mut chunk[..], Some(&cancellable))
            .context("reading")?;
        if read == 0 {
            break;
        }
        // Cap the response, not just the buffer: `load_contents` would let a hostile server decide
        // how much memory the shell buys.
        if out.len() + read > MAX_IMAGE_BYTES {
            anyhow::bail!("larger than the {MAX_IMAGE_BYTES} byte cap");
        }
        out.extend_from_slice(&chunk[..read]);
    }
    let _ = stream.close(Some(&cancellable));
    Ok(out)
}

/// Decode an icon file to a premultiplied `Abgr8888` [`MemoryBuffer`] of `px`×`px`
/// physical pixels, tagged at `scale` (the buffer-scale-tag trap). SVG via
/// `resvg`, everything else via the `image` crate; the icon keeps its own colors.
fn decode_icon(path: &Path, px: u32, scale: f64) -> anyhow::Result<MemoryBuffer> {
    let data = std::fs::read(path).with_context(|| format!("reading icon {}", path.display()))?;
    let is_svg = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("svg"));
    decode_image_bytes(&data, px, scale, is_svg)
}

/// The decode core, shared by [`decode_icon`] and [`ImageCache`]: bytes in, a premultiplied
/// `Abgr8888` [`MemoryBuffer`] of `px`×`px` out, aspect-fit and centred on transparency.
///
/// `is_svg` is only a *hint* (a filename extension, which fetched bytes do not have); the sniff
/// below is what actually decides when it is wrong.
fn decode_image_bytes(
    data: &[u8],
    px: u32,
    scale: f64,
    is_svg: bool,
) -> anyhow::Result<MemoryBuffer> {
    let rgba = if is_svg {
        render_app_svg(data, px)?
    } else {
        match decode_raster(data, px) {
            Ok(rgba) => rgba,
            // Some icon files are misnamed; if the bytes sniff as SVG, try that.
            Err(err) if data.starts_with(b"<?xml") || data.starts_with(b"<svg") => {
                render_app_svg(data, px).map_err(|_| err)?
            }
            Err(err) => return Err(err),
        }
    };
    Ok(MemoryBuffer::new(
        rgba,
        Fourcc::Abgr8888,
        Size::from((px as i32, px as i32)),
        Scale::from(scale),
        Transform::Normal,
    ))
}

/// Render a full-color SVG (no recolor) into premultiplied `Abgr8888` bytes.
/// `tiny_skia` pixmaps are already premultiplied `[R,G,B,A]`, our exact contract.
fn render_app_svg(data: &[u8], px: u32) -> anyhow::Result<Vec<u8>> {
    let pixmap = render_svg_pixmap(data, px, SvgFit::Contain)?;
    Ok(pixmap.data().to_vec())
}

/// Decode a raster icon, resample (aspect-preserving) to fit `px`, and center it
/// on a transparent square as premultiplied `Abgr8888` bytes.
fn decode_raster(data: &[u8], px: u32) -> anyhow::Result<Vec<u8>> {
    let mut img = image::load_from_memory(data)
        .context("decoding raster icon")?
        .to_rgba8();
    // Premultiply BEFORE resampling: filtering straight alpha across a hard edge
    // bleeds transparent pixels' (usually black) RGB into the edge, giving dark
    // fringes on downscaled icons. Premultiplied resize is edge-correct, and the
    // result is already in our output form.
    for p in img.pixels_mut() {
        let a = u32::from(p[3]);
        p[0] = (u32::from(p[0]) * a / 255) as u8;
        p[1] = (u32::from(p[1]) * a / 255) as u8;
        p[2] = (u32::from(p[2]) * a / 255) as u8;
    }

    let (w, h) = (img.width().max(1), img.height().max(1));
    let s = (px as f32 / w as f32).min(px as f32 / h as f32);
    let nw = ((w as f32 * s).round() as u32).clamp(1, px);
    let nh = ((h as f32 * s).round() as u32).clamp(1, px);
    let resized = image::imageops::resize(&img, nw, nh, image::imageops::FilterType::CatmullRom);

    let mut out = vec![0u8; (px * px * 4) as usize];
    let ox = (px - nw) / 2;
    let oy = (px - nh) / 2;
    for y in 0..nh {
        for x in 0..nw {
            let di = (((oy + y) * px + (ox + x)) * 4) as usize;
            out[di..di + 4].copy_from_slice(&resized.get_pixel(x, y).0); // premultiplied [R,G,B,A]
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A well-known symbolic icon resolves and rasterizes to visible coverage,
    /// recolored to the target (red here). Skips cleanly where the theme is absent.
    #[test]
    fn rasterizes_a_symbolic_icon_recolored() {
        let themes = vec!["Adwaita".to_string(), "hicolor".to_string()];
        // Try a couple of names very likely to exist in Adwaita.
        let path = ["night-light-symbolic", "weather-clear-night-symbolic"]
            .into_iter()
            .find_map(|n| resolve_symbolic(n, &themes));
        let Some(path) = path else {
            eprintln!("skipping rasterizes_a_symbolic_icon_recolored: no symbolic icons installed");
            return;
        };

        let buf = rasterize_symbolic(&path, 32, [1., 0., 0., 1.], 1.).expect("rasterize");
        assert_eq!(buf.size().w, 32);
        let data = buf.data();
        // Some opaque coverage exists, and where covered it is red (R set, G/B ~0).
        let covered = data.chunks_exact(4).filter(|p| p[3] > 40).count();
        assert!(covered > 20, "expected icon coverage, got {covered}");
        let reddish = data
            .chunks_exact(4)
            .filter(|p| p[3] > 40 && p[0] > 40 && p[1] < 20 && p[2] < 20)
            .count();
        assert!(
            reddish > 20,
            "expected the recolor to be red (R in slot 0), got {reddish}"
        );
    }

    /// The scale-tag guard: physical pixels grow with scale, but the tag keeps the
    /// logical size constant (else the icon composites `scale`× too big on HiDPI).
    #[test]
    fn icon_buffer_is_tagged_at_output_scale() {
        let themes = vec!["Adwaita".to_string(), "hicolor".to_string()];
        let Some(path) = ["night-light-symbolic", "weather-clear-night-symbolic"]
            .into_iter()
            .find_map(|n| resolve_symbolic(n, &themes))
        else {
            eprintln!(
                "skipping icon_buffer_is_tagged_at_output_scale: no symbolic icons installed"
            );
            return;
        };
        for scale in [1.0, 2.0] {
            let px = to_physical_precise_round::<i32>(scale, 16.) as u32;
            let buf = rasterize_symbolic(&path, px, [1., 1., 1., 1.], scale).expect("rasterize");
            let logical = buf.logical_size();
            assert!(
                (logical.w - 16.).abs() < 1.0,
                "logical width {} drifts from 16 at scale {scale}",
                logical.w
            );
        }
    }

    #[test]
    fn cache_resolves_and_reuses() {
        let cache = IconCache::new("Adwaita");
        // Unknown icon resolves to None and is remembered.
        assert!(cache
            .resolve("definitely-not-an-icon-xyz-symbolic")
            .is_none());
        assert!(cache
            .rasterize("definitely-not-an-icon-xyz-symbolic", 16, 1., [1.; 4])
            .is_none());
    }

    // ---- Full-color app-icon loader ----

    /// A tiny two-color SVG: red left half, blue right half.
    const RED_BLUE_SVG: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" width="32" height="32" viewBox="0 0 32 32"><rect x="0" y="0" width="16" height="32" fill="#ff0000"/><rect x="16" y="0" width="16" height="32" fill="#0000ff"/></svg>"##;

    /// The app SVG path keeps the icon's own colors — the exact inverse of the
    /// symbolic path's "everything becomes the tint color".
    #[test]
    fn app_icon_svg_keeps_its_colors() {
        let rgba = render_app_svg(RED_BLUE_SVG, 32).expect("render app svg");
        let reddish = rgba
            .chunks_exact(4)
            .filter(|p| p[0] > 180 && p[2] < 60 && p[3] > 200)
            .count();
        let bluish = rgba
            .chunks_exact(4)
            .filter(|p| p[2] > 180 && p[0] < 60 && p[3] > 200)
            .count();
        assert!(reddish > 40, "expected red pixels, got {reddish}");
        assert!(bluish > 40, "expected blue pixels, got {bluish}");
    }

    /// The raster path premultiplies alpha into the output.
    #[test]
    fn app_icon_png_premultiplies() {
        use std::io::Cursor;
        // 2×2, uniform red at 50% alpha (straight).
        let mut img = image::RgbaImage::new(2, 2);
        for p in img.pixels_mut() {
            *p = image::Rgba([255, 0, 0, 128]);
        }
        let mut png = Vec::new();
        img.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .expect("encode png");

        let rgba = decode_raster(&png, 8).expect("decode raster");
        // Premultiplied red = 255 * 128/255 = 128; alpha kept at 128.
        let p = rgba
            .chunks_exact(4)
            .find(|p| p[3] > 0)
            .expect("some coverage");
        assert!(
            (100..=150).contains(&u32::from(p[0])),
            "R not premultiplied: {}",
            p[0]
        );
        assert_eq!(p[1], 0);
        assert!((110..=140).contains(&u32::from(p[3])), "alpha {}", p[3]);
    }

    /// The full pipeline through a `File` icon: decodes, tags the buffer at the
    /// output scale (incl. a fractional scale — the buffer-scale-tag trap), and
    /// the cache is dropped by `set_theme`/`clear` but kept on a same-theme set.
    #[test]
    fn app_icon_file_pipeline_scale_tag_and_cache() {
        let path =
            std::env::temp_dir().join(format!("gsrs-app-icon-{}-rb.svg", std::process::id()));
        std::fs::write(&path, RED_BLUE_SVG).expect("write temp svg");
        let icon = AppIconRef::File(path.clone());

        let mut cache = AppIconCache::new("Adwaita");
        for scale in [1.0, 2.0, 2.25] {
            let buf = cache.buffer(&icon, 32., scale).expect("decode file icon");
            let logical = buf.logical_size();
            assert!(
                (logical.w - 32.).abs() < 1.0,
                "logical width {} drifts at scale {scale}",
                logical.w
            );
            let px = to_physical_precise_round::<i32>(scale, 32.);
            assert_eq!(buf.size().w, px, "physical px at scale {scale}");
        }
        assert!(cache.len() >= 1, "buffers cached");

        let before = cache.len();
        cache.set_theme("Adwaita");
        assert_eq!(cache.len(), before, "same theme keeps the cache");
        cache.set_theme("hicolor");
        assert_eq!(cache.len(), 0, "theme change clears the cache");
        let _ = cache.buffer(&icon, 32., 1.0);
        cache.clear();
        assert_eq!(cache.len(), 0, "clear drops the cache");

        let _ = std::fs::remove_file(&path);
    }

    /// The image cache has no themed fallback, and that is the whole reason it is separate: a
    /// source that will not load stays `None` so the caller can draw its own
    /// (`audio-x-generic-symbolic` for a media card). An `application-x-executable` stretched
    /// across an album-art slot would silently displace it.
    #[test]
    fn an_unloadable_image_stays_none_rather_than_becoming_an_icon() {
        let dir = std::env::temp_dir().join(format!("gsrs-image-cache-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let junk = dir.join("not-an-image.png");
        std::fs::write(&junk, b"certainly not a PNG").expect("write junk");

        let cache = ImageCache::new();
        assert!(
            cache
                .buffer(&ImageSource::File(junk.clone()), 48., 1.0)
                .is_none(),
            "a file that will not decode must not fall back to any icon"
        );
        // Cached as a negative, so a broken cover is not re-read (or, remote, re-fetched) forever.
        assert_eq!(cache.len(), 1);

        let missing = dir.join("gone.png");
        assert!(cache
            .buffer(&ImageSource::File(missing), 48., 1.0)
            .is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The two shapes real browsers actually hand us, captured from a live session: Firefox writes
    /// a `.png` under `~/.config/mozilla/firefox/firefox-mpris/`, Chromium an **extensionless**
    /// temp file (`/tmp/.org.chromium.Chromium.XXXXXX`). The extensionless one is why the format
    /// must be decided by sniffing the bytes and not by the filename.
    ///
    /// Both are 16:9 video thumbnails rather than square covers, so both letterbox in the 48px
    /// slot — which is exactly the case where the themed plate must not be painted behind them.
    #[test]
    fn browser_art_decodes_regardless_of_the_filename() {
        let dir = std::env::temp_dir().join(format!("gsrs-browser-art-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");

        // A 16:9 PNG, written once with an extension and once without.
        let png = {
            let mut bytes = std::io::Cursor::new(Vec::new());
            image::RgbaImage::from_pixel(336, 188, image::Rgba([20, 90, 160, 255]))
                .write_to(&mut bytes, image::ImageFormat::Png)
                .expect("encode");
            bytes.into_inner()
        };
        let named = dir.join("657400_7.png");
        let anonymous = dir.join(".org.chromium.Chromium.rIWTt1");
        std::fs::write(&named, &png).expect("write");
        std::fs::write(&anonymous, &png).expect("write");

        let cache = ImageCache::new();
        for path in [named, anonymous] {
            let buffer = cache
                .buffer(&ImageSource::File(path.clone()), 48., 1.0)
                .unwrap_or_else(|| panic!("{} must decode", path.display()));
            // Square, because the fit-and-centre happens in the decode: a 16:9 source becomes a
            // 48x48 buffer with transparent bands, which is what reproduces St's RESIZE_ASPECT.
            assert_eq!(buffer.size().w, 48);
            assert_eq!(buffer.size().h, 48);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A local path is chosen by an app exactly as freely as a URL is, so it gets the same limits.
    /// `/dev/zero` is the case that motivates the file-type check: it is a perfectly valid
    /// `file://` URI for a player to publish, and an uncapped `std::fs::read` of it returns when
    /// the machine runs out of memory.
    #[test]
    fn a_local_image_must_be_a_regular_file_within_the_cap() {
        let cache = ImageCache::new();

        let zero = ImageSource::File(PathBuf::from("/dev/zero"));
        let started = std::time::Instant::now();
        assert!(cache.buffer(&zero, 48., 1.0).is_none());
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "refusing a character device must be immediate, not an out-of-memory race"
        );

        // Over the cap: a sparse file, so this costs no disk.
        let dir = std::env::temp_dir().join(format!("gsrs-image-cap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let huge = dir.join("huge.png");
        let f = std::fs::File::create(&huge).expect("create");
        f.set_len(crate::image_source::MAX_IMAGE_BYTES as u64 + 1)
            .expect("grow");
        drop(f);
        assert!(cache.buffer(&ImageSource::File(huge), 48., 1.0).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Remote art is off unless `NIRI_REMOTE_ART=1`, so by default a remote source never reaches
    /// the transport at all — no DNS, no connection. The negative is still cached, so a player
    /// publishing http(s) art cannot make the shell retry it every frame.
    #[test]
    fn remote_art_is_refused_while_the_switch_is_off() {
        if crate::image_source::remote_fetch_enabled() {
            eprintln!("skipping: NIRI_REMOTE_ART=1 is set");
            return;
        }
        let cache = ImageCache::new();
        // A public address, so only the switch can be what refuses this.
        let source = ImageSource::Remote("https://example.com/cover.png".to_owned());
        assert!(cache.buffer(&source, 48., 1.0).is_none());
        assert_eq!(cache.len(), 1, "the refusal is cached, not retried");
    }

    /// The address guard runs *before* the transport, so a refused URL costs no connection at all.
    /// Deterministic and network-free: a loopback address is refused by the rule, not by failing to
    /// connect (port 1 would also fail, which is exactly the confound this avoids — the assertion
    /// below would pass either way, so the guard is pinned by its own unit tests in
    /// `image_source`, and what this pins is that `ImageCache` actually routes through it).
    #[test]
    fn a_refused_remote_source_is_cached_as_a_negative() {
        let cache = ImageCache::new();
        let blocked = ImageSource::Remote("http://127.0.0.1:1/probe".to_owned());
        assert!(cache.buffer(&blocked, 48., 1.0).is_none());
        // Negative-cached, so a hostile or broken cover cannot make the shell retry every frame —
        // for a remote source that would be a request per frame.
        assert_eq!(cache.len(), 1);
        assert!(cache.buffer(&blocked, 48., 1.0).is_none());
        assert_eq!(cache.len(), 1);
    }

    /// The real remote path: fetch over gvfs, decode, and come out as a buffer at the asked-for
    /// size. `#[ignore]` because it needs the network — run it by hand after touching
    /// [`fetch_remote`]:
    ///
    /// ```text
    /// cargo test --workspace remote_image -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "needs the network, and NIRI_REMOTE_ART=1"]
    fn remote_image_fetches_and_decodes() {
        if !crate::image_source::remote_fetch_enabled() {
            eprintln!("skipping: set NIRI_REMOTE_ART=1 to exercise the fetch");
            return;
        }
        let cache = ImageCache::new();
        let source = ImageSource::Remote("https://picsum.photos/200.jpg".to_owned());
        let buffer = cache
            .buffer(&source, 48., 2.0)
            .expect("fetch + decode a remote cover");
        assert_eq!(
            buffer.size().w,
            96,
            "decoded at the requested physical size"
        );
        assert!((buffer.logical_size().w - 48.).abs() < 1.0);
    }

    /// `retain` is the image cache's only bound: its key space is one entry per cover *played*,
    /// unlike the app-icon cache's bounded installed-app set.
    #[test]
    fn retain_evicts_the_covers_that_left_the_screen() {
        let dir = std::env::temp_dir().join(format!("gsrs-image-retain-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let keep = dir.join("now-playing.svg");
        let drop = dir.join("previous-track.svg");
        std::fs::write(&keep, RED_BLUE_SVG).expect("write svg");
        std::fs::write(&drop, RED_BLUE_SVG).expect("write svg");
        let keep_src = ImageSource::File(keep.clone());
        let drop_src = ImageSource::File(drop.clone());

        let mut cache = ImageCache::new();
        assert!(cache.buffer(&keep_src, 48., 1.0).is_some());
        assert!(cache.buffer(&drop_src, 48., 1.0).is_some());
        assert_eq!(cache.len(), 2);

        cache.retain(|source| source == &keep_src);
        assert_eq!(cache.len(), 1, "only the cover that left may be evicted");
        assert!(cache.buffer(&keep_src, 48., 1.0).is_some());
        assert_eq!(cache.len(), 1, "the survivor must not have been re-loaded");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A real installed app's icon resolves + decodes to visible coverage across
    /// scales. Skips cleanly on a bare host / with no icon theme.
    #[test]
    fn installed_app_icon_resolves_and_decodes() {
        let apps = crate::app_system::gio_installed_for_test();
        if apps.is_empty() {
            eprintln!("skipping installed_app_icon_resolves_and_decodes: no apps installed");
            return;
        }
        let cache = AppIconCache::new("Adwaita");
        let icon = apps.iter().find_map(|a| {
            if matches!(a.icon, AppIconRef::Fallback) {
                return None;
            }
            cache.buffer(&a.icon, 48., 1.0).map(|_| a.icon.clone())
        });
        let Some(icon) = icon else {
            eprintln!("skipping installed_app_icon_resolves_and_decodes: no resolvable app icon");
            return;
        };
        for scale in [1.0, 2.0] {
            let buf = cache.buffer(&icon, 48., scale).expect("decode");
            let covered = buf.data().chunks_exact(4).filter(|p| p[3] > 40).count();
            assert!(covered > 50, "coverage at scale {scale}: {covered}");
            let logical = buf.logical_size();
            assert!(
                (logical.w - 48.).abs() < 1.0,
                "logical {} at scale {scale}",
                logical.w
            );
        }
    }

    /// The fallback descriptor resolves `application-x-executable` where installed,
    /// and a missing `File` path never panics.
    #[test]
    fn fallback_ref_resolves_executable_or_none() {
        let cache = AppIconCache::new("Adwaita");
        match cache.buffer(&AppIconRef::Fallback, 32., 1.0) {
            Some(buf) => {
                let covered = buf.data().chunks_exact(4).filter(|p| p[3] > 40).count();
                assert!(covered > 20, "fallback icon should have coverage");
            }
            None => eprintln!("skipping fallback coverage: application-x-executable not installed"),
        }
        // Missing file → fallback chain (or None), never a panic.
        let _ = cache.buffer(
            &AppIconRef::File(PathBuf::from("/nonexistent/x.png")),
            32.,
            1.0,
        );
    }

    fn dummy_buffer() -> MemoryBuffer {
        MemoryBuffer::new(
            vec![255u8; 4],
            Fourcc::Abgr8888,
            Size::from((1, 1)),
            Scale::from(1.0),
            Transform::Normal,
        )
    }

    /// With a worker wired, a miss enqueues one request and returns `None`; a repeat
    /// doesn't re-enqueue; applying the decoded buffer makes it warm.
    #[test]
    fn async_miss_requests_once_then_applies() {
        let mut cache = AppIconCache::new("hicolor");
        let rx = cache.wire_test_channel();
        let icon = AppIconRef::Themed(vec!["some-app".into()]);

        assert!(
            cache.buffer(&icon, 96., 1.0).is_none(),
            "miss draws nothing"
        );
        let req = rx.try_recv().expect("a decode was requested");
        assert!(
            cache.buffer(&icon, 96., 1.0).is_none(),
            "still pending, no icon yet"
        );
        assert!(rx.try_recv().is_err(), "not re-queued while in flight");

        assert!(cache
            .apply_decoded(IconDecoded {
                key: req.key,
                buffer: Some(dummy_buffer()),
                generation: req.generation,
            })
            .is_some());
        assert!(cache.buffer(&icon, 96., 1.0).is_some(), "now warm");
    }

    /// An invalidation keeps drawing the icon it already has until the replacement
    /// lands. Blanking instead is what made the dash flicker on an `installed-changed`
    /// ping: the decode is off-thread, so "no pixels yet" is a visible frame.
    #[test]
    fn an_invalidated_icon_keeps_its_old_pixels_until_the_new_ones_land() {
        let mut cache = AppIconCache::new("hicolor");
        let rx = cache.wire_test_channel();
        let icon = AppIconRef::Themed(vec!["some-app".into()]);

        cache.buffer(&icon, 96., 1.0);
        let first = rx.try_recv().expect("a decode was requested");
        cache
            .apply_decoded(IconDecoded {
                key: first.key,
                buffer: Some(dummy_buffer()),
                generation: first.generation,
            })
            .expect("the first decode is current");
        assert!(
            cache.buffer(&icon, 96., 1.0).is_some(),
            "warm to begin with"
        );

        cache.clear();
        assert!(
            cache.buffer(&icon, 96., 1.0).is_some(),
            "an invalidated icon must keep drawing its old pixels, not go blank, \
             while the replacement decodes off-thread"
        );
        let second = rx.try_recv().expect("the replacement was requested");

        // ...and once the replacement lands it is what gets served, so the old pixels
        // are a stopgap rather than a permanent shadow.
        let key = cache
            .apply_decoded(IconDecoded {
                key: second.key.clone(),
                buffer: None,
                generation: second.generation,
            })
            .expect("the replacement is current");
        assert_eq!(
            key, second.key,
            "the applied key identifies what to re-upload"
        );
        assert!(
            cache.buffer(&icon, 96., 1.0).is_none(),
            "a replacement that resolves to nothing must win over the stale pixels"
        );
    }

    /// A result that lands after a `clear` (installed-changed) is dropped, and the
    /// icon re-requests rather than being stuck in flight.
    #[test]
    fn stale_result_after_clear_is_dropped_and_re_requested() {
        let mut cache = AppIconCache::new("hicolor");
        let rx = cache.wire_test_channel();
        let icon = AppIconRef::Themed(vec!["some-app".into()]);

        cache.buffer(&icon, 96., 1.0);
        let stale = rx.try_recv().unwrap();
        cache.clear(); // bumps the generation + clears in-flight

        assert!(
            cache
                .apply_decoded(IconDecoded {
                    key: stale.key,
                    buffer: Some(dummy_buffer()),
                    generation: stale.generation,
                })
                .is_none(),
            "a pre-clear result is stale"
        );
        assert!(cache.buffer(&icon, 96., 1.0).is_none());
        assert!(rx.try_recv().is_ok(), "re-requested after the stale drop");
    }

    /// A negative result (resolve/decode failure) is cached, so a broken icon isn't
    /// re-requested every frame.
    #[test]
    fn async_negative_result_is_cached() {
        let mut cache = AppIconCache::new("hicolor");
        let rx = cache.wire_test_channel();
        let icon = AppIconRef::Themed(vec!["nope".into()]);

        cache.buffer(&icon, 96., 1.0);
        let req = rx.try_recv().unwrap();
        cache.apply_decoded(IconDecoded {
            key: req.key,
            buffer: None,
            generation: req.generation,
        });

        assert!(cache.buffer(&icon, 96., 1.0).is_none());
        assert!(
            rx.try_recv().is_err(),
            "a cached negative is not re-requested"
        );
    }
}

#[cfg(test)]
mod worker_tests {
    use super::*;

    fn cache_with_worker() -> (IconCache, mpsc::Receiver<SymbolicRequest>) {
        let mut cache = IconCache::new("Adwaita");
        let (tx, rx) = mpsc::channel();
        cache.set_worker(tx);
        (cache, rx)
    }

    /// A miss is queued **once**, not once a frame. Without this the render path would re-send the
    /// same icon on every frame until the worker answered — which for an icon no theme provides
    /// means a few hundred `stat`s per frame, forever, since a negative never becomes a hit.
    #[test]
    fn a_miss_is_queued_once() {
        let (cache, rx) = cache_with_worker();
        let key = icon_key("night-light-symbolic", 16., 2., [1., 1., 1., 1.]);

        for _ in 0..5 {
            cache.request("night-light-symbolic", &key, 2., [1., 1., 1., 1.]);
        }

        assert_eq!(
            rx.try_iter().count(),
            1,
            "the same miss was queued repeatedly"
        );
    }

    /// The theme-change trap. An icon-theme change *replaces* the cache rather than clearing it,
    /// so a rasterization the previous cache started can still arrive afterwards — carrying a key
    /// that matches (name/size/tint say nothing about the theme) and pixels from the old theme.
    /// Filing it would leave the new theme showing an old icon until something else invalidated it.
    #[test]
    fn a_result_from_a_previous_cache_is_dropped() {
        let (mut cache, _rx) = cache_with_worker();
        let key = icon_key("night-light-symbolic", 16., 2., [1., 1., 1., 1.]);

        let stale = SymbolicRasterized {
            key: key.clone(),
            buffer: None,
            generation: cache.generation.wrapping_sub(1),
        };
        assert!(
            !cache.apply_rasterized(stale),
            "a result from another cache was accepted"
        );
        assert!(
            !cache.buffers.borrow().contains_key(&key),
            "a result from another cache reached the buffer map"
        );

        let ours = SymbolicRasterized {
            key: key.clone(),
            buffer: None,
            generation: cache.generation,
        };
        assert!(cache.apply_rasterized(ours), "our own result was dropped");
        assert!(cache.buffers.borrow().contains_key(&key));
    }

    /// A negative (no theme provides the icon) is remembered, so the resolve probe runs once
    /// rather than every frame. `apply_rasterized` clears `in_flight`, and the stored `None` is
    /// what stops the next frame re-queueing.
    #[test]
    fn a_negative_result_is_remembered_and_not_requeued() {
        let (mut cache, rx) = cache_with_worker();
        let key = icon_key("definitely-not-an-icon-xyz", 16., 2., [1., 1., 1., 1.]);

        cache.request("definitely-not-an-icon-xyz", &key, 2., [1., 1., 1., 1.]);
        assert_eq!(rx.try_iter().count(), 1);

        let miss = SymbolicRasterized {
            key: key.clone(),
            buffer: None,
            generation: cache.generation,
        };
        cache.apply_rasterized(miss);
        assert!(
            cache.in_flight.borrow().is_empty(),
            "the key stayed in flight, so it can never be retried"
        );
        assert!(
            matches!(cache.buffers.borrow().get(&key), Some(None)),
            "the negative was not cached, so the resolve probe runs again every frame"
        );
    }

    /// Without a worker — every headless test, and any session where the thread failed to spawn —
    /// the inline path must still produce pixels. This is what keeps the whole existing render
    /// corpus meaningful after the hoist.
    #[test]
    fn without_a_worker_rasterization_still_happens_inline() {
        let cache = IconCache::new("Adwaita");
        let Some(name) = ["night-light-symbolic", "weather-clear-night-symbolic"]
            .into_iter()
            .find(|n| cache.resolve(n).is_some())
        else {
            eprintln!("skipping: no Adwaita symbolic icons installed");
            return;
        };

        assert!(
            cache.rasterize(name, 32, 1., [1., 1., 1., 1.]).is_some(),
            "the no-worker fallback stopped rasterizing"
        );
    }
}
