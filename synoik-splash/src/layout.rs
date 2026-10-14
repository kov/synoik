// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Where the boot splash puts things (`docs/fork/boot-splash.md` §4): Fedora's `bgrt` theme as
//! plymouth's `two-step` plugin lays it out (plymouth 24.004.60,
//! `src/plugins/splash/two-step/plugin.c`; the numbers are `bgrt.plymouth`'s). Pure geometry, in
//! logical pixels; the outer and the initramfs splash draw it.
//!
//! Positions truncate towards zero, as two-step's `long` coordinates do.

use std::time::Duration;

use crate::plymouth::{Mode, Splash};

/// A position in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl From<(i32, i32)> for Point {
    fn from((x, y): (i32, i32)) -> Self {
        Self { x, y }
    }
}

/// A size in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Size {
    pub w: i32,
    pub h: i32,
}

impl From<(i32, i32)> for Size {
    fn from((w, h): (i32, i32)) -> Self {
        Self { w, h }
    }
}

/// A rectangle in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub loc: Point,
    pub size: Size,
}

impl Rect {
    pub fn new(loc: Point, size: Size) -> Self {
        Self { loc, size }
    }
}

/// The theme's images: bgrt's `ImageDir`.
pub const THEME_DIR: &str = "/usr/share/plymouth/themes/spinner";

/// The faces the splash sets text in: bgrt's sizes and weights (`Font=Cantarell 12`,
/// `TitleFont=Cantarell Light 30`, pango points at 96 dpi), in the shell's own sans rather than
/// bgrt's Cantarell (§4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Font {
    /// Messages and subtitles: 12 pt.
    Regular,
    /// Update titles: 30 pt, light.
    Title,
}

impl Font {
    /// Pixels per em.
    pub fn px(self) -> f64 {
        match self {
            Self::Regular => 16.,
            Self::Title => 40.,
        }
    }

    /// CSS weight.
    pub fn weight(self) -> u16 {
        match self {
            Self::Regular => 400,
            Self::Title => 300,
        }
    }
}

/// bgrt's `ProgressBarBackgroundColor` and `ProgressBarForegroundColor`.
pub const BAR_BACKGROUND: [f32; 3] = [0x60 as f32 / 255., 0x60 as f32 / 255., 0x60 as f32 / 255.];
pub const BAR_FOREGROUND: [f32; 3] = [1., 1., 1.];

/// two-step's `PROGRESS_BAR_WIDTH` and `PROGRESS_BAR_HEIGHT`.
const BAR_SIZE: (i32, i32) = (400, 5);

/// bgrt's alignments.
const ANIMATION_ALIGN: (f64, f64) = (0.5, 0.7);
const WATERMARK_ALIGN: (f64, f64) = (0.5, 0.96);
const TITLE_ALIGN: (f64, f64) = (0.5, 0.382);

/// Messages sit this far under the spinner or the progress bar (`view_show_message`).
const MESSAGE_GAP: i32 = 10;

/// One turn of the spinner (`ply-throbber.c` `THROBBER_DURATION`).
pub const SPINNER_PERIOD: Duration = Duration::from_secs(2);

/// The spinner's frame rate (`ply-throbber.c` `FRAMES_PER_SECOND`): how often it needs a redraw.
pub const SPINNER_FRAME_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / 30);

/// The sizes of the images the theme provides; `None` for one that failed to load, which the
/// splash then leaves out.
#[derive(Debug, Clone, Copy, Default)]
pub struct Images {
    pub watermark: Option<Size>,
    /// One frame's size, and how many frames there are.
    pub spinner: Option<(Size, usize)>,
}

/// One thing on screen, over a black background.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    Watermark(Point),
    Spinner {
        frame: usize,
        at: Point,
    },
    /// The update progress bar: `filled` pixels of foreground from the left, background after.
    Bar {
        rect: Rect,
        filled: i32,
    },
    /// A line of text, `at` the top-left of its line box.
    Text {
        text: String,
        font: Font,
        at: Point,
    },
}

/// The update modes' title and subtitle (bgrt's `Title=` and `SubTitle=`).
fn titles(mode: Mode) -> Option<(&'static str, &'static str)> {
    let title = match mode {
        Mode::BootUp | Mode::Shutdown | Mode::Reboot => return None,
        Mode::Updates => "Installing Updates...",
        Mode::SystemUpgrade => "Upgrading System...",
        Mode::FirmwareUpgrade => "Upgrading Firmware...",
        Mode::SystemReset => "Resetting System...",
    };
    Some((title, "Do not turn off your computer"))
}

/// The spinner frame `elapsed` into its animation (`ply-throbber.c` `animate_at_time`).
pub fn spinner_frame(elapsed: Duration, frames: usize) -> usize {
    let period = SPINNER_PERIOD.as_secs_f64();
    let fraction = elapsed.as_secs_f64() % period / period;
    ((frames as f64 * fraction) as usize).min(frames.saturating_sub(1))
}

