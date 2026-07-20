//! Symbolic-icon rasterization.
//!
//! GNOME's panel, quick settings, app grid, and overview all draw *symbolic*
//! icons: monochrome SVGs from the icon theme (`foo-symbolic`), recolored to the
//! current foreground color. We have no icon system otherwise, so this is the
//! shared gate for all of them.
//!
//! An [`IconCache`] resolves a symbolic name to a file in the icon theme, renders
//! it once at a target physical size with `resvg` (pure Rust), and recolors it by
//! using the render's **alpha as a coverage mask** times the target color — the
//! same tint model as our glyphs. The result is a premultiplied [`MemoryBuffer`]
//! the caller composites like any other CPU bitmap.
//!
//! Deferred: the full freedesktop theme-inheritance/`index.theme` machinery. Panel
//! symbolic icons all live under `<theme>/{symbolic,scalable}/<category>/` in the
//! current theme (with an Adwaita/hicolor fallback), which a direct search covers.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use smithay::backend::allocator::Fourcc;
use smithay::utils::{Scale, Size, Transform};

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
/// (`data/icons/scalable/status/`, GPLv2+) into `resources/icons/`.
fn embedded_icon(name: &str) -> Option<&'static [u8]> {
    match name {
        "no-notifications-symbolic" => Some(include_bytes!(
            "../../resources/icons/no-notifications-symbolic.svg"
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
    use resvg::{tiny_skia, usvg};

    let px = px.max(1);
    let opt = usvg::Options::default();
    let tree = usvg::Tree::from_data(data, &opt).context("parsing icon SVG")?;

    let mut pixmap = tiny_skia::Pixmap::new(px, px).context("allocating icon pixmap")?;
    let size = tree.size();
    let sx = px as f32 / size.width();
    let sy = px as f32 / size.height();
    let transform = tiny_skia::Transform::from_scale(sx, sy);
    resvg::render(&tree, transform, &mut pixmap.as_mut());

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
}
