// SPDX-License-Identifier: GPL-3.0-only
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

use std::cell::RefCell;
use std::collections::HashMap;
use std::env;
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::rc::Rc;

use anyhow::{anyhow, ensure, Context};
use resvg::{tiny_skia, usvg};
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::input::pointer::{CursorIcon, CursorImageStatus, CursorImageSurfaceData};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Buffer, IsAlive, Logical, Point, Size, Transform};
use smithay::wayland::compositor::with_states;
use xcursor::parser::{parse_xcursor, Image};
use xcursor::CursorTheme;

/// Some default looking `left_ptr` icon.
static FALLBACK_CURSOR_DATA: &[u8] = include_bytes!("../resources/cursor.rgba");

/// GNOME 51's scalable Adwaita (`resources/cursor-theme`, embedded by `build.rs`), as
/// `(path under the theme root, bytes)`. gnome-shell compiles the same files into a GResource
/// (`data/gnome-shell-cursor-theme.gresource.xml`), so they are there whatever the system ships.
static BUNDLED_THEME: &[(&str, &[u8])] = include!(concat!(env!("OUT_DIR"), "/cursor_theme.rs"));

/// The theme the bundled cursors are, and the one `cursor-theme` names by default.
const BUNDLED_THEME_NAME: &str = "Adwaita";

/// The magnifications a zoomed cursor is rasterized at: a zoom draws the smallest tier at or
/// above it, scaled down, so an animated zoom uploads a handful of images, not one per frame.
const ZOOM_TIERS: [f64; 4] = [1., 1.5, 2., 3.];

/// More uploads than this and the cache starts over: a theme's animated cursors run to ~60
/// frames, and zoom tiers multiply what a cursor can have cached.
const MAX_TEXTURES: usize = 512;

/// How many times larger an SVG frame is drawn before it is averaged down to its size
/// ([`SvgFrame::rasterize`]).
const SUPERSAMPLE: usize = 2;

/// Which raster an upload is: an SVG at a scale (its bits), or an Xcursor at a requested size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum RasterKey {
    Svg(u64),
    Xcursor(i32),
}

type SvgCache = HashMap<(CursorIcon, bool), Option<Rc<SvgCursor>>>;
type XCursorCache = HashMap<(CursorIcon, i32), Option<Rc<XCursor>>>;

pub struct CursorManager {
    theme: CursorTheme,
    theme_name: String,
    size: i32,
    /// Where themes' `icons/` directories are looked for: the XDG data directories, user first.
    data_dirs: Vec<PathBuf>,
    current_cursor: CursorImageStatus,
    /// A compositor-wide cursor that wins over whatever the pointer is actually
    /// over — mutter's `MetaCompositor:global_cursor`, applied through the backend's
    /// `override-cursor` hook (`compositor.c:1104-1132`). Its only user is
    /// launch feedback: any startup sequence still pending shows `wait`.
    global_override: Option<CursorIcon>,
    /// Scalable cursors by name, and whether they came from the bundled theme. `None` is a
    /// cached miss.
    svg_cache: RefCell<SvgCache>,
    /// Xcursor images by name and requested pixel size.
    xcursor_cache: RefCell<XCursorCache>,
    /// Uploads by name, raster and frame.
    textures: RefCell<HashMap<(CursorIcon, RasterKey, usize), MemoryRenderBuffer>>,
}

impl CursorManager {
    pub fn new(theme: &str, size: i32) -> Self {
        Self::ensure_env(theme, size);

        Self {
            theme: CursorTheme::load(theme),
            theme_name: theme.to_owned(),
            size,
            data_dirs: crate::gnome::xdg_data_dirs(),
            current_cursor: CursorImageStatus::default_named(),
            global_override: None,
            svg_cache: Default::default(),
            xcursor_cache: Default::default(),
            textures: Default::default(),
        }
    }

    /// Follow `org.gnome.desktop.interface cursor-theme`/`cursor-size`. Returns whether anything
    /// changed — the caller then owes a redraw.
    pub fn set_theme(&mut self, theme: &str, size: i32) -> bool {
        if self.theme_name == theme && self.size == size {
            return false;
        }
        Self::ensure_env(theme, size);
        self.theme = CursorTheme::load(theme);
        self.theme_name = theme.to_owned();
        self.size = size;
        self.svg_cache.get_mut().clear();
        self.xcursor_cache.get_mut().clear();
        self.textures.get_mut().clear();
        true
    }

    /// Look for scalable themes in `dirs` instead of the XDG data directories. Tests hand it
    /// none, so the bundled theme answers whatever the host has installed.
    pub fn set_data_dirs(&mut self, dirs: Vec<PathBuf>) {
        self.data_dirs = dirs;
        self.svg_cache.get_mut().clear();
        self.textures.get_mut().clear();
    }

