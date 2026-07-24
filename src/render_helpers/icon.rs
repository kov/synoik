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
use std::sync::mpsc;

use anyhow::Context as _;
use calloop::channel::Sender as CalloopSender;
use smithay::backend::allocator::Fourcc;
use smithay::utils::{Scale, Size, Transform};

use crate::app_system::AppIconRef;
use crate::render_helpers::memory::MemoryBuffer;
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

/// Resolves + rasterizes symbolic icons on demand, caching both the resolved
/// paths and the rendered (recolored) buffers.
pub struct IconCache {
    /// Themes to search, in order (e.g. the configured theme, then Adwaita/hicolor).
    themes: Vec<String>,
    resolved: RefCell<HashMap<String, Option<PathBuf>>>,
    buffers: RefCell<HashMap<(String, u32, u32), MemoryBuffer>>,
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
            resolved: RefCell::new(HashMap::new()),
            buffers: RefCell::new(HashMap::new()),
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

    /// A recolored icon buffer for `name` at `logical_px` (square), rendered at the
    /// output `scale` and tinted to straight-RGBA `color`. `None` if unresolved.
    /// Cached by (name, physical size, color).
    pub fn buffer(
        &self,
        name: &str,
        logical_px: f64,
        scale: f64,
        color: [f32; 4],
    ) -> Option<MemoryBuffer> {
        let px: u32 = to_physical_precise_round::<i32>(scale, logical_px).max(1) as u32;
        let key = (name.to_string(), px, color_key(color));
        if let Some(buf) = self.buffers.borrow().get(&key) {
            return Some(buf.clone());
        }
        let result = if let Some(bytes) = embedded_icon(name) {
            rasterize_symbolic_bytes(bytes, px, color, scale)
        } else {
            let path = self.resolve(name)?;
            rasterize_symbolic(&path, px, color, scale)
        };
        match result {
            Ok(buf) => {
                self.buffers.borrow_mut().insert(key, buf.clone());
                Some(buf)
            }
            Err(err) => {
                tracing::warn!("failed to rasterize icon {name:?}: {err:#}");
                None
            }
        }
    }
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
    /// Bumped on every cache-invalidating change (theme swap / `clear`); stamped on
    /// each request so a result that lands after an invalidation is dropped.
    generation: u64,
    /// Requests currently on the worker, so a miss isn't re-queued every frame.
    /// Cleared on invalidation (else a key stuck here would never re-resolve).
    in_flight: RefCell<HashSet<IconKey>>,
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
            generation: 0,
            in_flight: RefCell::new(HashSet::new()),
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
    pub fn apply_decoded(&mut self, decoded: IconDecoded) -> bool {
        self.in_flight.get_mut().remove(&decoded.key);
        if decoded.generation != self.generation {
            return false;
        }
        self.buffers.get_mut().insert(decoded.key, decoded.buffer);
        true
    }

    /// Bump the generation and drop everything in-flight — every cache-invalidating
    /// change routes through here, so a decode started before it lands stale.
    fn invalidate(&mut self) {
        self.generation += 1;
        self.buffers.get_mut().clear();
        self.in_flight.get_mut().clear();
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
                        self.buffers.borrow_mut().insert(key, result.clone());
                        return result;
                    }
                }
                None
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
    fn wire_test_channel(&mut self) -> mpsc::Receiver<IconRequest> {
        let (tx, rx) = mpsc::channel();
        self.decode_tx = Some(tx);
        rx
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

/// Decode an icon file to a premultiplied `Abgr8888` [`MemoryBuffer`] of `px`×`px`
/// physical pixels, tagged at `scale` (the buffer-scale-tag trap). SVG via
/// `resvg`, everything else via the `image` crate; the icon keeps its own colors.
fn decode_icon(path: &Path, px: u32, scale: f64) -> anyhow::Result<MemoryBuffer> {
    let data = std::fs::read(path).with_context(|| format!("reading icon {}", path.display()))?;
    let is_svg = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("svg"));
    let rgba = if is_svg {
        render_app_svg(&data, px)?
    } else {
        match decode_raster(&data, px) {
            Ok(rgba) => rgba,
            // Some icon files are misnamed; if the bytes sniff as SVG, try that.
            Err(err) if data.starts_with(b"<?xml") || data.starts_with(b"<svg") => {
                render_app_svg(&data, px).map_err(|_| err)?
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
            .buffer("definitely-not-an-icon-xyz-symbolic", 16., 1., [1.; 4])
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

        assert!(cache.apply_decoded(IconDecoded {
            key: req.key,
            buffer: Some(dummy_buffer()),
            generation: req.generation,
        }));
        assert!(cache.buffer(&icon, 96., 1.0).is_some(), "now warm");
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
            !cache.apply_decoded(IconDecoded {
                key: stale.key,
                buffer: Some(dummy_buffer()),
                generation: stale.generation,
            }),
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