/// Lay out `splash` on a `screen` of logical pixels. `measure` gives a line's width and line-box
/// height in a font. `spinner_frame` is the frame to show, which a frozen splash (`H`) keeps.
///
/// A splash not `shown` is the frame plymouth leaves behind on `quit --retain-splash`: the
/// background and the watermark. It is what the outer shows from the handover until the greeter's
/// first frame.
pub fn layout(
    splash: &Splash,
    screen: Size,
    images: &Images,
    spinner_frame: usize,
    measure: &mut dyn FnMut(&str, Font) -> Size,
) -> Vec<Item> {
    let (w, h) = (f64::from(screen.w), f64::from(screen.h));
    let mut items = Vec::new();

    if let Some(size) = images.watermark {
        // `screen * align - image * align`, not `(screen - image) * align`: the same arithmetic,
        // rounded the same way.
        let x = w * WATERMARK_ALIGN.0 - f64::from(size.w) * WATERMARK_ALIGN.0;
        let y = h * WATERMARK_ALIGN.1 - f64::from(size.h) * WATERMARK_ALIGN.1;
        items.push(Item::Watermark(Point::from((x as i32, y as i32))));
    }
    if !splash.shown {
        return items;
    }

    // What the message sits under (`view->animation_bottom`).
    let mut animation_bottom = None;
    let titles = titles(splash.mode);
    if titles.is_some() {
        // `UseProgressBar=true`, and so no spinner (`UseAnimation` defaults to its negation).
        let (bw, bh) = BAR_SIZE;
        let x = ANIMATION_ALIGN.0 * (w - f64::from(bw));
        let y = ANIMATION_ALIGN.1 * (h - f64::from(bh));
        let rect = Rect::new((x as i32, y as i32).into(), (bw, bh).into());
        let fraction = f64::from(splash.progress.unwrap_or(0)) / 100.;
        let filled = (f64::from(bw) * fraction) as i32;
        items.push(Item::Bar { rect, filled });
        animation_bottom = Some(rect.loc.y + bh);
    } else if let Some((size, frames)) = images.spinner {
        let x = ANIMATION_ALIGN.0 * w - f64::from(size.w) / 2.;
        let y = ANIMATION_ALIGN.1 * h - f64::from(size.h) / 2.;
        let at = Point::from((x as i32, y as i32));
        items.push(Item::Spinner {
            frame: spinner_frame.min(frames.saturating_sub(1)),
            at,
        });
        animation_bottom = Some(at.y + size.h);
    }

    if let Some((title, subtitle)) = titles {
        let title_size = measure(title, Font::Title);
        let subtitle_size = measure(subtitle, Font::Regular);
        // The subtitle's height separates the two (`view_show_title`).
        let mut y = (h - f64::from(title_size.h) - 2. * f64::from(subtitle_size.h)) * TITLE_ALIGN.1;
        let x = (w - f64::from(title_size.w)) * TITLE_ALIGN.0;
        items.push(Item::Text {
            text: title.to_owned(),
            font: Font::Title,
            at: Point::from((x as i32, y as i32)),
        });
        y += f64::from(title_size.h + subtitle_size.h);
        let x = (w - f64::from(subtitle_size.w)) * TITLE_ALIGN.0;
        items.push(Item::Text {
            text: subtitle.to_owned(),
            font: Font::Regular,
            at: Point::from((x as i32, y as i32)),
        });
    }

    // One message line: the latest. The update modes suppress messages (`SuppressMessages`) and
    // put the percentage there once one is reported.
    let message = if titles.is_some() {
        splash.progress.map(|p| format!("{p}% complete"))
    } else {
        splash.messages.last().cloned()
    };
    if let Some(text) = message.filter(|t| !t.is_empty()) {
        let size = measure(&text, Font::Regular);
        let x = (w - f64::from(size.w)) * 0.5;
        // Without an animation two-step's `animation_bottom` is still 0.
        let y = animation_bottom.unwrap_or(0) + MESSAGE_GAP;
        items.push(Item::Text {
            text,
            font: Font::Regular,
            at: Point::from((x as i32, y)),
        });
    }

    items
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The installed theme's images.
    fn images() -> Images {
        Images {
            watermark: Some(Size::from((149, 43))),
            spinner: Some((Size::from((32, 32)), 30)),
        }
    }

    /// Every character 8 px wide, every line box 20 px tall; titles twice that.
    fn measure(text: &str, font: Font) -> Size {
        let k = if font == Font::Title { 2 } else { 1 };
        Size::from((8 * k * text.chars().count() as i32, 20 * k))
    }

    fn lay(splash: &Splash, w: i32, h: i32) -> Vec<Item> {
        layout(splash, Size::from((w, h)), &images(), 7, &mut measure)
    }

    fn shown(mode: Mode) -> Splash {
        Splash {
            mode,
            shown: true,
            ..Splash::default()
        }
    }

    #[test]
    fn the_retained_frame_is_the_watermark_alone() {
        // Measured on Limina at 1280×800: plymouth's watermark at 565×726.
        assert_eq!(
            lay(&Splash::default(), 1280, 800),
            [Item::Watermark(Point::from((565, 726)))]
        );
    }

    #[test]
    fn boot_shows_the_spinner_below_centre() {
        let items = lay(&shown(Mode::BootUp), 1280, 800);
        assert_eq!(
            items,
            [
                Item::Watermark(Point::from((565, 726))),
                Item::Spinner {
                    frame: 7,
                    at: Point::from((624, 544)),
                },
            ]
        );
        // Shutdown and reboot look the same.
        assert_eq!(lay(&shown(Mode::Shutdown), 1280, 800), items);
        assert_eq!(lay(&shown(Mode::Reboot), 1280, 800), items);
    }

    #[test]
    fn the_latest_message_sits_under_the_spinner() {
        let mut splash = shown(Mode::BootUp);
        splash.messages = vec!["older".into(), "Touch the key".into()];
        let items = lay(&splash, 1280, 800);
        // 13 characters, 104 px wide, centred; 10 px under the spinner's bottom at 576.
        assert_eq!(
            items.last(),
            Some(&Item::Text {
                text: "Touch the key".into(),
                font: Font::Regular,
                at: Point::from((588, 586)),
            })
        );
    }

    #[test]
    fn updates_show_a_bar_titles_and_the_percentage() {
        let mut splash = shown(Mode::Updates);
        splash.messages = vec!["suppressed".into()];
        splash.progress = Some(42);
        let items = lay(&splash, 1280, 800);
        // Title 40 px tall, subtitle 20: the pair starts at 0.382 × (800 − 40 − 40).
        assert_eq!(
            items,
            [
                Item::Watermark(Point::from((565, 726))),
                Item::Bar {
                    rect: Rect::new((440, 556).into(), (400, 5).into()),
                    filled: 168,
                },
                Item::Text {
                    text: "Installing Updates...".into(),
                    font: Font::Title,
                    at: Point::from((472, 275)),
                },
                Item::Text {
                    text: "Do not turn off your computer".into(),
                    font: Font::Regular,
                    at: Point::from((524, 335)),
                },
                Item::Text {
                    text: "42% complete".into(),
                    font: Font::Regular,
                    at: Point::from((592, 571)),
                },
            ]
        );
    }

    #[test]
    fn each_update_mode_has_its_title() {
        for (mode, title) in [
            (Mode::Updates, "Installing Updates..."),
            (Mode::SystemUpgrade, "Upgrading System..."),
            (Mode::FirmwareUpgrade, "Upgrading Firmware..."),
            (Mode::SystemReset, "Resetting System..."),
        ] {
            let items = lay(&shown(mode), 1280, 800);
            assert!(
                items.iter().any(|i| matches!(
                    i,
                    Item::Text { text, font: Font::Title, .. } if text == title
                )),
                "{mode:?}: {items:?}"
            );
            // No percentage before the first report, and no spinner.
            assert!(!items.iter().any(|i| matches!(i, Item::Spinner { .. })));
            assert!(!items
                .iter()
                .any(|i| matches!(i, Item::Text { text, .. } if text.ends_with("complete"))));
        }
    }

    #[test]
    fn the_bar_fills_with_progress() {
        let filled = |progress| {
            let mut splash = shown(Mode::SystemUpgrade);
            splash.progress = progress;
            lay(&splash, 1920, 1080)
                .into_iter()
                .find_map(|i| match i {
                    Item::Bar { filled, .. } => Some(filled),
                    _ => None,
                })
                .unwrap()
        };
        assert_eq!(filled(None), 0);
        assert_eq!(filled(Some(1)), 4);
        assert_eq!(filled(Some(100)), 400);
    }

    #[test]
    fn an_empty_message_shows_nothing() {
        let mut splash = shown(Mode::BootUp);
        splash.messages = vec![String::new()];
        assert!(!lay(&splash, 1280, 800)
            .iter()
            .any(|i| matches!(i, Item::Text { .. })));
    }

    #[test]
    fn missing_images_are_left_out() {
        let items = layout(
            &shown(Mode::BootUp),
            Size::from((1280, 800)),
            &Images::default(),
            0,
            &mut measure,
        );
        assert!(items.is_empty());
    }

    #[test]
    fn the_spinner_turns_once_every_two_seconds() {
        let ms = Duration::from_millis;
        assert_eq!(spinner_frame(ms(0), 30), 0);
        assert_eq!(spinner_frame(ms(66), 30), 0);
        assert_eq!(spinner_frame(ms(67), 30), 1);
        assert_eq!(spinner_frame(ms(1999), 30), 29);
        assert_eq!(spinner_frame(ms(2000), 30), 0);
        assert_eq!(spinner_frame(ms(5000), 30), 15);
        assert_eq!(spinner_frame(ms(5000), 0), 0);
    }
}
