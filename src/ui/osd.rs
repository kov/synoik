// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The on-screen display — gnome-shell 50.1's `OsdWindow` / `OsdWindowManager`
//! (`js/ui/osdWindow.js`).
//!
//! A small pill at the bottom of each monitor showing an icon, an optional label
//! and an optional level bar: volume, mute, microphone, keyboard backlight,
//! rotation lock, screen brightness. **Never interactive** — nothing in
//! `osdWindow.js` is reactive, so this type has no hit-test and no pointer state.
//!
//! The shape of the subsystem is worth stating once, because it decides who calls
//! this: `ShowOSD` is an *inbound* method on `org.gnome.Shell`
//! (`js/ui/shellDBus.js:121-153`) that gsd-media-keys calls after handling a volume
//! key — GNOME's shell does not handle those keys itself. Brightness is the mirror
//! image: the shell owns those keys, so `brightnessManager.js:264-276` calls the OSD
//! directly. See `docs/fork/osd-media-port.md`.
//!
//! Timing (`osdWindow.js:10-12`): a 100 ms `EASE_OUT_QUAD` fade in and out, and a
//! **1500 ms** hide timeout re-armed on every `show()`. A second OSD arriving while
//! one is up replaces its content **in place with no re-fade** (`:94-111` only
//! animates on the hidden→visible edge) and the level value *eases* 100 ms rather
//! than snapping (`:71-84`). `show()` with no icon does nothing (`:90-92`).
//!
//! One window per output, kept in step by [`OsdManager::add_output`] /
//! [`remove_output`](OsdManager::remove_output) — GNOME rebuilds its array on
//! `monitors-changed` (`:143-163`). [`OsdManager::show`] takes a per-output level
//! map and **cancels** the windows absent from it (`:172-182`), which is how the
//! brightness manager shows an OSD only on the monitors that actually changed.
//!
//! Deliberately not here: the monitor-number label (`osdMonitorLabeler`, which
//! belongs to display config), the pad OSD, and the resize popup — each is its own
//! surface that merely shares `%osd_panel`.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use smithay::backend::renderer::element::Kind;
use smithay::output::Output;
use smithay::utils::{Logical, Physical, Point, Rectangle, Size, Transform};
use synoik_config::Config;

use crate::animation::{Animation, Clock, Curve};
use crate::render_helpers::icon::IconCache;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanFrame, VulkanRenderer};
use crate::ui::widget::{self, style, Align, BarLevel, BarLevelStyle, Painter, ShapedText};
use crate::utils::output_size;

/// `HIDE_TIMEOUT` (`js/ui/osdWindow.js:10`).
const HIDE_TIMEOUT: Duration = Duration::from_millis(1500);
/// `FADE_TIME` (`js/ui/osdWindow.js:11`).
const FADE_TIME_MS: u64 = 100;
/// `LEVEL_ANIMATION_TIME` (`js/ui/osdWindow.js:12`).
const LEVEL_TIME_MS: u64 = 100;

/// `%heading` — 11pt/700 (`_common.scss:266-269`), applied by `.osd-window`.
const TEXT_PT: f64 = 11.;
/// `padding: $base_padding * 2 $base_padding * 3` (`_osd.scss:11`).
const PADDING_V: f64 = 12.;
const PADDING_H: f64 = 18.;
/// `spacing: $base_padding * 2` — between the icon and the label/level column.
const SPACING: f64 = 12.;
/// `& > * { spacing: $base_margin * 2 }` — inside that column (`_osd.scss:12`).
const INNER_SPACING: f64 = 8.;
/// `.osd-window StIcon { icon-size: $large_icon_size }` (`_osd.scss:15`).
const ICON_PX: f64 = 32.;
/// `border: 1px solid $osd_outer_borders_color` (`%osd_panel`, `_common.scss:294`).
const BORDER: f64 = 1.;
/// `StLabel:ltr { margin-right: $base_padding }` (`_osd.scss:18`) and the same on
/// `.level` (`_osd.scss:32`) — both trailing margins, so the column is that much
/// wider than its content.
const TRAILING_MARGIN: f64 = 6.;
/// `.level { margin-bottom: $base_margin }`, dropped to 0 when the level is the
/// column's first child, i.e. when there is no label (`_osd.scss:23-24`).
const LEVEL_MARGIN_BOTTOM: f64 = 4.;
/// `margin-bottom: 4em` (`_osd.scss:14`) — em against the element's own 11pt font.
const MARGIN_BOTTOM_EM: f64 = 4.;

