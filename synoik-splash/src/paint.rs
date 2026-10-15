// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The splash on the CPU, into an XRGB8888 buffer: what the initramfs splash scans out
//! (`docs/fork/boot-splash.md` §7.3). The layout is the outer's; this is only its pixels, at
//! scale 1, without text.

use std::fs::File;
use std::io;
use std::path::Path;

use crate::layout::{self, Images, Item, Point, Rect, Size};
use crate::plymouth::Splash;

/// An image, premultiplied, one `0xAARRGGBB` per pixel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub size: Size,
    pub pixels: Vec<u32>,
}

impl Image {
    /// Decode the PNG at `path`.
    pub fn load(path: &Path) -> io::Result<Self> {
        let mut decoder = png::Decoder::new(io::BufReader::new(File::open(path)?));
        // Whatever the file holds, 8-bit RGB(A) comes out.
        decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
        let mut reader = decoder.read_info().map_err(io::Error::other)?;
        let mut buf = vec![0; reader.output_buffer_size().unwrap_or(0)];
        let info = reader.next_frame(&mut buf).map_err(io::Error::other)?;
        let channels = match info.color_type {
            png::ColorType::Rgba => 4,
            png::ColorType::Rgb => 3,
            png::ColorType::GrayscaleAlpha => 2,
            png::ColorType::Grayscale => 1,
            png::ColorType::Indexed => return Err(io::Error::other("unexpanded palette")),
        };
        let pixels = buf[..info.buffer_size()]
            .chunks_exact(channels)
            .map(|p| {
                let (r, g, b, a) = match *p {
                    [r, g, b, a] => (r, g, b, a),
                    [r, g, b] => (r, g, b, 255),
                    [l, a] => (l, l, l, a),
                    [l] => (l, l, l, 255),
                    _ => unreachable!(),
                };
                // As the outer's `load_image`: rounded, so the two drawers agree.
                let mul = |c: u8| ((u16::from(c) * u16::from(a) + 127) / 255) as u32;
                (u32::from(a) << 24) | (mul(r) << 16) | (mul(g) << 8) | mul(b)
            })
            .collect();
        Ok(Self {
            size: Size::from((info.width as i32, info.height as i32)),
            pixels,
        })
    }
}

/// The theme's images, each one that failed to load left out, as the outer does.
#[derive(Debug, Clone, Default)]
pub struct Theme {
    pub watermark: Option<Image>,
    /// All the frames or none: a spinner with a gap would stutter.
    pub spinner: Vec<Image>,
}

impl Theme {
    pub fn load(dir: &Path) -> Self {
        let watermark = Image::load(&dir.join("watermark.png"))
            .map_err(|err| warn!("splash: watermark: {err}"))
            .ok();
        let spinner = (1..)
            .map(|i| dir.join(format!("throbber-{i:04}.png")))
            .take_while(|path| path.exists())
            .map(|path| Image::load(&path))
            .collect::<io::Result<Vec<_>>>()
            .unwrap_or_else(|err| {
                warn!("splash: spinner: {err}");
                Vec::new()
            });
        Self { watermark, spinner }
    }

    pub fn images(&self) -> Images {
        Images {
            watermark: self.watermark.as_ref().map(|i| i.size),
            spinner: self.spinner.first().map(|i| (i.size, self.spinner.len())),
        }
    }
}

/// A mapped XRGB8888 buffer: `stride` bytes per row.
pub struct Canvas<'a> {
    pub size: Size,
    pub stride: usize,
    pub bytes: &'a mut [u8],
}