    /// Checks if the cursor WlSurface is alive, and if not, cleans it up.
    pub fn check_cursor_image_surface_alive(&mut self) {
        if let CursorImageStatus::Surface(surface) = &self.current_cursor {
            if !surface.alive() {
                self.current_cursor = CursorImageStatus::default_named();
            }
        }
    }

    /// Set the compositor-wide cursor override (`None` clears it). Returns whether
    /// it changed — the caller owes a redraw if so.
    pub fn set_global_override(&mut self, icon: Option<CursorIcon>) -> bool {
        if self.global_override == icon {
            return false;
        }
        self.global_override = icon;
        true
    }

    /// The compositor-wide override currently in force, if any.
    pub fn global_override(&self) -> Option<CursorIcon> {
        self.global_override
    }

    /// The icon actually drawn, override included. `Hidden` still wins: a client
    /// that hid the pointer (a video player, a game) should not have one put back
    /// under it by a launch happening elsewhere.
    fn effective_named(&self) -> Option<CursorIcon> {
        match (&self.current_cursor, self.global_override) {
            (CursorImageStatus::Hidden, _) => None,
            (_, Some(icon)) => Some(icon),
            (CursorImageStatus::Named(icon), None) => Some(*icon),
            (CursorImageStatus::Surface(_), None) => None,
        }
    }

    /// The cursor to draw on an output of `scale`, magnified by `zoom`, at `millis` into its
    /// animation.
    pub fn get_render_cursor(&self, scale: f64, zoom: f64, millis: u32) -> RenderCursor {
        if let Some(icon) = self.global_override {
            if !matches!(self.current_cursor, CursorImageStatus::Hidden) {
                return RenderCursor::Named(self.named(icon, scale, zoom, millis));
            }
        }
        match self.current_cursor.clone() {
            CursorImageStatus::Hidden => RenderCursor::Hidden,
            CursorImageStatus::Surface(surface) => {
                let hotspot = with_states(&surface, |states| {
                    states
                        .data_map
                        .get::<CursorImageSurfaceData>()
                        .unwrap()
                        .lock()
                        .unwrap()
                        .hotspot
                });

                RenderCursor::Surface { hotspot, surface }
            }
            CursorImageStatus::Named(icon) => {
                RenderCursor::Named(self.named(icon, scale, zoom, millis))
            }
        }
    }

    pub fn is_current_cursor_animated(&self, scale: f64) -> bool {
        // Through `effective_named`, so an animated `wait` keeps the redraw loop
        // running while it is overriding a still cursor.
        let Some(icon) = self.effective_named() else {
            return false;
        };
        if let Some(svg) = self.svg(icon, false) {
            return svg.frames.len() > 1;
        }
        let px = self.xcursor_px(scale, 1.);
        if let Some(cursor) = self.xcursor(icon, px) {
            return cursor.is_animated_cursor();
        }
        self.svg(icon, true).is_some_and(|svg| svg.frames.len() > 1)
    }

    /// Currently used cursor_image as a cursor provider.
    pub fn cursor_image(&self) -> &CursorImageStatus {
        &self.current_cursor
    }

    /// Set new cursor image provider.
    pub fn set_cursor_image(&mut self, cursor: CursorImageStatus) {
        self.current_cursor = cursor;
    }

    /// `icon` as drawn, in mutter's order (`meta_cursor_theme_get_cursor`): the theme's scalable
    /// cursor, else its Xcursor. Past what mutter does, a theme with neither borrows the bundled
    /// Adwaita, and a name nothing has is drawn as the default pointer.
    pub fn named(&self, icon: CursorIcon, scale: f64, zoom: f64, millis: u32) -> NamedCursor {
        let zoom = zoom.max(1.);
        if let Some(svg) = self.svg(icon, false) {
            return self.draw_svg(icon, &svg, scale, zoom, millis);
        }
        let px = self.xcursor_px(scale, zoom);
        if let Some(cursor) = self.xcursor(icon, px) {
            return self.draw_xcursor(icon, &cursor, px, zoom, millis);
        }
        if let Some(svg) = self.svg(icon, true) {
            return self.draw_svg(icon, &svg, scale, zoom, millis);
        }
        if icon != CursorIcon::Default {
            return self.named(CursorIcon::Default, scale, zoom, millis);
        }
        let fallback = Self::fallback_cursor();
        self.draw_xcursor(icon, &fallback, 0, zoom, millis)
    }