/// `%osd_panel`'s colours, shared with the Alt-Tab `.switcher-list` — see [`style::OSD_BG`].
const OSD_BG: widget::Rgba = style::OSD_BG;
const OSD_FG: widget::Rgba = style::OSD_FG;
const OSD_BORDER: widget::Rgba = style::OSD_BORDER;
/// The level bar's theme colors (`_osd.scss:27-30`); the dark variant's track is
/// `transparentize($osd_fg_color, 0.9)`, and the overdrive is `$destructive_color`.
const LEVEL_STYLE: BarLevelStyle = BarLevelStyle {
    track: [1., 1., 1., 0.1],
    fill: OSD_FG,
    overdrive: style::DESTRUCTIVE_BG,
    separator: OSD_FG,
};

/// 1em — the realized base font (`crate::ui::base_font_pt`), not the nominal 11pt.
fn em() -> f64 {
    crate::ui::pt_to_px(TEXT_PT)
}

/// What one OSD shows. An empty `icon` candidate list means "no icon", which makes
/// [`OsdManager::show`] a no-op for that output (`osdWindow.js:90-92`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OsdContent {
    /// Symbolic icon-name candidates, first that resolves wins.
    pub icon: Vec<String>,
    pub label: Option<String>,
    pub level: Option<f64>,
    /// `maximum-value`, clamped to at least 1 like the property setter
    /// (`barLevel.js:73-80`). Above 1 the bar grows an overdrive segment starting at
    /// 1.0, since nothing ever sets `overdrive-start` — that is how amplified volume
    /// renders red past 100%.
    pub max_level: f64,
}

impl OsdContent {
    /// An icon-only OSD (mute, rotation lock).
    pub fn icon(candidates: &[&str]) -> Self {
        Self {
            icon: candidates.iter().map(|s| (*s).to_owned()).collect(),
            max_level: 1.,
            ..Self::default()
        }
    }

    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    pub fn with_level(mut self, level: f64, max_level: f64) -> Self {
        self.level = Some(level);
        self.max_level = max_level.max(1.);
        self
    }
}

/// Icon-name candidates from a **serialized `GIcon`** — what `ShowOSD` carries
/// (`js/ui/shellDBus.js:140-142` runs it through `Gio.Icon.new_for_string`).
///
/// Two forms reach us in practice: a bare theme name (`g_icon_to_string` of a
/// single-name `GThemedIcon` is just the name), and the serialized list
/// `". GThemedIcon name1 name2 …"` that `g_themed_icon_new_with_default_fallbacks`
/// produces — which maps exactly onto our first-that-resolves candidate list.
///
/// **Divergence:** the other `GIcon` kinds — `GFileIcon` (a path or `file://` URI)
/// and `GBytesIcon` — yield no candidates, so the OSD is refused rather than
/// showing a loaded image. Our icon cache resolves theme names only; nothing on
/// this desktop sends anything else to `ShowOSD`.
pub fn icon_candidates(serialized: &str) -> Vec<String> {
    let s = serialized.trim();
    if let Some(rest) = s.strip_prefix(". GThemedIcon ") {
        return rest.split_whitespace().map(str::to_owned).collect();
    }
    // A serialized non-themed icon, a path, or a URI. A theme name never contains
    // a slash, a colon or a space, so those characters are what rules it out —
    // testing only the leading character would let `file:///x.png` through as a
    // name.
    if s.is_empty() || s.starts_with('.') || s.contains(['/', ':', ' ']) {
        return Vec::new();
    }
    vec![s.to_owned()]
}

