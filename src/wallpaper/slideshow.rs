// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Time-of-day wallpapers: GNOME's XML slideshow format.
//!
//! `org.gnome.desktop.background picture-uri` may point at an XML file rather than a picture —
//! that is how a background changes with the hour. gnome-shell does not implement it either; it
//! wraps `GnomeDesktop.BGSlideShow` (`js/ui/background.js:646`), so the behaviour this module
//! ports lives in gnome-desktop 44.5, `libgnome-desktop/gnome-bg/gnome-bg-slide-show.c`.
//!
//! A slideshow is a `<starttime>` and a ring of slides, each either `<static>` (one picture for
//! `<duration>` seconds) or `<transition>` (a cross-fade `<from>` one picture `<to>` another).
//! The ring repeats forever: the position is the time since `starttime` **modulo** the summed
//! duration, which is why a file written in 2024 still lines up with today's clock.
//!
//! **Divergence — `<size>` variants.** `<file>`, `<from>` and `<to>` may each hold several
//! `<size width= height=>` alternatives, so a picture can be chosen per monitor; that is the only
//! reason gnome-shell keys animated backgrounds per monitor (`background.js:607`). We take the
//! first and keep one wallpaper for all outputs. No stock GNOME or Fedora slideshow uses variants,
//! and the selection rule (`find_best_size`, `slideshow.c:457`) is only worth porting once the
//! wallpaper itself is per-display.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
struct Slide {
    /// Seconds this slide occupies in the ring.
    duration: f64,
    /// `<static>` rather than `<transition>`. Kept for parity with the reference's `is_fixed`
    /// out-parameter; what actually distinguishes the two here is whether `to` is empty.
    fixed: bool,
    /// `<file>` for a static slide, `<from>` for a transition.
    from: PathBuf,
    /// `<to>`; `None` for a static slide.
    to: Option<PathBuf>,
}

/// A parsed slideshow. Immutable — every question about *what to show now* is answered by
/// [`current`](Self::current) from a clock the caller passes in.
#[derive(Debug, Clone, PartialEq)]
pub struct Slideshow {
    /// `<starttime>` as a Unix timestamp. The XML gives a *local* wall-clock time and the
    /// reference resolves it with `mktime` (`slideshow.c:684`), so a slideshow written for 8 AM
    /// starts at 8 AM in whatever zone the machine is in.
    start: f64,
    /// The summed slide durations: the length of one turn around the ring.
    total: f64,
    slides: Vec<Slide>,
}

/// What to draw right now.
pub struct CurrentSlide<'a> {
    /// How far into this slide we are, in `0.0..1.0`. For a transition this is the cross-fade
    /// factor: `to` at `progress`, over `from`.
    pub progress: f64,
    /// The slide's length in seconds — what the caller sizes its next wake-up from.
    pub duration: f64,
    pub from: &'a Path,
    /// `None` for a static slide.
    pub to: Option<&'a Path>,
}