    /// The pixel size an Xcursor is asked for: the integer scale's, as the buffer was always
    /// loaded, times the zoom tier.
    fn xcursor_px(&self, scale: f64, zoom: f64) -> i32 {
        (f64::from(self.size) * scale.ceil() * zoom_tier(zoom)).round() as i32
    }

    fn draw_svg(
        &self,
        icon: CursorIcon,
        svg: &SvgCursor,
        scale: f64,
        zoom: f64,
        millis: u32,
    ) -> NamedCursor {
        let idx = frame_at(svg.frames.iter().map(|f| f.delay), millis);
        let frame = &svg.frames[idx];
        // Logical pixels per SVG unit: the theme draws at `nominal_size`, the user asked for
        // `cursor-size` (`st-cursor.c`, `create_surface_for_frame`).
        let unit = f64::from(self.size) / frame.nominal_size;
        let raster_scale = unit * scale * zoom_tier(zoom);
        let pixels = Size::<i32, Buffer>::from((
            (frame.width * raster_scale).ceil().max(1.) as i32,
            (frame.height * raster_scale).ceil().max(1.) as i32,
        ));
        let buffer = self.texture((icon, RasterKey::Svg(raster_scale.to_bits()), idx), || {
            // Argb8888, as Xcursor images are: the nested backend puts only that on the
            // outer's cursor plane, and draws anything else into the frame.
            let mut pixmap = frame.rasterize(raster_scale, pixels);
            for pixel in pixmap.data_mut().as_chunks_mut::<4>().0 {
                pixel.swap(0, 2);
            }
            MemoryRenderBuffer::from_slice(
                pixmap.data(),
                Fourcc::Argb8888,
                (pixels.w, pixels.h),
                1,
                Transform::Normal,
                None,
            )
        });
        // The drawing moved by its snap; the pointer moves with it.
        let snap = frame.snap(raster_scale);
        let hotspot = (
            frame.hotspot.0 + snap.x / raster_scale,
            frame.hotspot.1 + snap.y / raster_scale,
        );
        NamedCursor {
            buffer,
            pixels,
            size: Size::from((frame.width * unit * zoom, frame.height * unit * zoom)),
            hotspot: Point::from((hotspot.0 * unit * zoom, hotspot.1 * unit * zoom)),
        }
    }

    fn draw_xcursor(
        &self,
        icon: CursorIcon,
        cursor: &XCursor,
        px: i32,
        zoom: f64,
        millis: u32,
    ) -> NamedCursor {
        let (idx, image) = cursor.frame(millis);
        let pixels = Size::<i32, Buffer>::from((image.width as i32, image.height as i32));
        let buffer = self.texture((icon, RasterKey::Xcursor(px), idx), || {
            MemoryRenderBuffer::from_slice(
                &image.pixels_rgba,
                Fourcc::Argb8888,
                (pixels.w, pixels.h),
                1,
                Transform::Normal,
                None,
            )
        });
        // A theme without the size asked for gave its nearest: the image is drawn at
        // `cursor-size` all the same, scaled.
        let unit = f64::from(self.size) / f64::from(image.size.max(1));
        NamedCursor {
            buffer,
            pixels,
            size: Size::from((
                f64::from(image.width) * unit * zoom,
                f64::from(image.height) * unit * zoom,
            )),
            hotspot: Point::from((
                f64::from(image.xhot) * unit * zoom,
                f64::from(image.yhot) * unit * zoom,
            )),
        }
    }

    fn texture(
        &self,
        key: (CursorIcon, RasterKey, usize),
        make: impl FnOnce() -> MemoryRenderBuffer,
    ) -> MemoryRenderBuffer {
        let mut textures = self.textures.borrow_mut();
        if textures.len() >= MAX_TEXTURES && !textures.contains_key(&key) {
            textures.clear();
        }
        textures.entry(key).or_insert_with(make).clone()
    }

    /// The scalable `icon` of the current theme, or of the bundled one.
    fn svg(&self, icon: CursorIcon, bundled: bool) -> Option<Rc<SvgCursor>> {
        self.svg_cache
            .borrow_mut()
            .entry((icon, bundled))
            .or_insert_with(|| {
                let found = std::iter::once(icon.name())
                    .chain(icon.alt_names().iter().copied())
                    .find_map(|name| {
                        if bundled {
                            SvgCursor::load_bundled(name)
                        } else {
                            SvgCursor::load_from_theme(&self.data_dirs, &self.theme_name, name)
                        }
                    });
                match found {
                    Some(Ok(cursor)) => Some(Rc::new(cursor)),
                    Some(Err(err)) => {
                        warn!("error loading scalable cursor {}: {err:?}", icon.name());
                        None
                    }
                    None => None,
                }
            })
            .clone()
    }