/// Where each piece of an OSD sits inside its own bake buffer.
#[derive(Debug, Clone, Copy)]
struct OsdLayout {
    size: Size<f64, Logical>,
    icon_center: Point<f64, Logical>,
    /// The label's allocation; text is centered in it (`text-align: center`).
    label: Option<Rectangle<f64, Logical>>,
    level: Option<Rectangle<f64, Logical>>,
}

fn layout(content: &OsdContent) -> OsdLayout {
    let px = em();
    let label_w = content.label.as_ref().map(|t| {
        synoik_vk::text::measure_line_width_weighted(t, px as f32, true) + TRAILING_MARGIN
    });
    // The caption's line box, not a ceil of the em: ceiling the font size is a fourth private
    // spelling of a rule that belongs in one place, and it is short — the box is
    // `ceil(ascent) + ceil(descent)`, which at 11pt is 19 against this 15.
    let label_h = crate::ui::line_height_px(TEXT_PT);
    let level_w = content
        .level
        .is_some()
        .then_some(BarLevel::MIN_WIDTH + TRAILING_MARGIN);

    let column_w = label_w.unwrap_or(0.).max(level_w.unwrap_or(0.));
    let column_h = match (label_w.is_some(), level_w.is_some()) {
        (true, true) => label_h + INNER_SPACING + BarLevel::HEIGHT + LEVEL_MARGIN_BOTTOM,
        (true, false) => label_h,
        // First child, so no bottom margin.
        (false, true) => BarLevel::HEIGHT,
        (false, false) => 0.,
    };

    let content_w = ICON_PX
        + if column_w > 0. {
            SPACING + column_w
        } else {
            0.
        };
    let content_h = ICON_PX.max(column_h);
    let size = Size::from((
        content_w + 2. * (PADDING_H + BORDER),
        content_h + 2. * (PADDING_V + BORDER),
    ));

    let left = BORDER + PADDING_H;
    let top = BORDER + PADDING_V;
    // The icon is `y_expand`, so it drives the row height and centers in it.
    let icon_center = Point::from((left + ICON_PX / 2., top + content_h / 2.));
    // The column is `y_align: CENTER` (`osdWindow.js:36-38`).
    let column_x = left + ICON_PX + SPACING;
    let column_y = top + (content_h - column_h) / 2.;

    // Both children fill the column's width; the label centers its text in that
    // allocation (`text-align: center`, `_osd.scss:9`).
    let label = label_w.map(|_| {
        Rectangle::new(
            Point::from((column_x, column_y)),
            Size::from((column_w - TRAILING_MARGIN, label_h)),
        )
    });
    let level = level_w.map(|_| {
        let y = column_y + label.map_or(0., |l| l.size.h + INNER_SPACING);
        Rectangle::new(
            Point::from((column_x, y)),
            // The bar fills the column (vertical boxes allocate FILL across), so a
            // label longer than 160px widens the bar past its min-width too.
            Size::from((column_w - TRAILING_MARGIN, BarLevel::HEIGHT)),
        )
    });

    OsdLayout {
        size,
        icon_center,
        label,
        level,
    }
}

enum State {
    Hidden,
    Showing(Animation),
    Shown,
    Hiding(Animation),
}

/// One monitor's OSD.
struct OsdWindow {
    state: State,
    content: OsdContent,
    /// The level actually drawn: it eases toward `content.level` while the window is
    /// already visible, and snaps when it is not (`osdWindow.js:71-84`).
    level_anim: Option<Animation>,
    /// When the OSD takes itself away. Armed by `show()` — **not** by the
    /// Showing→Shown transition: GNOME starts the 1500 ms timeout at `show()` time,
    /// concurrently with the fade-in (`osdWindow.js:107-110`), so a `show()` that
    /// lands mid-fade re-arms it just the same. Keeping it out of the state enum is
    /// what makes that expressible, and it is the only place a deadline is ever set,
    /// which is what `Synoik::reschedule_osd_timer` needs in order to stay in step.
    deadline: Option<Duration>,
    /// Bumped when the *baked chrome* changes (label text, geometry) — deliberately
    /// NOT by the level, whose animated value would otherwise re-bake the whole pill
    /// every frame. The level bar is its own small bake, keyed on the physical pixel
    /// its end cap lands on.
    revision: u64,
    chrome_cache: RefCell<widget::BakeCache>,
    level_cache: RefCell<widget::BakeCache>,
    clock: Clock,
    config: Rc<RefCell<Config>>,
}