impl Slideshow {
    /// Parse `xml`, the contents of a `.xml` picture-uri.
    ///
    /// `None` for anything we cannot show: not a `<background>`, no slides, or slides whose
    /// durations sum to zero — the position is a modulo by that sum, and a zero ring has no
    /// answer. The caller treats that exactly as a picture that would not decode.
    pub fn parse(xml: &str) -> Option<Self> {
        let doc = roxmltree::Document::parse(xml)
            .map_err(|err| warn!("could not parse the background slideshow: {err}"))
            .ok()?;
        let root = doc.root_element();
        if !root.has_tag_name("background") {
            warn!(
                "background slideshow root is <{}>, not <background>",
                root.tag_name().name()
            );
            return None;
        }

        let mut start = None;
        let mut slides: Vec<Slide> = Vec::new();
        for node in root.children().filter(|n| n.is_element()) {
            match node.tag_name().name() {
                "starttime" => start = parse_start_time(node),
                name @ ("static" | "transition") => {
                    let mut duration = 0.;
                    let mut from = None;
                    let mut to = None;
                    for child in node.children().filter(|n| n.is_element()) {
                        match child.tag_name().name() {
                            "duration" => {
                                duration = child
                                    .text()
                                    .and_then(|t| t.trim().parse().ok())
                                    .unwrap_or(0.)
                            }
                            "file" | "from" => from = picture(child),
                            "to" => to = picture(child),
                            _ => (),
                        }
                    }
                    let Some(from) = from else {
                        warn!("ignoring a <{name}> background slide with no picture");
                        continue;
                    };
                    slides.push(Slide {
                        duration,
                        fixed: name == "static",
                        from,
                        to,
                    });
                }
                _ => (),
            }
        }

        let total: f64 = slides.iter().map(|s| s.duration).sum();
        if slides.is_empty() || !total.is_finite() || total <= 0. {
            warn!("background slideshow has no slides to show");
            return None;
        }

        Some(Self {
            start: start.unwrap_or(0.),
            total,
            slides,
        })
    }

    /// Read `path` and parse it. Runs on the decode worker, alongside the decode it feeds, so a
    /// slideshow that cannot be read fails the same way and in the same place as a picture that
    /// cannot be decoded.
    pub fn open(path: &Path) -> Option<Self> {
        let xml = std::fs::read_to_string(path)
            .map_err(|err| warn!("could not read the background slideshow {path:?}: {err}"))
            .ok()?;
        Self::parse(&xml)
    }

    /// The slide covering `now`, a Unix timestamp in seconds.
    ///
    /// Ported from `gnome_bg_slide_show_get_current_slide` (`slideshow.c:513`): the ring position
    /// is `(now - start) mod total`, walked against the accumulated durations. A `now` *before*
    /// `start` — a slideshow dated in the future, or a clock that has gone backwards — wraps to
    /// the end of the ring rather than falling off the front.
    pub fn current(&self, now: f64) -> CurrentSlide<'_> {
        let mut delta = (now - self.start) % self.total;
        if delta < 0. {
            delta += self.total;
        }

        let mut elapsed = 0.;
        for slide in &self.slides {
            if elapsed + slide.duration > delta {
                return CurrentSlide {
                    progress: (delta - elapsed) / slide.duration,
                    duration: slide.duration,
                    from: &slide.from,
                    to: slide.to.as_deref(),
                };
            }
            elapsed += slide.duration;
        }

        // Unreachable while `delta < total`, which the modulo guarantees and a zero-length ring
        // cannot reach because `parse` refuses one. The reference asserts here; the last slide is
        // the picture the ring is about to wrap onto anyway.
        let last = self
            .slides
            .last()
            .expect("parse rejects an empty slideshow");
        CurrentSlide {
            progress: 1.,
            duration: last.duration,
            from: &last.from,
            to: last.to.as_deref(),
        }
    }

    /// How long to wait before asking [`current`](Self::current) again, for a slide of `duration`
    /// seconds.
    ///
    /// gnome-shell steps a cross-fade's opacity by 4/255 at a time and never wakes more often than
    /// once a second (`background.js:426-441`), which over Fedora's two-hour transition is a wake
    /// every ~113 s. Nothing here is animated in the frame-loop sense: a slideshow is a picture
    /// that changes a little, tens of times an hour. The upper bound is ours — a ten-hour static
    /// slide would otherwise arm a ten-hour timer, and a machine that suspended through it would
    /// come back to yesterday's picture until it fired.
    pub fn wakeup_interval(duration: f64) -> std::time::Duration {
        const OPACITY_STEP_INCREMENT: f64 = 4.0;
        const MIN_WAKEUP: f64 = 1.0;
        const MAX_WAKEUP: f64 = 3600.0;

        let steps = 255. / OPACITY_STEP_INCREMENT;
        let per_step = duration / steps;
        std::time::Duration::from_secs_f64(per_step.clamp(MIN_WAKEUP, MAX_WAKEUP))
    }
}