    /// The Xcursor `icon` of the current theme at the size nearest `px`.
    fn xcursor(&self, icon: CursorIcon, px: i32) -> Option<Rc<XCursor>> {
        self.xcursor_cache
            .borrow_mut()
            .entry((icon, px))
            .or_insert_with(|| {
                let mut cursor = Self::load_xcursor(&self.theme, icon.name(), px);

                // Check alternative names to account for non-compliant themes.
                if cursor.is_err() {
                    for name in icon.alt_names() {
                        cursor = Self::load_xcursor(&self.theme, name, px);
                        if cursor.is_ok() {
                            break;
                        }
                    }
                }

                if let Err(err) = &cursor {
                    debug!("no xcursor {}@{px}: {err:?}", icon.name());
                }

                cursor.ok().map(Rc::new)
            })
            .clone()
    }

    /// Load the cursor with the given `name` from the file system picking the closest
    /// one to the given `size`.
    fn load_xcursor(theme: &CursorTheme, name: &str, size: i32) -> anyhow::Result<XCursor> {
        let _span = tracy_client::span!("load_xcursor");

        let path = theme
            .load_icon(name)
            .ok_or_else(|| anyhow!("no default icon"))?;

        let mut file = File::open(path).context("error opening cursor icon file")?;
        let mut buf = vec![];
        file.read_to_end(&mut buf)
            .context("error reading cursor icon file")?;

        let mut images = parse_xcursor(&buf).context("error parsing cursor icon file")?;

        let (width, height) = images
            .iter()
            .min_by_key(|image| (size - image.size as i32).abs())
            .map(|image| (image.width, image.height))
            .ok_or_else(|| anyhow!("no images"))?;

        images.retain(move |image| image.width == width && image.height == height);

        let animation_duration = images.iter().fold(0, |acc, image| acc + image.delay);

        Ok(XCursor {
            images,
            animation_duration,
        })
    }

    /// Set the common XCURSOR env variables.
    fn ensure_env(theme: &str, size: i32) {
        env::set_var("XCURSOR_THEME", theme);
        env::set_var("XCURSOR_SIZE", size.to_string());
    }

    fn fallback_cursor() -> XCursor {
        let images = vec![Image {
            size: 32,
            width: 64,
            height: 64,
            xhot: 1,
            yhot: 1,
            delay: 0,
            pixels_rgba: Vec::from(FALLBACK_CURSOR_DATA),
            pixels_argb: vec![],
        }];

        XCursor {
            images,
            animation_duration: 0,
        }
    }
}

/// The smallest of [`ZOOM_TIERS`] at or above `zoom`.
fn zoom_tier(zoom: f64) -> f64 {
    ZOOM_TIERS
        .into_iter()
        .find(|tier| *tier >= zoom)
        .unwrap_or(ZOOM_TIERS[ZOOM_TIERS.len() - 1])
}

/// Which of frames lasting `delays` ms shows `millis` into a looping animation. A cursor whose
/// delays are all zero is still.
fn frame_at(delays: impl Iterator<Item = u32> + Clone, millis: u32) -> usize {
    let total: u32 = delays.clone().sum();
    if total == 0 {
        return 0;
    }
    let mut millis = millis % total;
    for (i, delay) in delays.enumerate() {
        if millis < delay {
            return i;
        }
        millis -= delay;
    }
    0
}

/// The cursor prepared for renderer.
pub enum RenderCursor {
    Hidden,
    Surface {
        hotspot: Point<i32, Logical>,
        surface: WlSurface,
    },
    Named(NamedCursor),
}

/// A named cursor's current image and where it goes.
pub struct NamedCursor {
    /// The image, at buffer scale 1.
    pub buffer: MemoryRenderBuffer,
    /// The image's size in pixels.
    pub pixels: Size<i32, Buffer>,
    /// The size it is drawn at.
    pub size: Size<f64, Logical>,
    /// Where the pointer is on it, from its top-left corner.
    pub hotspot: Point<f64, Logical>,
}

/// A cursor from a theme's `cursors_scalable`: a `metadata.json` listing SVG frames — KDE's
/// format, which gnome-shell 51 reads (`st-cursor.c`).
struct SvgCursor {
    frames: Vec<SvgFrame>,
}

/// Straight edges along one axis, as `(where, length)` in SVG units.
type Edges = Vec<(f64, f64)>;