impl OsdWindow {
    fn new(clock: Clock, config: Rc<RefCell<Config>>) -> Self {
        Self {
            state: State::Hidden,
            content: OsdContent::default(),
            level_anim: None,
            deadline: None,
            revision: 0,
            chrome_cache: RefCell::new(widget::BakeCache::new()),
            level_cache: RefCell::new(widget::BakeCache::new()),
            clock,
            config,
        }
    }

    /// GNOME's OSD timings are fixed constants, not config knobs; the only knob we
    /// honor is the global animations-off switch, expressed as a zero duration.
    fn ease(&self, from: f64, to: f64, ms: u64) -> Animation {
        let ms = if self.config.borrow().animations.off {
            0
        } else {
            ms
        };
        Animation::ease(self.clock.clone(), from, to, 0., ms, Curve::EaseOutQuad)
    }

    fn is_visible(&self) -> bool {
        !matches!(self.state, State::Hidden)
    }

    fn displayed_level(&self) -> f64 {
        match &self.level_anim {
            Some(anim) => anim.value(),
            None => self.content.level.unwrap_or(0.),
        }
    }

    fn show(&mut self, content: OsdContent) {
        if content.icon.is_empty() {
            return;
        }
        let visible = self.is_visible();

        // The level eases only when the window is already up; otherwise it snaps.
        // Either way any running ease is replaced — `ease_property` does
        // (`osdWindow.js:75-78`), and leaving a stale one would keep animating toward
        // the *previous* target and then jump back (volume up then down in one frame).
        // Read where the bar *is* before dropping the running ease — clearing first
        // would fall back to the previous target instead of the current position.
        let from = self.displayed_level();
        self.level_anim = None;
        if let (true, Some(level)) = (visible, content.level) {
            if from != level {
                self.level_anim = Some(self.ease(from, level, LEVEL_TIME_MS));
            }
        }

        // Only the chrome's own inputs bump the revision — see `revision`.
        let chrome_changed = self.content.label != content.label
            || self.content.level.is_some() != content.level.is_some()
            || self.content.max_level != content.max_level;
        if chrome_changed {
            self.revision += 1;
        }
        self.content = content;

        // The fade runs ONLY on the hidden→visible edge (`osdWindow.js:94-105` guards
        // it with `if (!this.visible)`); every other state keeps whatever opacity
        // transition it already has. In particular a second show during the 100 ms
        // fade-in must let that fade finish rather than snapping to opaque, and one
        // during the fade-*out* does not resurrect the OSD: GNOME's actor is still
        // `visible` until `_reset()` (`:136`), so the ease-to-0 completes and the OSD
        // it was asked to show is genuinely lost. A 100 ms window, but faithful.
        if !visible {
            self.state = State::Showing(self.ease(0., 1., FADE_TIME_MS));
        }
        // Re-armed on every show, whatever the state (`osdWindow.js:107-110`).
        self.deadline = Some(self.clock.now_unadjusted() + HIDE_TIMEOUT);
    }

    /// Kill the timer and start hiding now (`osdWindow.js:114-120`).
    fn cancel(&mut self) {
        let from = match &self.state {
            State::Hidden | State::Hiding(_) => return,
            State::Showing(anim) => anim.value(),
            State::Shown => 1.,
        };
        self.deadline = None;
        self.state = State::Hiding(self.ease(from, 0., FADE_TIME_MS));
    }