impl Canvas<'_> {
    fn put(&mut self, x: i32, y: i32, pixel: u32) {
        let at = y as usize * self.stride + x as usize * 4;
        self.bytes[at..at + 4].copy_from_slice(&pixel.to_le_bytes());
    }

    fn get(&self, x: i32, y: i32) -> u32 {
        let at = y as usize * self.stride + x as usize * 4;
        u32::from_le_bytes(self.bytes[at..at + 4].try_into().unwrap())
    }

    /// `rect` clipped to the canvas, as (x0, y0, x1, y1).
    fn clip(&self, rect: Rect) -> Option<(i32, i32, i32, i32)> {
        let x0 = rect.loc.x.max(0);
        let y0 = rect.loc.y.max(0);
        let x1 = (rect.loc.x + rect.size.w).min(self.size.w);
        let y1 = (rect.loc.y + rect.size.h).min(self.size.h);
        (x0 < x1 && y0 < y1).then_some((x0, y0, x1, y1))
    }

    pub fn fill(&mut self, rect: Rect, rgb: [f32; 3]) {
        let Some((x0, y0, x1, y1)) = self.clip(rect) else {
            return;
        };
        let [r, g, b] = rgb.map(|c| (c.clamp(0., 1.) * 255.).round() as u32);
        let pixel = 0xff00_0000 | (r << 16) | (g << 8) | b;
        for y in y0..y1 {
            for x in x0..x1 {
                self.put(x, y, pixel);
            }
        }
    }

    /// `image` over what is there, its top-left `at`.
    pub fn blend(&mut self, image: &Image, at: Point) {
        let rect = Rect::new(at, image.size);
        let Some((x0, y0, x1, y1)) = self.clip(rect) else {
            return;
        };
        for y in y0..y1 {
            for x in x0..x1 {
                let src = image.pixels[((y - at.y) * image.size.w + (x - at.x)) as usize];
                let a = src >> 24;
                let pixel = match a {
                    0 => continue,
                    255 => src,
                    _ => {
                        let dst = self.get(x, y);
                        let over = |shift: u32| {
                            let s = (src >> shift) & 0xff;
                            let d = (dst >> shift) & 0xff;
                            (s + (d * (255 - a) + 127) / 255).min(255) << shift
                        };
                        0xff00_0000 | over(16) | over(8) | over(0)
                    }
                };
                self.put(x, y, pixel);
            }
        }
    }
}

/// Lay out `splash` on the whole canvas, its spinner at `spinner_frame`, without text.
pub fn items(splash: &Splash, size: Size, theme: &Theme, spinner_frame: usize) -> Vec<Item> {
    let mut layout = layout::layout(splash, size, &theme.images(), spinner_frame, &mut |_, _| {
        Size::default()
    });
    layout.retain(|item| !matches!(item, Item::Text { .. }));
    layout
}

/// The area the spinner occupies, whichever frame it is on: what a turn redraws.
pub fn spinner_rect(items: &[Item], theme: &Theme) -> Option<Rect> {
    let size = theme.spinner.first()?.size;
    items.iter().find_map(|item| match item {
        Item::Spinner { at, .. } => Some(Rect::new(*at, size)),
        _ => None,
    })
}

/// Paint `items` over black.
pub fn paint(canvas: &mut Canvas<'_>, items: &[Item], theme: &Theme) {
    let full = Rect::new(Point::default(), canvas.size);
    canvas.fill(full, [0., 0., 0.]);
    for item in items {
        paint_item(canvas, item, theme);
    }
}

/// Repaint only the spinner's area: black, then its frame.
pub fn paint_spinner(canvas: &mut Canvas<'_>, items: &[Item], theme: &Theme) {
    let Some(rect) = spinner_rect(items, theme) else {
        return;
    };
    canvas.fill(rect, [0., 0., 0.]);
    for item in items {
        if matches!(item, Item::Spinner { .. }) {
            paint_item(canvas, item, theme);
        }
    }
}