struct SvgFrame {
    tree: usvg::Tree,
    /// How long it shows, in ms; zero for a still cursor.
    delay: u32,
    /// The pointer's position, in SVG units.
    hotspot: (f64, f64),
    /// The cursor size the SVG is drawn for, in SVG units.
    nominal_size: f64,
    /// The SVG's intrinsic size.
    width: f64,
    height: f64,
    /// The outline's vertical edges ([`SvgFrame::snap`]).
    x_edges: Edges,
    /// Its horizontal edges.
    y_edges: Edges,
}

impl SvgFrame {
    /// The frame at `scale` pixels per SVG unit, on a `pixels`-sized premultiplied RGBA pixmap.
    ///
    /// Drawn at [`SUPERSAMPLE`] times the size and averaged down: tiny-skia draws a stroke no
    /// wider than a pixel as a hairline, a pixel wide along its minor axis, so Adwaita's 1-unit
    /// outline came out thin and grey on the arrow's diagonal at cursor-size 24. Averaged down,
    /// the raster matches librsvg's, which gnome-shell draws with.
    ///
    /// The drawing is shifted by [`SvgFrame::snap`] first.
    fn rasterize(&self, scale: f64, pixels: Size<i32, Buffer>) -> tiny_skia::Pixmap {
        let _span = tracy_client::span!("SvgFrame::rasterize");
        let (w, h) = (pixels.w as usize, pixels.h as usize);
        let mut large =
            tiny_skia::Pixmap::new((w * SUPERSAMPLE) as u32, (h * SUPERSAMPLE) as u32).unwrap();
        let large_scale = scale as f32 * SUPERSAMPLE as f32;
        let snap = self.snap(scale);
        let transform = tiny_skia::Transform::from_scale(large_scale, large_scale).post_translate(
            snap.x as f32 * SUPERSAMPLE as f32,
            snap.y as f32 * SUPERSAMPLE as f32,
        );
        resvg::render(&self.tree, transform, &mut large.as_mut());

        // Premultiplied, so a plain average of each block is the right blend.
        let mut pixmap = tiny_skia::Pixmap::new(w as u32, h as u32).unwrap();
        let src = large.data();
        let stride = w * SUPERSAMPLE * 4;
        for (i, out) in pixmap
            .data_mut()
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .enumerate()
        {
            let (x, y) = (i % w * SUPERSAMPLE, i / w * SUPERSAMPLE);
            for (c, out) in out.iter_mut().enumerate() {
                let mut sum = 0u32;
                for dy in 0..SUPERSAMPLE {
                    for dx in 0..SUPERSAMPLE {
                        sum += u32::from(src[(y + dy) * stride + (x + dx) * 4 + c]);
                    }
                }
                let n = (SUPERSAMPLE * SUPERSAMPLE) as u32;
                *out = ((sum + n / 2) / n) as u8;
            }
        }
        pixmap
    }

    /// How far, in pixels and under half of one each way, the drawing moves at `scale` so its
    /// straight outline edges fall between pixels rather than across them.
    ///
    /// Adwaita's shapes sit on the unit grid but the groups holding them do not (`default`'s is
    /// translated by 1.748 units), so at cursor-size 24 a horizontal edge straddles two rows and
    /// both come out grey. A cursor is a few pixels of outline, so that is most of what it is:
    /// this is our own hinting, not GNOME's, which draws the SVG where it lies. Each axis takes
    /// the shift that leaves the least edge length off the grid, the smallest one of those.
    fn snap(&self, scale: f64) -> Point<f64, Buffer> {
        fn axis(edges: &[(f64, f64)], scale: f64) -> f64 {
            let misfit = |shift: f64| -> f64 {
                edges
                    .iter()
                    .map(|(at, length)| {
                        let at = at * scale + shift;
                        (at - at.round()).abs() * length
                    })
                    .sum()
            };
            std::iter::once(0.)
                .chain(
                    edges
                        .iter()
                        .map(|(at, _)| (at * scale).round() - at * scale),
                )
                .min_by(|a, b| {
                    // Within a millipixel of edge length the fits are equal.
                    let key = |shift: f64| ((misfit(shift) * 1000.).round(), shift.abs());
                    key(*a).partial_cmp(&key(*b)).unwrap()
                })
                .unwrap()
        }
        Point::from((axis(&self.x_edges, scale), axis(&self.y_edges, scale)))
    }
}