    fn next_wakeup(&self) -> Option<Duration> {
        self.deadline
    }

    fn advance_animations(&mut self) {
        if let Some(anim) = &self.level_anim {
            if anim.is_done() {
                self.level_anim = None;
            }
        }
        match &mut self.state {
            State::Hidden | State::Shown => (),
            State::Showing(anim) => {
                if anim.is_done() {
                    self.state = State::Shown;
                }
            }
            State::Hiding(anim) => {
                if anim.is_clamped_done() {
                    self.state = State::Hidden;
                    // `_reset()` (`osdWindow.js:135-140`) clears the content.
                    self.content = OsdContent::default();
                    self.level_anim = None;
                    self.deadline = None;
                    self.revision += 1;
                }
            }
        }
        // GNOME's timeout is a plain GLib source: it fires on its own schedule, not
        // only once the fade-in has landed, so a still-Showing OSD expires too.
        if let Some(deadline) = self.deadline {
            if self.clock.now_unadjusted() >= deadline {
                self.cancel();
            }
        }
    }

    fn are_animations_ongoing(&self) -> bool {
        matches!(self.state, State::Showing(_) | State::Hiding(_)) || self.level_anim.is_some()
    }

    fn alpha(&self) -> f32 {
        match &self.state {
            State::Hidden => 0.,
            State::Showing(anim) | State::Hiding(anim) => anim.value().clamp(0., 1.) as f32,
            State::Shown => 1.,
        }
    }

    /// The pill's top-left on `output`: horizontally centered, `4em` above the
    /// monitor's bottom edge (`osdWindow.js:20-21`, `_osd.scss:14`).
    fn origin(&self, output: &Output, size: Size<f64, Logical>) -> Point<f64, Logical> {
        let out = output_size(output);
        Point::from((
            ((out.w - size.w) / 2.).max(0.),
            (out.h - MARGIN_BOTTOM_EM * em() - size.h).max(0.),
        ))
    }

    fn shape_label(
        &self,
        renderer: &mut VulkanRenderer,
        scale: f64,
    ) -> anyhow::Result<Option<ShapedText>> {
        let Some(text) = &self.content.label else {
            return Ok(None);
        };
        let mut shaper = widget::TextShaper::new(renderer, scale);
        Ok(Some(
            shaper.shape(text, widget::TextStyle::new(TEXT_PT).bold())?,
        ))
    }

    /// The pill, its hairline border and the label — everything whose look does not
    /// change while the level animates.
    fn paint_chrome(
        &self,
        frame: &mut VulkanFrame,
        phys: Size<i32, Physical>,
        scale: f64,
        layout: &OsdLayout,
        label: &Option<ShapedText>,
    ) -> anyhow::Result<()> {
        let mut p = Painter::new(frame, scale, phys);
        p.clear(style::TRANSPARENT)?;
        // `border-radius: $forced_circular_radius` — a pill, so half the height.
        let radius = layout.size.h / 2.;
        p.fill_rounded_full(radius, OSD_BG)?;
        p.stroke_rounded_full(radius, BORDER, OSD_BORDER)?;
        if let (Some(rect), Some(run)) = (layout.label, label) {
            p.text(
                run,
                Point::from((rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.)),
                Align::CENTER,
                OSD_FG,
            )?;
        }
        Ok(())
    }

    fn element(
        renderer: &mut VulkanRenderer,
        texture: VkTexture,
        scale: f64,
        loc: Point<f64, Logical>,
        alpha: f32,
    ) -> TextureRenderElement<VkTexture> {
        let buffer =
            TextureBuffer::from_texture(renderer, texture, scale, Transform::Normal, Vec::new());
        TextureRenderElement::from_texture_buffer(buffer, loc, alpha, None, None, Kind::Unspecified)
    }