/// `<starttime>` as a Unix timestamp, resolved in the system time zone.
///
/// The reference fills a `struct tm` and calls `mktime` with `tm_isdst = -1`
/// (`slideshow.c:681-686`), i.e. "this is local wall-clock time, work out the offset yourself".
/// `jiff`'s system zone does the same job from the same `zoneinfo`.
fn parse_start_time(node: roxmltree::Node<'_, '_>) -> Option<f64> {
    let field = |name: &str| -> Option<i32> {
        node.children()
            .find(|n| n.is_element() && n.has_tag_name(name))?
            .text()?
            .trim()
            .parse()
            .ok()
    };

    let datetime = jiff::civil::DateTime::new(
        i16::try_from(field("year")?).ok()?,
        i8::try_from(field("month")?).ok()?,
        i8::try_from(field("day")?).ok()?,
        i8::try_from(field("hour").unwrap_or(0)).ok()?,
        i8::try_from(field("minute").unwrap_or(0)).ok()?,
        i8::try_from(field("second").unwrap_or(0)).ok()?,
        0,
    )
    .ok()?;

    let zoned = datetime
        .to_zoned(jiff::tz::TimeZone::system())
        .map_err(|err| warn!("background slideshow has an unusable <starttime>: {err}"))
        .ok()?;
    Some(zoned.timestamp().as_second() as f64)
}