/// The vertical and horizontal edges of `tree`'s outline, as [`SvgFrame`] holds them: a
/// stroke's two sides, or a fill's edge when it has no stroke. Shadows, drawn through filters,
/// are blurred anyway and stay out of it.
fn straight_edges(tree: &usvg::Tree) -> (Edges, Edges) {
    use usvg::tiny_skia_path::PathSegment;

    fn walk(group: &usvg::Group, x: &mut Edges, y: &mut Edges) {
        for node in group.children() {
            match node {
                usvg::Node::Group(group) if group.filters().is_empty() => walk(group, x, y),
                usvg::Node::Path(path) if path.is_visible() => add(path, x, y),
                _ => (),
            }
        }
    }

    fn add(path: &usvg::Path, x: &mut Edges, y: &mut Edges) {
        let transform = path.abs_transform();
        let sides: &[f32] = &match path.stroke() {
            Some(stroke) => {
                let scale = (transform.sx * transform.sy - transform.kx * transform.ky)
                    .abs()
                    .sqrt();
                let half = stroke.width().get() * scale / 2.;
                [-half, half]
            }
            None if path.fill().is_some() => [0., 0.],
            None => return,
        };
        let mut line = |mut from: tiny_skia::Point, mut to: tiny_skia::Point| {
            transform.map_point(&mut from);
            transform.map_point(&mut to);
            let (dx, dy) = ((to.x - from.x).abs(), (to.y - from.y).abs());
            if dx < 1e-3 && dy > 1e-3 {
                x.extend(
                    sides
                        .iter()
                        .map(|side| (f64::from(from.x + side), f64::from(dy))),
                );
            } else if dy < 1e-3 && dx > 1e-3 {
                y.extend(
                    sides
                        .iter()
                        .map(|side| (f64::from(from.y + side), f64::from(dx))),
                );
            }
        };
        let (mut start, mut at) = (tiny_skia::Point::zero(), tiny_skia::Point::zero());
        for segment in path.data().segments() {
            match segment {
                PathSegment::MoveTo(to) => (start, at) = (to, to),
                PathSegment::LineTo(to) => {
                    line(at, to);
                    at = to;
                }
                PathSegment::QuadTo(_, to) | PathSegment::CubicTo(_, _, to) => at = to,
                PathSegment::Close => {
                    line(at, start);
                    at = start;
                }
            }
        }
    }

    let (mut x, mut y) = (Vec::new(), Vec::new());
    walk(tree.root(), &mut x, &mut y);
    (x, y)
}

#[derive(serde::Deserialize)]
struct FrameMetadata {
    filename: String,
    #[serde(default)]
    delay: u32,
    #[serde(default)]
    hotspot_x: f64,
    #[serde(default)]
    hotspot_y: f64,
    nominal_size: f64,
}

impl SvgCursor {
    /// `name` from `icons/<theme>/cursors_scalable` in the user's data directory, then each
    /// system one; the bundled copy too when the theme is Adwaita (`find_cursor_metadata`).
    /// `None` when no directory has it.
    fn load_from_theme(
        data_dirs: &[PathBuf],
        theme: &str,
        name: &str,
    ) -> Option<anyhow::Result<Self>> {
        let found = data_dirs.iter().find_map(|dir| {
            let dir = dir
                .join("icons")
                .join(theme)
                .join("cursors_scalable")
                .join(name);
            dir.join("metadata.json").is_file().then_some(dir)
        });
        if let Some(dir) = found {
            return Some(Self::load(|file: &str| -> anyhow::Result<Vec<u8>> {
                let path: PathBuf = dir.join(file);
                std::fs::read(&path).with_context(|| format!("reading {}", path.display()))
            }));
        }
        (theme == BUNDLED_THEME_NAME)
            .then(|| Self::load_bundled(name))
            .flatten()
    }

    /// `name` from the bundled Adwaita.
    fn load_bundled(name: &str) -> Option<anyhow::Result<Self>> {
        let prefix = format!("{BUNDLED_THEME_NAME}/cursors_scalable/{name}/");
        let read = |file: &str| -> anyhow::Result<Vec<u8>> {
            let path = format!("{prefix}{file}");
            BUNDLED_THEME
                .iter()
                .find(|(p, _)| *p == path)
                .map(|(_, bytes)| bytes.to_vec())
                .ok_or_else(|| anyhow!("{path} is not bundled"))
        };
        read("metadata.json").ok()?;
        Some(Self::load(read))
    }