    /// Front-to-back, like every other UI `render`.
    fn render(
        &self,
        renderer: &mut VulkanRenderer,
        icons: &IconCache,
        output: &Output,
    ) -> Vec<TextureRenderElement<VkTexture>> {
        if !self.is_visible() || self.content.icon.is_empty() {
            return Vec::new();
        }
        let scale = output.current_scale().fractional_scale();
        let layout = layout(&self.content);
        let alpha = self.alpha();
        let origin = self
            .origin(output, layout.size)
            .to_physical_precise_round(scale)
            .to_logical(scale);

        let mut elements = Vec::new();

        if let Some(el) = widget::icon_element_alpha(
            renderer,
            icons,
            &self.content.icon,
            ICON_PX,
            scale,
            OSD_FG,
            origin,
            layout.icon_center,
            alpha,
        ) {
            elements.push(el);
        }

        if let Some(rect) = layout.level {
            let value = self.displayed_level();
            let max = self.content.max_level.max(1.);
            // Key the bake on the physical pixel the bar actually reaches, so the
            // 100 ms ease re-bakes once per moved pixel instead of once per frame
            // (and a *static* OSD never re-bakes at all).
            let key = widget::Revision::new()
                .of((rect.size.w * value / max * scale).round() as i64)
                .px(rect.size.w)
                .px(max)
                // The separator flips from a solid white tick to a gap showing the
                // track as the value crosses `overdrive_start` — a branch the rounded
                // end-cap pixel above cannot see, so two values that straddle it and
                // land on the same pixel would serve each other's texture.
                .of(value > 1.)
                .done();
            let baked = widget::bake(
                renderer,
                &mut self.level_cache.borrow_mut(),
                scale,
                rect.size,
                key,
                |_| Ok(()),
                |frame, phys, _: &()| {
                    let mut p = Painter::new(frame, scale, phys);
                    p.clear(style::TRANSPARENT)?;
                    p.bar_level(
                        Rectangle::from_size(rect.size),
                        value,
                        max,
                        // Nothing sets `overdrive-start`, so it stays at its default
                        // 1.0 (`barLevel.js:24`) — the overdrive segment appears
                        // exactly when `max_level > 1`.
                        1.,
                        LEVEL_STYLE,
                    )
                },
            );
            match baked {
                Ok(texture) => elements.push(Self::element(
                    renderer,
                    texture,
                    scale,
                    origin + rect.loc,
                    alpha,
                )),
                Err(err) => tracing::error!("error drawing the OSD level bar: {err:#}"),
            }
        }

        match widget::bake(
            renderer,
            &mut self.chrome_cache.borrow_mut(),
            scale,
            layout.size,
            self.revision,
            |renderer| self.shape_label(renderer, scale),
            |frame, phys, label| self.paint_chrome(frame, phys, scale, &layout, label),
        ) {
            Ok(texture) => elements.push(Self::element(renderer, texture, scale, origin, alpha)),
            Err(err) => {
                tracing::error!("error drawing the OSD: {err:#}");
                // Without its background the icon and bar would float on the desktop.
                elements.clear();
            }
        }

        elements
    }
}

/// The per-output level for one [`OsdManager::show`] — GNOME's `levels` dict
/// (`osdWindow.js:172-182`).
#[derive(Debug, Clone, Copy)]
pub struct OsdLevel {
    pub level: Option<f64>,
    pub max_level: f64,
}

impl OsdLevel {
    pub fn new(level: f64, max_level: f64) -> Self {
        Self {
            level: Some(level),
            max_level: max_level.max(1.),
        }
    }

    /// An entry with no bar — the output still shows the icon/label.
    pub fn none() -> Self {
        Self {
            level: None,
            max_level: 1.,
        }
    }
}

/// `OsdWindowManager` (`js/ui/osdWindow.js:143`): one window per output.
pub struct OsdManager {
    windows: Vec<(Output, OsdWindow)>,
    clock: Clock,
    config: Rc<RefCell<Config>>,
}

impl OsdManager {
    pub fn new(clock: Clock, config: Rc<RefCell<Config>>) -> Self {
        Self {
            windows: Vec::new(),
            clock,
            config,
        }
    }

