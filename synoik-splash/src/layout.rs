// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Where the boot splash puts things (`docs/fork/boot-splash.md` §4): Fedora's `bgrt` theme as
//! plymouth's `two-step` plugin lays it out (plymouth 24.004.60,
//! `src/plugins/splash/two-step/plugin.c`; the numbers are `bgrt.plymouth`'s). Pure geometry, in
//! logical pixels; the outer and the initramfs splash draw it.
//!
//! Positions truncate towards zero, as two-step's `long` coordinates do.

use std::path::{Path, PathBuf};
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

/// The watermark when the theme ships none: Debian's spinner has none, and its plymouth hook
/// copies this logo into the image in its place (`/usr/share/initramfs-tools/hooks/plymouth`).
pub const WATERMARK_FALLBACK: &str =
    "/usr/share/desktop-base/debian-logos/logo-text-version-64.png";

/// The watermark in theme directory `dir`, or [`WATERMARK_FALLBACK`] where `dir` has none.
pub fn watermark_file(dir: &Path) -> PathBuf {
    let own = dir.join("watermark.png");
    if own.exists() || !Path::new(WATERMARK_FALLBACK).exists() {
        own
    } else {
        PathBuf::from(WATERMARK_FALLBACK)
    }
}

/// The faces the splash sets text in: bgrt's sizes and weights (`Font=Cantarell 12`,
/// `TitleFont=Cantarell Light 30`, pango points at 96 dpi), in the shell's own sans rather than
/// bgrt's Cantarell (§4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Font {
    /// Messages, subtitles and the password prompt: 12 pt.
    Regular,
    /// Update titles: 30 pt, light.
    Title,
    /// The keyboard layout's name beside the keyboard icon: `keymap-render.py`'s 30 px bold, in
    /// its grey.
    Keymap,
}

impl Font {
    /// Pixels per em.
    pub fn px(self) -> f64 {
        match self {
            Self::Regular => 16.,
            Self::Title => 40.,
            Self::Keymap => 30.,
        }
    }

    /// CSS weight.
    pub fn weight(self) -> u16 {
        match self {
            Self::Regular => 400,
            Self::Title => 300,
            Self::Keymap => 700,
        }
    }