    fn load(read: impl Fn(&str) -> anyhow::Result<Vec<u8>>) -> anyhow::Result<Self> {
        let _span = tracy_client::span!("SvgCursor::load");

        let metadata: Vec<FrameMetadata> =
            serde_json::from_slice(&read("metadata.json")?).context("parsing metadata.json")?;
        ensure!(!metadata.is_empty(), "metadata.json lists no frames");

        let options = usvg::Options::default();
        let frames = metadata
            .into_iter()
            .map(|meta| {
                ensure!(meta.nominal_size > 0., "{}: no nominal_size", meta.filename);
                let tree = usvg::Tree::from_data(&read(&meta.filename)?, &options)
                    .with_context(|| format!("parsing {}", meta.filename))?;
                let size = tree.size();
                let (x_edges, y_edges) = straight_edges(&tree);
                Ok(SvgFrame {
                    delay: meta.delay,
                    hotspot: (meta.hotspot_x, meta.hotspot_y),
                    nominal_size: meta.nominal_size,
                    width: f64::from(size.width()),
                    height: f64::from(size.height()),
                    x_edges,
                    y_edges,
                    tree,
                })
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(Self { frames })
    }
}

// The XCursorBuffer implementation is inspired by `wayland-rs`, thus provided under MIT license.

/// The state of the `NamedCursor`.
pub struct XCursor {
    /// The image for the underlying named cursor.
    images: Vec<Image>,
    /// The total duration of the animation.
    animation_duration: u32,
}

impl XCursor {
    /// Given a time, calculate which frame to show.
    ///
    /// Time will wrap, so if for instance the cursor has an animation lasting 100ms,
    /// then calling this function with 5ms and 105ms as input gives the same output.
    pub fn frame(&self, millis: u32) -> (usize, &Image) {
        if self.animation_duration == 0 {
            return (0, &self.images[0]);
        }
        let idx = frame_at(self.images.iter().map(|image| image.delay), millis);
        (idx, &self.images[idx])
    }

    /// Check whether the cursor is animated.
    pub fn is_animated_cursor(&self) -> bool {
        self.images.len() > 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A manager that sees only the bundled theme, whatever the host installed.
    fn manager(theme: &str, size: i32) -> CursorManager {
        let mut manager = CursorManager::new(theme, size);
        manager.set_data_dirs(Vec::new());
        manager
    }

    /// Every name a client can ask for (`wp_cursor_shape_v1` maps onto these, all 36) has a
    /// scalable image in the bundled theme, so the default theme never falls back to a bitmap.
    #[test]
    fn the_bundled_theme_covers_every_cursor_name() {
        use CursorIcon::*;
        let manager = manager(BUNDLED_THEME_NAME, 24);
        for icon in [
            Default,
            ContextMenu,
            Help,
            Pointer,
            Progress,
            Wait,
            Cell,
            Crosshair,
            Text,
            VerticalText,
            Alias,
            Copy,
            Move,
            NoDrop,
            NotAllowed,
            Grab,
            Grabbing,
            EResize,
            NResize,
            NeResize,
            NwResize,
            SResize,
            SeResize,
            SwResize,
            WResize,
            EwResize,
            NsResize,
            NeswResize,
            NwseResize,
            ColResize,
            RowResize,
            AllScroll,
            ZoomIn,
            ZoomOut,
            DndAsk,
            AllResize,
        ] {
            assert!(manager.svg(icon, true).is_some(), "{icon:?} is not bundled");
        }
    }

    /// The default pointer at GNOME's defaults: Adwaita's 32-unit SVG, drawn for a 24 px
    /// cursor, with its hotspot at (8, 7) (`cursors_scalable/default/metadata.json`), moved
    /// with the drawing by the snap that puts the notch's edges, at 17.748 and 18.748, on rows.
    #[test]
    fn the_default_pointer_is_the_bundled_svg() {
        let manager = manager(BUNDLED_THEME_NAME, 24);
        let named = manager.named(CursorIcon::Default, 1., 1., 0);
        assert_eq!(named.hotspot.x, 8.);
        assert!(
            (named.hotspot.y - 7.2516).abs() < 1e-3,
            "{:?}",
            named.hotspot
        );
        assert_eq!(named.size, Size::from((32., 32.)));
        assert_eq!(named.pixels, Size::from((32, 32)));
    }

    /// `cursor-size` and the output scale both scale the SVG, the scale fractionally: no
    /// rounding to a bitmap size.
    #[test]
    fn an_svg_cursor_renders_at_any_size_and_scale() {
        let manager = manager(BUNDLED_THEME_NAME, 48);
        let named = manager.named(CursorIcon::Default, 1.5, 1., 0);
        // (8, 7) at 2 logical pixels a unit, but drawn at 3 pixels a unit, where the notch's
        // edges fall at 53.245 and 56.245 and the snap is -0.245 px.
        assert_eq!(named.hotspot.x, 16.);
        assert!(
            (named.hotspot.y - (7. - 0.2452 / 3.) * 2.).abs() < 1e-3,
            "{:?}",
            named.hotspot
        );
        assert_eq!(named.size, Size::from((64., 64.)));
        assert_eq!(named.pixels, Size::from((96, 96)));
    }

    /// A zoom scales the drawn size and the hotspot exactly, and rasterizes at the tier above:
    /// sharp, and one upload for every zoom up to it.
    #[test]
    fn a_zoomed_cursor_rasterizes_at_the_tier_above() {
        let manager = manager(BUNDLED_THEME_NAME, 24);
        let named = manager.named(CursorIcon::Default, 1., 1.2, 0);
        assert_eq!(named.hotspot, Point::from((8. * 1.2, 7. * 1.2)));
        assert_eq!(named.size, Size::from((32. * 1.2, 32. * 1.2)));
        assert_eq!(named.pixels, Size::from((48, 48)));

        let named = manager.named(CursorIcon::Default, 1., 3., 0);
        assert_eq!(named.pixels, Size::from((96, 96)));
    }

    /// The rasterized arrow is opaque at its tip and leaves the shadow margin's far corner
    /// clear — the pixmap holds the drawing, not a blank or a filled square.
    #[test]
    fn the_rasterized_pointer_has_its_tip_and_clear_corners() {
        let cursor = SvgCursor::load_bundled("default").unwrap().unwrap();
        let frame = &cursor.frames[0];
        let pixmap = frame.rasterize(1., Size::from((32, 32)));
        // Just inside the tip, below and right of the hotspot.
        let tip = pixmap.pixel(10, 12).unwrap();
        assert!(tip.alpha() > 200, "{tip:?}");
        let corner = pixmap.pixel(31, 0).unwrap();
        assert_eq!(corner.alpha(), 0, "{corner:?}");
    }

    /// At cursor-size 24 and scale 1 the arrow's white outline holds up along its diagonal,
    /// as librsvg draws it, rather than thinning to grey: the outermost visible pixel of each
    /// row down the diagonal is near white.
    #[test]
    fn the_arrow_keeps_its_white_outline_on_the_diagonal() {
        let cursor = SvgCursor::load_bundled("default").unwrap().unwrap();
        let pixmap = cursor.frames[0].rasterize(1., Size::from((32, 32)));
        for y in 8..17 {
            let edge = (0..32)
                .rev()
                .map(|x| pixmap.pixel(x, y).unwrap().demultiply())
                .find(|p| p.alpha() > 64)
                .unwrap();
            let lightness = edge.red().min(edge.green()).min(edge.blue());
            assert!(lightness > 220, "row {y}: {edge:?}");
        }
    }

    /// At cursor-size 24 and scale 1 the bar across the arrow's notch is one white row, not two
    /// grey ones: the snap put its edges between rows.
    #[test]
    fn the_arrow_notch_lands_on_a_pixel_row() {
        let cursor = SvgCursor::load_bundled("default").unwrap().unwrap();
        let pixmap = cursor.frames[0].rasterize(1., Size::from((32, 32)));
        let pixel = |x, y| pixmap.pixel(x, y).unwrap().demultiply();
        // The bar runs from the tail to the diagonal: white and opaque on row 18, with the
        // black head above it and only shadow below.
        for x in 14..19 {
            let bar = pixel(x, 18);
            assert!(bar.alpha() > 240 && bar.red() > 240, "({x}, 18): {bar:?}");
        }
        for x in 12..16 {
            let head = pixel(x, 17);
            assert!(head.alpha() > 240 && head.red() < 30, "({x}, 17): {head:?}");
        }
        for x in 15..19 {
            let below = pixel(x, 19);
            assert!(below.red() < 30, "({x}, 19): {below:?}");
        }
    }

    /// `wait` is animated, frame by frame on its delays, and loops.
    #[test]
    fn an_animated_svg_cursor_steps_through_its_frames() {
        let cursor = SvgCursor::load_bundled("wait").unwrap().unwrap();
        assert!(cursor.frames.len() > 1);
        let delays = || cursor.frames.iter().map(|f| f.delay);
        let first = cursor.frames[0].delay;
        let total: u32 = delays().sum();
        assert_eq!(frame_at(delays(), 0), 0);
        assert_eq!(frame_at(delays(), first), 1);
        assert_eq!(frame_at(delays(), total), 0);
    }

    /// A theme with neither scalable nor Xcursor images borrows the bundled Adwaita rather than
    /// the last-resort bitmap.
    #[test]
    fn a_missing_theme_borrows_the_bundled_cursors() {
        let manager = manager("synoik-test-no-such-cursor-theme", 24);
        let named = manager.named(CursorIcon::Pointer, 1., 1., 0);
        // Either the system's `default` Xcursor theme answered (an Xcursor image at 24) or the
        // bundled SVG did; never the 64 px fallback bitmap.
        assert_ne!(named.pixels, Size::from((64, 64)));
    }
}