    /// `_monitorsChanged` (`osdWindow.js:151-163`), split into the two events we get.
    pub fn add_output(&mut self, output: &Output) {
        if self.windows.iter().any(|(o, _)| o == output) {
            return;
        }
        self.windows.push((
            output.clone(),
            OsdWindow::new(self.clock.clone(), self.config.clone()),
        ));
    }

    pub fn remove_output(&mut self, output: &Output) {
        self.windows.retain(|(o, _)| o != output);
    }

    /// `show(icon, label, levels)` (`osdWindow.js:172-182`): outputs listed in
    /// `levels` show the OSD, and **every other output's OSD is cancelled**.
    pub fn show(&mut self, icon: &[&str], label: Option<&str>, levels: &[(Output, OsdLevel)]) {
        for (output, window) in &mut self.windows {
            match levels.iter().find(|(o, _)| o == output) {
                Some((_, lv)) => {
                    let mut content = OsdContent::icon(icon);
                    content.label = label.map(str::to_owned);
                    content.level = lv.level;
                    content.max_level = lv.max_level.max(1.);
                    window.show(content);
                }
                None => window.cancel(),
            }
        }
    }

    /// `showOne` (`osdWindow.js:184-188`) — note this cancels the other outputs.
    pub fn show_one(&mut self, output: &Output, icon: &[&str], label: Option<&str>, lv: OsdLevel) {
        self.show(icon, label, &[(output.clone(), lv)]);
    }

    /// `showAll` (`osdWindow.js:190-193`).
    pub fn show_all(&mut self, icon: &[&str], label: Option<&str>, lv: OsdLevel) {
        let levels: Vec<_> = self.windows.iter().map(|(o, _)| (o.clone(), lv)).collect();
        self.show(icon, label, &levels);
    }

    /// `hideAll` (`osdWindow.js:195-198`) — the alt-tab switcher's opening move
    /// (`switcherPopup.js:178`).
    pub fn hide_all(&mut self) {
        for (_, window) in &mut self.windows {
            window.cancel();
        }
    }

    pub fn is_visible(&self) -> bool {
        self.windows.iter().any(|(_, w)| w.is_visible())
    }

    /// The earliest armed hide deadline across outputs — the calloop timer's target,
    /// needed because a static OSD over a damage-free desktop produces no frames to
    /// expire it on.
    pub fn next_wakeup(&self) -> Option<Duration> {
        self.windows
            .iter()
            .filter_map(|(_, w)| w.next_wakeup())
            .min()
    }

    pub fn advance_animations(&mut self) {
        for (_, window) in &mut self.windows {
            window.advance_animations();
        }
    }