fn paint_item(canvas: &mut Canvas<'_>, item: &Item, theme: &Theme) {
    match item {
        Item::Watermark(at) => {
            if let Some(image) = &theme.watermark {
                canvas.blend(image, *at);
            }
        }
        Item::Spinner { frame, at } => {
            if let Some(image) = theme.spinner.get(*frame) {
                canvas.blend(image, *at);
            }
        }
        Item::Bar { rect, filled } => {
            canvas.fill(*rect, layout::BAR_BACKGROUND);
            if *filled > 0 {
                let done = Rect::new(rect.loc, Size::from((*filled, rect.size.h)));
                canvas.fill(done, layout::BAR_FOREGROUND);
            }
        }
        Item::Text { .. } => (),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plymouth::Mode;

    fn solid(w: i32, h: i32, pixel: u32) -> Image {
        Image {
            size: Size::from((w, h)),
            pixels: vec![pixel; (w * h) as usize],
        }
    }

    /// A theme of solid squares: a half-transparent white watermark, and spinner frames whose
    /// red channel is their index.
    fn theme() -> Theme {
        Theme {
            watermark: Some(solid(149, 43, 0x8080_8080)),
            spinner: (0..30)
                .map(|i| solid(32, 32, 0xff00_0000 | (i << 16)))
                .collect(),
        }
    }

    struct Buffer {
        size: Size,
        bytes: Vec<u8>,
    }

    impl Buffer {
        fn new(w: i32, h: i32) -> Self {
            Self {
                size: Size::from((w, h)),
                // Garbage the painter must cover.
                bytes: vec![0x5a; (w * h * 4) as usize],
            }
        }

        fn canvas(&mut self) -> Canvas<'_> {
            Canvas {
                size: self.size,
                stride: self.size.w as usize * 4,
                bytes: &mut self.bytes,
            }
        }

        fn at(&mut self, x: i32, y: i32) -> u32 {
            self.canvas().get(x, y)
        }
    }

    #[test]
    fn the_boot_splash_paints_where_the_layout_says() {
        let theme = theme();
        let splash = Splash {
            shown: true,
            ..Splash::default()
        };
        let mut buf = Buffer::new(1280, 800);
        let items = items(&splash, buf.size, &theme, 7);
        paint(&mut buf.canvas(), &items, &theme);
        // The layout's numbers for 1280×800 (`layout::tests`): watermark at 565×726, spinner
        // at 624×544.
        assert_eq!(buf.at(565, 726), 0xff80_8080);
        assert_eq!(buf.at(565 + 148, 726 + 42), 0xff80_8080);
        assert_eq!(buf.at(564, 726), 0xff00_0000);
        assert_eq!(buf.at(624, 544), 0xff07_0000);
        assert_eq!(buf.at(624 + 31, 544 + 31), 0xff07_0000);
        assert_eq!(buf.at(623, 544), 0xff00_0000);
        assert_eq!(buf.at(0, 0), 0xff00_0000);
        assert_eq!(buf.at(1279, 799), 0xff00_0000);
    }

    #[test]
    fn a_turn_repaints_only_the_spinner() {
        let theme = theme();
        let splash = Splash {
            shown: true,
            ..Splash::default()
        };
        let mut buf = Buffer::new(1280, 800);
        let size = buf.size;
        paint(&mut buf.canvas(), &items(&splash, size, &theme, 7), &theme);
        let before = buf.bytes.clone();
        paint_spinner(&mut buf.canvas(), &items(&splash, size, &theme, 8), &theme);
        assert_eq!(buf.at(624, 544), 0xff08_0000);
        // Everything outside the spinner is as it was.
        let changed = (0..800)
            .flat_map(|y| (0..1280).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                let at = (y * 1280 + x) as usize * 4;
                before[at..at + 4] != buf.bytes[at..at + 4]
            })
            .count();
        assert_eq!(changed, 32 * 32);
    }

    #[test]
    fn the_update_bar_fills_with_progress() {
        let theme = theme();
        let splash = Splash {
            mode: Mode::Updates,
            shown: true,
            progress: Some(50),
            ..Splash::default()
        };
        let mut buf = Buffer::new(1280, 800);
        let size = buf.size;
        paint(&mut buf.canvas(), &items(&splash, size, &theme, 0), &theme);
        // The bar at 440×556, 400×5: half white, half #606060.
        assert_eq!(buf.at(440, 556), 0xffff_ffff);
        assert_eq!(buf.at(639, 560), 0xffff_ffff);
        assert_eq!(buf.at(640, 556), 0xff60_6060);
        assert_eq!(buf.at(839, 560), 0xff60_6060);
        assert_eq!(buf.at(840, 556), 0xff00_0000);
    }

    #[test]
    fn what_falls_off_the_screen_is_clipped() {
        let theme = theme();
        let splash = Splash {
            shown: true,
            ..Splash::default()
        };
        // Smaller than the watermark: everything clips, nothing panics.
        let mut buf = Buffer::new(100, 30);
        let size = buf.size;
        paint(&mut buf.canvas(), &items(&splash, size, &theme, 0), &theme);
        assert_eq!(buf.at(0, 0), 0xff80_8080);
    }

    #[test]
    fn the_installed_theme_loads() {
        let dir = Path::new(layout::THEME_DIR);
        if !dir.join("watermark.png").exists() {
            eprintln!("skipping: no plymouth spinner theme");
            return;
        }
        let theme = Theme::load(dir);
        let watermark = theme.watermark.as_ref().unwrap();
        assert!(watermark.size.w > 0 && watermark.size.h > 0);
        assert_eq!(
            watermark.pixels.len(),
            (watermark.size.w * watermark.size.h) as usize
        );
        assert!(!theme.spinner.is_empty());
        // Premultiplied: no channel above its alpha.
        for p in &watermark.pixels {
            let a = p >> 24;
            assert!((p >> 16) & 0xff <= a && (p >> 8) & 0xff <= a && p & 0xff <= a);
        }
    }
}