    /// The text's colour.
    pub fn color(self) -> [f32; 3] {
        match self {
            Self::Regular | Self::Title => [1., 1., 1.],
            Self::Keymap => [0.75, 0.75, 0.75],
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

/// bgrt's `DialogHorizontalAlignment` and `DialogVerticalAlignment`.
const DIALOG_ALIGN: (f64, f64) = (0.5, 0.382);

/// Fedora's gap between the entry and the prompt under it (§4.2).
const PROMPT_GAP: i32 = 5;

/// Between the keyboard icon and the layout's name (`ply-keymap-icon.c` `SPACING`).
const KEYMAP_SPACING: i32 = 10;

/// The band the layout's name sits in, and the margin on either side of it: `keymap-render.py`'s
/// strip, 50 px tall, its names `MARGIN` = 10 px apart from their neighbours.
const KEYMAP_BAND: i32 = 50;
const KEYMAP_MARGIN: i32 = 10;

/// One turn of the spinner (`ply-throbber.c` `THROBBER_DURATION`).
pub const SPINNER_PERIOD: Duration = Duration::from_secs(2);

/// The spinner's frame rate (`ply-throbber.c` `FRAMES_PER_SECOND`): how often it needs a redraw.
pub const SPINNER_FRAME_INTERVAL: Duration = Duration::from_nanos(1_000_000_000 / 30);

/// The firmware's boot logo (`crate::bgrt`): its size, where the firmware put its top-left, and
/// the mode of the display that stands for the panel the firmware drew it on ([`panel`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Firmware {
    pub size: Size,
    pub offset: Point,
    pub panel: Size,
}

/// The display two-step takes for the one the firmware drew its logo on, from every lit display in
/// the card's connector order: whether its connector is a built-in panel, and its mode. The last
/// built-in panel, else the first display (the drm renderer's `ply_renderer_head_new` keeps the
/// first output's mode unless a built-in one comes along).
pub fn panel(displays: impl IntoIterator<Item = (bool, Size)>) -> Option<Size> {
    let mut panel = None;
    for (built_in, size) in displays {
        if panel.is_none() || built_in {
            panel = Some(size);
        }
    }
    panel
}

/// Whether a connector, by its type's name (`eDP` in `eDP-1`), is a built-in panel: the drm
/// renderer's LVDS, eDP and DSI.
pub fn is_built_in(connector: &str) -> bool {
    let kind = connector
        .rsplit_once('-')
        .map_or(connector, |(kind, _)| kind);
    matches!(kind, "LVDS" | "eDP" | "DSI")
}

/// The sizes of the images the theme provides; `None` for one that failed to load, which the
/// splash then leaves out.
#[derive(Debug, Clone, Copy, Default)]
pub struct Images {
    /// Not the theme's, but drawn as its background in the modes that use it.
    pub firmware: Option<Firmware>,
    pub watermark: Option<Size>,
    /// One frame's size, and how many frames there are.
    pub spinner: Option<(Size, usize)>,
    /// The password dialog's: `lock.png`, `entry.png`, `bullet.png`, `keyboard.png` and
    /// `capslock.png`.
    pub lock: Option<Size>,
    pub entry: Option<Size>,
    pub bullet: Option<Size>,
    pub keyboard: Option<Size>,
    pub caps_lock: Option<Size>,
}

/// What the keyboard says about a password being typed: the drawers' side of the dialog, which
/// never crosses a handoff.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Dialog {
    /// One per character typed.
    pub bullets: usize,
    pub caps_lock: bool,
    /// The keyboard layout's name, as xkbcommon gives it ("English (US)").
    pub keymap: Option<String>,
}

/// One thing on screen, over a black background.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    /// The firmware's logo, part of the background: always first.
    Firmware(Point),
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
    /// The password dialog: the lock, the entry field, a bullet per character, the keyboard
    /// icon, and the caps-lock icon while caps lock is on.
    Lock(Point),
    Entry(Point),
    /// The part `src` of the bullet image, its top-left `at`: all of it, but for the half bullet
    /// that says more were typed than fit.
    Bullet {
        at: Point,
        src: Rect,
    },
    Keyboard(Point),
    CapsLock(Point),
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

/// Where the firmware's logo goes on a `screen` (two-step's `view_set_bgrt_background`, at
/// scale 1 and with neither the logo nor the panel rotated): centred horizontally with its centre
/// at 38.2 % of the height, the Microsoft boot logo rule; but where the firmware's own offset
/// centres it horizontally on the panel, that offset, the screen's difference from the panel split
/// as the default splits it. So on the panel itself, the logo stays where the firmware left it.
/// `None` when it would start off the screen's top or left.
///
/// two-step trusts the offsets on both axes alone, without a panel; its drm renderer always has
/// one, so that rule never applies.
fn place_firmware(firmware: &Firmware, screen: Size) -> Option<Point> {
    let Firmware {
        size,
        offset,
        panel,
    } = *firmware;
    // C's `int` arithmetic, truncating towards zero, as Rust's does.
    let (x, y) = if (panel.w - size.w) / 2 == offset.x {
        (
            offset.x + (screen.w - panel.w) / 2,
            offset.y + (screen.h - panel.h) * 382 / 1000,
        )
    } else {
        ((screen.w - size.w) / 2, screen.h * 382 / 1000 - size.h / 2)
    };
    (x >= 0 && y >= 0).then(|| Point::from((x, y)))
}

/// The spinner frame `elapsed` into its animation (`ply-throbber.c` `animate_at_time`).
pub fn spinner_frame(elapsed: Duration, frames: usize) -> usize {
    let period = SPINNER_PERIOD.as_secs_f64();
    let fraction = elapsed.as_secs_f64() % period / period;
    ((frames as f64 * fraction) as usize).min(frames.saturating_sub(1))
}

/// Lay out `splash` on a `screen` of logical pixels. `measure` gives a line's width and line-box
/// height in a font. `spinner_frame` is the frame to show, which a frozen splash (`H`) keeps.
/// `dialog` is what has been typed into a password prompt, if one is up.
///
/// A splash not `shown` is the frame plymouth leaves behind on `quit --retain-splash`: the
/// background, the firmware's logo where the mode shows it, and the watermark. It is what the outer
/// shows from the handover until the greeter's first frame.
pub fn layout(
    splash: &Splash,
    screen: Size,
    images: &Images,
    spinner_frame: usize,
    dialog: &Dialog,
    measure: &mut dyn FnMut(&str, Font) -> Size,
) -> Vec<Item> {
    let (w, h) = (f64::from(screen.w), f64::from(screen.h));
    let mut items = Vec::new();

    // bgrt's boot, shutdown and reboot modes draw the logo (`UseFirmwareBackground`); the update
    // modes do not, and a password dialog clears it (`DialogClearsFirmwareBackground`).
    let firmware = images
        .firmware
        .filter(|_| titles(splash.mode).is_none() && splash.password_prompt.is_none());
    if let Some(at) = firmware.and_then(|f| place_firmware(&f, screen)) {
        items.push(Item::Firmware(at));
    }
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

    // A password prompt replaces the animation, the progress bar and the titles (two-step's
    // `display_password` stops the animation; `on_draw` draws the dialog instead).
    let prompting = splash.password_prompt.is_some();
    if let Some(prompt) = &splash.password_prompt {
        lay_dialog(
            prompt.text.as_deref(),
            screen,
            images,
            dialog,
            measure,
            &mut items,
        );
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
        if !prompting {
            items.push(Item::Bar { rect, filled });
        }
        animation_bottom = Some(rect.loc.y + bh);
    } else if let Some((size, frames)) = images.spinner {
        let x = ANIMATION_ALIGN.0 * w - f64::from(size.w) / 2.;
        let y = ANIMATION_ALIGN.1 * h - f64::from(size.h) / 2.;
        let at = Point::from((x as i32, y as i32));
        if !prompting {
            items.push(Item::Spinner {
                frame: spinner_frame.min(frames.saturating_sub(1)),
                at,
            });
        }
        animation_bottom = Some(at.y + size.h);
    }

    if let Some((title, subtitle)) = titles.filter(|_| !prompting) {
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
    // The percentage is the progress bar's, and goes with it.
    let message = if titles.is_some() {
        splash
            .progress
            .filter(|_| !prompting)
            .map(|p| format!("{p}% complete"))
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

/// The password dialog, as two-step's `view_show_prompt` places it: the lock and the entry side by
/// side, the prompt under them, and under that the keyboard indicators.
fn lay_dialog(
    prompt: Option<&str>,
    screen: Size,
    images: &Images,
    dialog: &Dialog,
    measure: &mut dyn FnMut(&str, Font) -> Size,
    items: &mut Vec<Item>,
) {
    let (w, h) = (f64::from(screen.w), f64::from(screen.h));
    let lock = images.lock.unwrap_or_default();
    let entry = images.entry.unwrap_or_default();

    let dialog_size = Size::from((lock.w + entry.w, lock.h.max(entry.h)));
    let dialog_at = Point::from((
        ((w - f64::from(dialog_size.w)) * DIALOG_ALIGN.0) as i32,
        ((h - f64::from(dialog_size.h)) * DIALOG_ALIGN.1) as i32,
    ));
    let centred = |outer: i32, inner: i32| (f64::from(outer - inner) / 2.) as i32;
    if images.lock.is_some() {
        items.push(Item::Lock(Point::from((
            dialog_at.x,
            dialog_at.y + centred(dialog_size.h, lock.h),
        ))));
    }
    let entry_at = Point::from((
        dialog_at.x + lock.w,
        dialog_at.y + centred(dialog_size.h, entry.h),
    ));
    if images.entry.is_some() {
        items.push(Item::Entry(entry_at));
    }

    // `ply_entry_draw_area`: bullets a bullet's width apart, the first half a bullet in.
    if let Some(bullet) = images.bullet.filter(|b| b.w > 0 && dialog.bullets > 0) {
        let fit = entry.w / bullet.w - i32::from(entry.w % bullet.w < bullet.w / 2);
        let fit = usize::try_from(fit).unwrap_or(0);
        let y = (f64::from(entry_at.y) + f64::from(entry.h) / 2. - f64::from(bullet.h) / 2.) as i32;
        let whole = Rect::new(Point::default(), bullet);
        if dialog.bullets > fit {
            // More than fit: half a bullet at the entry's left edge says so.
            let x = (f64::from(entry_at.x) - f64::from(bullet.w) / 2.) as i32;
            let cut = entry_at.x - x;
            items.push(Item::Bullet {
                at: Point::from((entry_at.x, y)),
                src: Rect::new((cut, 0).into(), (bullet.w - cut, bullet.h).into()),
            });
        }
        for i in 0..dialog.bullets.min(fit) as i32 {
            let x = f64::from(entry_at.x + i * bullet.w) + f64::from(bullet.w) / 2.;
            items.push(Item::Bullet {
                at: Point::from((x as i32, y)),
                src: whole,
            });
        }
    }

    let mut bottom = dialog_at.y + dialog_size.h;
    // Fedora's prompts end without the ':' upstream's carry (§4.2).
    if let Some(text) = prompt
        .map(|p| p.strip_suffix(':').unwrap_or(p))
        .filter(|p| !p.is_empty())
    {
        let size = measure(text, Font::Regular);
        let y = bottom + PROMPT_GAP;
        items.push(Item::Text {
            text: text.to_owned(),
            font: Font::Regular,
            at: Point::from((((w - f64::from(size.w)) / 2.) as i32, y)),
        });
        bottom = y + size.h;
    }

    // The keyboard indicators: the layout (icon and name), then caps lock beside it. Only the
    // layout's width centres them.
    let Some(keyboard) = images.keyboard else {
        return;
    };
    let name = dialog.keymap.as_deref().filter(|n| !n.is_empty());
    let name_size = name.map(|n| measure(n, Font::Keymap));
    let band = Size::from((
        name_size.map_or(0, |s| s.w + 2 * KEYMAP_MARGIN),
        if name.is_some() { KEYMAP_BAND } else { 0 },
    ));
    let keymap = Size::from((keyboard.w + KEYMAP_SPACING + band.w, keyboard.h.max(band.h)));
    let caps = images.caps_lock.unwrap_or_default();
    let indicators_h = caps.h.max(keymap.h);
    let x = ((w - f64::from(keymap.w)) * DIALOG_ALIGN.0) as i32;
    let y = (f64::from(bottom + indicators_h / 2) + f64::from(indicators_h - keymap.h) / 2.) as i32;
    items.push(Item::Keyboard(Point::from((
        x,
        y + (keymap.h - keyboard.h) / 2,
    ))));
    if let (Some(name), Some(size)) = (name, name_size) {
        // Where plymouth shows its strip's slice: the name starts at the slice's left edge, the
        // two margins after it; centred in the band.
        let band_y = y + (keymap.h - band.h) / 2;
        items.push(Item::Text {
            text: name.to_owned(),
            font: Font::Keymap,
            at: Point::from((
                x + keyboard.w + KEYMAP_SPACING,
                band_y + (band.h - size.h) / 2,
            )),
        });
    }
    if dialog.caps_lock && images.caps_lock.is_some() {
        let y =
            (f64::from(bottom + indicators_h / 2) + f64::from(indicators_h - caps.h) / 2.) as i32;
        items.push(Item::CapsLock(Point::from((x + keymap.w, y))));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The installed theme's images.
    fn images() -> Images {
        Images {
            firmware: None,
            watermark: Some(Size::from((149, 43))),
            spinner: Some((Size::from((32, 32)), 30)),
            lock: Some(Size::from((35, 34))),
            entry: Some(Size::from((305, 34))),
            bullet: Some(Size::from((10, 10))),
            keyboard: Some(Size::from((36, 36))),
            caps_lock: Some(Size::from((24, 28))),
        }
    }

    /// Every character 8 px wide, every line box 20 px tall; titles twice that.
    fn measure(text: &str, font: Font) -> Size {
        let k = if font == Font::Title { 2 } else { 1 };
        Size::from((8 * k * text.chars().count() as i32, 20 * k))
    }

    fn lay(splash: &Splash, w: i32, h: i32) -> Vec<Item> {
        layout(
            splash,
            Size::from((w, h)),
            &images(),
            7,
            &Dialog::default(),
            &mut measure,
        )
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
            &Dialog::default(),
            &mut measure,
        );
        assert!(items.is_empty());
    }

    fn prompting(mode: Mode, text: Option<&str>) -> Splash {
        let mut splash = shown(mode);
        splash.password_prompt = Some(crate::plymouth::Prompt {
            text: text.map(str::to_owned),
        });
        splash
    }

    fn lay_dialog(splash: &Splash, dialog: &Dialog) -> Vec<Item> {
        layout(
            splash,
            Size::from((1280, 800)),
            &images(),
            7,
            dialog,
            &mut measure,
        )
    }

    #[test]
    fn a_password_prompt_replaces_the_spinner() {
        let dialog = Dialog {
            bullets: 3,
            caps_lock: true,
            keymap: Some("English (US)".into()),
        };
        let items = lay_dialog(
            &prompting(Mode::BootUp, Some("Passphrase for data:")),
            &dialog,
        );
        // The dialog, 340×34, at (0.5 × (1280 − 340), 0.382 × (800 − 34)).
        assert_eq!(
            items,
            [
                Item::Watermark(Point::from((565, 726))),
                Item::Lock(Point::from((470, 292))),
                Item::Entry(Point::from((505, 292))),
                Item::Bullet {
                    at: Point::from((510, 304)),
                    src: Rect::new((0, 0).into(), (10, 10).into()),
                },
                Item::Bullet {
                    at: Point::from((520, 304)),
                    src: Rect::new((0, 0).into(), (10, 10).into()),
                },
                Item::Bullet {
                    at: Point::from((530, 304)),
                    src: Rect::new((0, 0).into(), (10, 10).into()),
                },
                // Without its ':', 19 characters, 5 px under the entry.
                Item::Text {
                    text: "Passphrase for data".into(),
                    font: Font::Regular,
                    at: Point::from((564, 331)),
                },
                // The layout: 36 + 10 + (96 + 20) = 162 wide, its 50 px band the tallest, half
                // of it under the prompt's bottom at 351.
                Item::Keyboard(Point::from((559, 383))),
                Item::Text {
                    text: "English (US)".into(),
                    font: Font::Keymap,
                    at: Point::from((605, 391)),
                },
                Item::CapsLock(Point::from((721, 387))),
            ]
        );
    }

    #[test]
    fn a_full_entry_shows_half_a_bullet_at_its_edge() {
        let dialog = Dialog {
            bullets: 31,
            ..Dialog::default()
        };
        let items = lay_dialog(&prompting(Mode::BootUp, None), &dialog);
        let bullets: Vec<_> = items
            .iter()
            .filter_map(|i| match i {
                Item::Bullet { at, src } => Some((*at, *src)),
                _ => None,
            })
            .collect();
        // 305 / 10 = 30 whole ones, and the right half of one more at the entry's left edge.
        assert_eq!(bullets.len(), 31);
        assert_eq!(
            bullets[0],
            (
                Point::from((505, 304)),
                Rect::new((5, 0).into(), (5, 10).into())
            )
        );
        assert_eq!(bullets[30].0, Point::from((800, 304)));
        // No prompt and no layout name: the icon alone, 46 wide with its spacing, half its
        // height under the dialog's bottom at 326; caps lock off.
        assert!(items.contains(&Item::Keyboard(Point::from((617, 344)))));
        assert!(!items.iter().any(|i| matches!(i, Item::CapsLock(_))));
    }

    #[test]
    fn a_prompt_over_an_update_hides_the_bar_and_titles() {
        let mut splash = prompting(Mode::Updates, Some("Password"));
        splash.progress = Some(42);
        let items = lay_dialog(&splash, &Dialog::default());
        assert!(!items.iter().any(|i| matches!(
            i,
            Item::Bar { .. }
                | Item::Spinner { .. }
                | Item::Text {
                    font: Font::Title,
                    ..
                }
        )));
        assert!(!items
            .iter()
            .any(|i| matches!(i, Item::Text { text, .. } if text.ends_with("complete"))));
    }

    #[test]
    fn messages_stay_under_the_hidden_spinner() {
        let mut splash = prompting(Mode::BootUp, None);
        splash.messages = vec!["Touch the key".into()];
        let items = lay_dialog(&splash, &Dialog::default());
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

    /// Limina's logo: 256×256, centred on its 1280×800 display.
    fn with_logo(panel: (i32, i32)) -> Images {
        Images {
            firmware: Some(Firmware {
                size: Size::from((256, 256)),
                offset: Point::from((512, 272)),
                panel: Size::from(panel),
            }),
            ..images()
        }
    }

    fn lay_with(splash: &Splash, images: &Images, w: i32, h: i32) -> Vec<Item> {
        layout(
            splash,
            Size::from((w, h)),
            images,
            7,
            &Dialog::default(),
            &mut measure,
        )
    }

    #[test]
    fn the_logo_stays_where_the_firmware_left_it() {
        // Measured on Limina at 1280×800 (`--acpi`): the firmware's offsets centre the logo on
        // both axes, and the panel is the display.
        let items = lay_with(&shown(Mode::BootUp), &with_logo((1280, 800)), 1280, 800);
        assert_eq!(items[0], Item::Firmware(Point::from((512, 272))));
        assert!(matches!(items[1], Item::Watermark(_)));
    }

    #[test]
    fn another_display_splits_the_difference_as_the_default_does() {
        // 1920×1080 beside the 1280×800 panel: the panel's offset, plus half the extra width
        // and 38.2 % of the extra height.
        let items = lay_with(&shown(Mode::BootUp), &with_logo((1280, 800)), 1920, 1080);
        assert_eq!(
            items[0],
            Item::Firmware(Point::from((512 + 320, 272 + 280 * 382 / 1000)))
        );
    }

    #[test]
    fn offsets_for_another_mode_give_the_golden_ratio() {
        // The firmware drew at 1024×768 (centred: x = 384), the panel came up at 1280×800: the
        // offsets are not the panel's, so the Microsoft rule places it.
        let images = Images {
            firmware: Some(Firmware {
                size: Size::from((256, 256)),
                offset: Point::from((384, 256)),
                panel: Size::from((1280, 800)),
            }),
            ..images()
        };
        let items = lay_with(&shown(Mode::BootUp), &images, 1280, 800);
        assert_eq!(
            items[0],
            Item::Firmware(Point::from((512, 800 * 382 / 1000 - 128)))
        );
    }

    #[test]
    fn a_logo_starting_off_screen_is_left_out() {
        let items = lay_with(&shown(Mode::BootUp), &with_logo((1280, 800)), 200, 100);
        assert!(!items.iter().any(|i| matches!(i, Item::Firmware(_))));
    }

    #[test]
    fn the_logo_shows_where_bgrt_uses_the_firmware_background() {
        let logo = with_logo((1280, 800));
        let has_logo = |splash: &Splash| {
            lay_with(splash, &logo, 1280, 800)
                .iter()
                .any(|i| matches!(i, Item::Firmware(_)))
        };
        for mode in [Mode::BootUp, Mode::Shutdown, Mode::Reboot] {
            assert!(has_logo(&shown(mode)), "{mode:?}");
            // And in the frame `quit --retain-splash` leaves.
            assert!(
                has_logo(&Splash {
                    mode,
                    ..Splash::default()
                }),
                "{mode:?}"
            );
        }
        for mode in [
            Mode::Updates,
            Mode::SystemUpgrade,
            Mode::FirmwareUpgrade,
            Mode::SystemReset,
        ] {
            assert!(!has_logo(&shown(mode)), "{mode:?}");
        }
        // A password dialog clears it.
        assert!(!has_logo(&prompting(Mode::BootUp, Some("Passphrase:"))));
    }

    #[test]
    fn the_panel_is_the_built_in_display_else_the_first() {
        let (a, b, c) = (
            Size::from((1920, 1080)),
            Size::from((2560, 1600)),
            Size::from((1280, 800)),
        );
        assert_eq!(panel([(false, a), (true, b), (false, c)]), Some(b));
        assert_eq!(panel([(false, a), (false, c)]), Some(a));
        assert_eq!(panel([]), None);
        assert!(is_built_in("eDP-1") && is_built_in("LVDS-2") && is_built_in("DSI-1"));
        assert!(!is_built_in("Virtual-1") && !is_built_in("DP-1") && !is_built_in("HDMI-A-1"));
    }
}