    pub fn are_animations_ongoing(&self) -> bool {
        self.windows.iter().any(|(_, w)| w.are_animations_ongoing())
    }

    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        icons: &IconCache,
        output: &Output,
    ) -> Vec<TextureRenderElement<VkTexture>> {
        match self.windows.iter().find(|(o, _)| o == output) {
            Some((_, window)) => window.render(renderer, icons, output),
            None => Vec::new(),
        }
    }

    #[cfg(test)]
    pub fn content(&self, output: &Output) -> Option<OsdContent> {
        let (_, w) = self.windows.iter().find(|(o, _)| o == output)?;
        w.is_visible().then(|| w.content.clone())
    }

    #[cfg(test)]
    pub fn displayed_level(&self, output: &Output) -> Option<f64> {
        let (_, w) = self.windows.iter().find(|(o, _)| o == output)?;
        w.content.level.map(|_| w.displayed_level())
    }

    #[cfg(test)]
    pub fn alpha(&self, output: &Output) -> f32 {
        match self.windows.iter().find(|(o, _)| o == output) {
            Some((_, w)) => w.alpha(),
            None => 0.,
        }
    }

    /// The pill's rect on `output`, for render tests.
    #[cfg(test)]
    pub fn rect(&self, output: &Output) -> Option<Rectangle<f64, Logical>> {
        let (_, w) = self.windows.iter().find(|(o, _)| o == output)?;
        if !w.is_visible() {
            return None;
        }
        let l = layout(&w.content);
        Some(Rectangle::new(w.origin(output, l.size), l.size))
    }

    /// The level bar's rect on `output` — output-absolute, so a render test can
    /// count bar pixels without also catching the (white) icon and label.
    #[cfg(test)]
    pub fn level_rect(&self, output: &Output) -> Option<Rectangle<f64, Logical>> {
        let (_, w) = self.windows.iter().find(|(o, _)| o == output)?;
        if !w.is_visible() {
            return None;
        }
        let l = layout(&w.content);
        let rect = l.level?;
        Some(Rectangle::new(
            w.origin(output, l.size) + rect.loc,
            rect.size,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The OSD's label band is a caption **line box**, not a ceil of the font size.
    ///
    /// It was `px.ceil()` — 15 at the base 11pt, where the box is 19 — so a labelled OSD was 4px
    /// short and its text sat off-centre in the band. That was the fourth private spelling of a
    /// rule that now lives in [`crate::ui::line_height_px`]; this pins the OSD to the shared one
    /// so a fifth cannot appear here.
    #[test]
    fn a_labelled_osd_reserves_a_full_line_box() {
        let line = crate::ui::line_height_px(TEXT_PT);
        assert!(
            line > crate::ui::pt_to_px(TEXT_PT).ceil(),
            "the line box must exceed a bare ceil of the em, or this test proves nothing"
        );

        let label_only =
            layout(&OsdContent::icon(&["audio-volume-high-symbolic"]).with_label("50%"));
        let band = label_only
            .label
            .expect("a labelled OSD lays out a label band");
        assert_eq!(band.size.h, line, "the label band is one caption line box");

        // Not asserted: that the OSD box itself grows by the band. It does not — `content_h` is
        // `ICON_PX.max(column_h)` and the icon dominates a one-line column, so the band's height
        // only reaches the box once a level bar is stacked under it.
    }

    /// The full volume OSD, box for box against a mapped dump of the 50.3 shell.
    ///
    /// Measured with `DumpOsd` at the default 11pt Adwaita Sans: `.osd-window` is **248x63**,
    /// holding a 32px icon and a 166-wide column whose label band is 19 and whose level bar is
    /// 160x6, the two 8 apart. The whole pill sits 59 above the monitor's bottom edge.
    ///
    /// The interesting part is where the 160 comes from: the label is *not* 160 wide of text —
    /// "UI Dump" is a fraction of that. A vertical `StBoxLayout` allocates FILL across, so the
    /// level's `min-width: 160` sets the column and the label is stretched to match. A model that
    /// sized the column from the label alone would agree on every OSD wide enough to hide it.
    #[test]
    #[cfg_attr(
        not(feature = "reference-env"),
        ignore = "measures shaped text, so it needs the reference font stack; \
run it with --features reference-env, as the fedora CI job does"
    )]
    fn the_osd_matches_the_live_shell() {
        let l = layout(
            &OsdContent::icon(&["audio-volume-high-symbolic"])
                .with_label("UI Dump")
                .with_level(0.5, 1.),
        );

        assert_eq!((l.size.w, l.size.h), (248., 63.), "the .osd-window box");

        // Positions are relative to the pill's own origin; the live abs values are the dump's
        // minus `.osd-window`'s [1156, 1318].
        let label = l.label.expect("a labelled OSD has a label band");
        let level = l.level.expect("a levelled OSD has a bar");
        assert_eq!((label.loc.x, label.loc.y), (63., 13.), "label origin");
        assert_eq!((label.size.w, label.size.h), (160., 19.), "label band");
        assert_eq!((level.loc.x, level.loc.y), (63., 40.), "level origin");
        assert_eq!((level.size.w, level.size.h), (160., 6.), "level bar");
        // The icon is `y_expand`, so it centres in the 37-tall row rather than its own 32.
        assert_eq!(
            (l.icon_center.x, l.icon_center.y),
            (35., 31.5),
            "icon centre"
        );
    }
}