/// The picture under a `<file>`, `<from>` or `<to>`: its own text, or the first `<size>` variant.
fn picture(node: roxmltree::Node<'_, '_>) -> Option<PathBuf> {
    // Whitespace is not a path — the newline between `<file>` and a `<size>` child is text too,
    // which is why the reference tests for it explicitly (`slideshow.c:405`).
    let text = node.text().map(str::trim).filter(|t| !t.is_empty());
    let variant = || {
        node.children()
            .find(|n| n.is_element() && n.has_tag_name("size"))?
            .text()
            .map(str::trim)
            .filter(|t| !t.is_empty())
    };

    text.or_else(variant).map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fedora's shipped slideshow, trimmed to its shape: day, fade to night, night, fade back.
    /// Every assertion below is a wall-clock question with an arithmetic answer, so the tests take
    /// `now` rather than reading a clock — they run everywhere, decode nothing, and cannot flake
    /// with the time of day the suite happens to run at.
    const DAY_NIGHT: &str = r#"
        <background>
          <starttime>
            <year>2024</year><month>10</month><day>22</day>
            <hour>8</hour><minute>00</minute><second>00</second>
          </starttime>
          <static><duration>36000.0</duration><file>/day.png</file></static>
          <transition type="overlay">
            <duration>7200.0</duration>
            <from>/day.png</from>
            <to>/night.png</to>
          </transition>
          <static><duration>36000.0</duration><file>/night.png</file></static>
          <transition type="overlay">
            <duration>7200.0</duration>
            <from>/night.png</from>
            <to>/day.png</to>
          </transition>
        </background>
    "#;

    fn day_night() -> Slideshow {
        Slideshow::parse(DAY_NIGHT).unwrap()
    }

    #[test]
    fn a_slideshow_is_a_ring_of_slides() {
        let show = day_night();
        assert_eq!(show.slides.len(), 4);
        assert_eq!(show.total, 86400.);
        assert!(show.slides[0].fixed);
        assert!(!show.slides[1].fixed);
        assert_eq!(show.slides[0].to, None, "a static slide has no <to>");
    }

    /// The hour of the day is the whole point: the same slideshow answers differently as the
    /// clock moves.
    #[test]
    fn the_slide_follows_the_clock() {
        let show = day_night();
        let at = |offset: f64| show.current(show.start + offset);

        // Ten hours of day, then two of fading to night, then ten of night.
        assert_eq!(at(0.).from, Path::new("/day.png"));
        assert_eq!(at(0.).to, None);
        assert_eq!(at(35_999.).from, Path::new("/day.png"));

        let fading = at(36_000. + 3_600.);
        assert_eq!(fading.from, Path::new("/day.png"));
        assert_eq!(fading.to, Some(Path::new("/night.png")));
        assert_eq!(fading.progress, 0.5, "halfway through a two-hour fade");
        assert_eq!(fading.duration, 7200.);

        assert_eq!(at(43_200. + 1.).from, Path::new("/night.png"));
        assert_eq!(at(43_200. + 1.).to, None);
    }

    /// A slideshow's `<starttime>` is in the past — often years back — so every reading is a
    /// wrapped one. A year later must look exactly like today.
    #[test]
    fn the_ring_repeats_forever() {
        let show = day_night();
        let today = show.current(show.start + 40_000.);
        let next_year = show.current(show.start + 40_000. + 365. * 86400.);

        assert_eq!(today.from, next_year.from);
        assert_eq!(today.to, next_year.to);
        assert!((today.progress - next_year.progress).abs() < 1e-9);
    }

    /// A clock *before* the start time — a slideshow dated in the future, or a machine whose
    /// clock has not been set yet — wraps to the end of the ring. Without the correction the
    /// remainder is negative and no slide matches, which would leave the desktop with no
    /// wallpaper at all rather than the wrong one.
    #[test]
    fn a_clock_before_the_start_time_wraps_backwards() {
        let show = day_night();
        let before = show.current(show.start - 3_600.);

        assert_eq!(before.from, Path::new("/night.png"));
        assert_eq!(before.to, Some(Path::new("/day.png")));
        assert_eq!(before.progress, 0.5, "an hour into the last two-hour fade");
    }

    /// `<size>` variants are a per-monitor choice we do not make yet, but a slideshow that uses
    /// them must still show *a* picture rather than none.
    #[test]
    fn a_sized_picture_falls_back_to_the_first_variant() {
        let show = Slideshow::parse(
            r#"
            <background>
              <starttime><year>2024</year><month>1</month><day>1</day>
                <hour>0</hour><minute>0</minute><second>0</second></starttime>
              <static>
                <duration>100.0</duration>
                <file>
                  <size width="1920" height="1080">/wide.png</size>
                  <size width="1600" height="1200">/square.png</size>
                </file>
              </static>
            </background>
            "#,
        )
        .unwrap();

        assert_eq!(show.current(show.start).from, Path::new("/wide.png"));
    }

    /// Files we cannot show must come back as `None` so the caller draws its solid backstop.
    /// A zero total duration is the one that would otherwise be a division by zero — and a
    /// slideshow of nothing but zero-length slides parses perfectly happily.
    #[test]
    fn an_unusable_slideshow_is_not_a_slideshow() {
        assert!(Slideshow::parse("not xml at all <").is_none());
        assert!(Slideshow::parse("<other><static/></other>").is_none());
        assert!(Slideshow::parse("<background></background>").is_none());
        assert!(
            Slideshow::parse(
                "<background><static><duration>0.0</duration><file>/a.png</file></static></background>"
            )
            .is_none(),
            "a ring of zero length has no position to read"
        );
    }

    /// GNOME's own wake-up sizing: one 4/255 opacity step of the slide, never under a second.
    #[test]
    fn the_wakeup_is_one_opacity_step() {
        // Fedora's two-hour transition.
        let fade = Slideshow::wakeup_interval(7200.);
        assert!((fade.as_secs_f64() - 7200. / (255. / 4.)).abs() < 1e-6);
        assert!(fade.as_secs() > 60 && fade.as_secs() < 180);

        // A very short slide still may not spin the loop.
        assert_eq!(Slideshow::wakeup_interval(1.).as_secs_f64(), 1.);
        // ...and a very long one still checks back, so a missed step cannot strand the picture.
        assert_eq!(Slideshow::wakeup_interval(1e9).as_secs(), 3600);
    }
}
