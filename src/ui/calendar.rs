// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The dateMenu calendar grid.
//!
//! A fork-owned port of gnome-shell's `js/ui/calendar.js` month view: a
//! `‹ Month ›` header (month name alone in the current year, `‹ Month Year ›`
//! otherwise) with prev/next-month pagers, a single-letter weekday-heading row
//! rotated by the locale/gsettings week-start, and a 6×7 day grid with the
//! current day highlighted and out-of-month days dimmed. An optional ISO
//! week-number column shows when `org.gnome.desktop.calendar show-weekdate` is
//! set. Date math is done with libc (no date crate); all text is drawn through
//! the owned Vulkan glyph atlas, like the rest of the panel.
//!
//! The Events section below the grid IS ported (`EventsSectionModel` +
//! [`DateMenu::set_events`], fed by `src/dbus/calendar_server.rs`). Divergences
//! recorded there: the `datemenu-displays-section` ScrollView is deferred — when
//! the column would overflow the popover the bottom event rows clip instead of
//! scrolling (trigger to un-defer: when WorldClocks/Weather land), and because the
//! clip is a hard cut against `events_alloc_h`, an overflowing card loses its
//! rounded bottom corners until the ScrollView lands; the section is
//! non-reactive (GNOME's `calendarApp === null` state — we don't resolve the
//! `text/calendar` default app yet, so clicking launches nothing); the "No
//! Events" placeholder is upright, not italic (no italic face in the shaper);
//! `_formatEventTime`'s RTL swaps are the repo-wide deferred-RTL divergence; and
//! the title/events follow the *selected* day, so paging the month keeps the old
//! selection (and its title) rather than jumping to the new month — the same
//! paging-keeps-selection divergence the grid already carries, so an event list
//! can outlive a visible month change until a day in the new month is clicked.
//!
//! The World Clocks section below the Events card IS ported (`set_world_clocks`,
//! fed by `src/world_clocks.rs`): a `%card` with the same clip-not-scroll and
//! clipped-bottom-corner divergences as the events card. Its own divergences (in
//! `world_clocks.rs`): the timezone is resolved from the serialized coordinates
//! (tzf-rs), not GWeather's DB; city labels are the English serialized name;
//! clicking anywhere on the card launches GNOME Clocks via `gtk-launch` (GNOME
//! activates the app object). Shown iff `org.gnome.clocks.desktop` is installed —
//! sampled once at startup, so installing/removing Clocks mid-session needs a relog
//! (GNOME re-syncs on `installed-changed`). Long city names hard-clip rather than
//! ellipsize (GNOME's city label shows "…").
//!
//! Deferred vs gnome-shell: weather, calendar day-has-events dots, and keyboard
//! grid navigation. Those hang off daemons/D-Bus (GWeather) or are follow-ups.
//!
//! The popover content itself is [`DateMenu`]: gnome-shell's dateMenu is a
//! two-column hbox with the notification message list as the FIRST (left in
//! LTR) column and the calendar column second (`js/ui/dateMenu.js:917-940`).
//! The list ([`CalendarMessageList`], gnome-shell's `CalendarMessageList` +
//! `MessageView`, `js/ui/calendar.js:794-879`) renders the shared notification
//! cards flat (grouped stacks are a later slice), newest-first, with a
//! placeholder when empty and a Clear pill when not.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::CStr;
use std::ptr::null_mut;

use ordered_float::NotNan;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::{ContextId, Renderer};
use smithay::utils::{Logical, Physical, Point, Rectangle, Scale, Size, Transform};

use crate::calendar_events::CalendarEventStore;
use crate::image_source::ImageSource;
use crate::notifications::SourceKey;
use crate::render_helpers::icon::{AppIconCache, IconCache, ImageCache};
use crate::render_helpers::renderer::OffscreenRenderer;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::render_helpers::{render_to_texture, NATIVE_FOURCC};
use crate::ui::media_card;
use crate::ui::notification_card::{self, CardCache, CardContent, CardGroup, CardLayout};
use crate::ui::popover::PopoverAction;
use crate::ui::theme_node::{Edges, ThemeNode};
use crate::ui::widget::{self, Align, Painter, ShapedText, TextShaper, TextStyle};
use crate::utils::to_physical_precise_round;

// Geometry, logical px. Derived from `_calendar.scss` and pinned against a mapped actor dump of
// a live GNOME 50.3 date menu at the default font — see `calendar_matches_the_live_shell`.
//
// What this column *is*, in GNOME: two sibling cards inside `.datemenu-calendar-column`, each
// with its own 1px border and `$base_margin`, separated by the column's `spacing`. We bake both
// into one texture, so every offset below has to reproduce that two-box structure by hand.

/// `%card`/`%card_flat` margin (`$base_margin`, `_common.scss:141`) — column edge to card box.
const CARD_MARGIN: f64 = 4.;
/// `.datemenu-calendar-column { spacing: $base_padding }` (`_calendar.scss:14`).
const COLUMN_SPACING: f64 = 6.;
/// The `%card` border: 1px of `$card_shadow_border_color`, which is **transparent** in the dark
/// theme. Never drawn, always reserved — St is border-box. See `display_card_node`.
const CARD_BORDER: f64 = 1.;

/// `.calendar-month-header`'s height, set by its `.pager-button { height: 2.6em }`
/// (`_calendar.scss:65-69`) — taller than the month label, so it wins. Rounded because St
/// allocates in whole logical px; 38 live.
fn header_h() -> f64 {
    crate::ui::em(2.6).round()
}

/// The weekday-initials band: a `.calendar-day-heading` line box plus its `padding: 3px 6px` and
/// `margin: 4px` (`_calendar.scss`). 15 + 6 + 8 = 29 live.
fn weekday_h() -> f64 {
    crate::ui::line_height_px(WEEKDAY_PT) + 2. * DAY_HEADING_PAD_V + 2. * DAY_HEADING_MARGIN
}
const DAY_HEADING_PAD_V: f64 = 3.;
const DAY_HEADING_MARGIN: f64 = 4.;

/// Day-cell pitch: `.calendar-day` is `3em` **of its own `%smaller` 9pt font**
/// (`_calendar.scss:75`, `_common.scss:281-284`), plus its `margin: 2px`. 36 + 4 = 40 live.
///
/// This was `em(3.0)` against the *base* 11pt font, giving 44 — the same "right rule, wrong
/// specificity" trap as `.quick-settings .icon-button`. An `em` is only meaningful with the font it
/// is resolved against, and a day cell is not set in the base font.
fn cell() -> f64 {
    3. * crate::ui::pt_to_px(WEEKDAY_PT) + 2. * DAY_MARGIN
}
const DAY_MARGIN: f64 = 2.;

/// Week-number column width (only shown when week numbers are enabled): the
/// `.calendar-week-number` label plus its `padding: 0 6px` and `margin: 6px`. Measured 39 live
/// (27 + 12); the label itself is content-derived from a two-digit week at 9pt.
const WEEKCOL_W: f64 = 39.;
const GRID_ROWS: usize = 6;
const GRID_COLS: usize = 7;

// Month label is GNOME's `.calendar-month-label` (%heading, 11pt); the weekday headings
// and day-number cells are 9pt (`_calendar.scss`); shaping routes these through [`TextShaper`].
// The month-nav chevron is a drawn glyph sized in logical px, not a GNOME point size, so
// `ARROW_PT` is the point size whose `pt_to_px` reproduces its historical 18 logical px.
const HEADER_PT: f64 = 11.;
const WEEKDAY_PT: f64 = 9.;
const DAY_PT: f64 = 9.;
const ARROW_PT: f64 = 13.5; // pt_to_px(13.5) == 18 logical px
/// Diameter (logical px) of the today/selected highlight circle, drawn behind the
/// day number with `render_rounded_rect` (a half-diameter radius clamps to a full
/// circle in `sdf_rect.frag`). gnome-shell 50.1 makes both today and the selected
/// day circular filled buttons (`.calendar-day { border-radius:
/// $forced_circular_radius }`; today `%default_button`, selected `%flat_button`).
/// Measured 39px logical against a real 50.1 popover (≈0.89 of the `3em` cell); ours was 30.
const DISC_DIAM: f64 = 39.;

/// Fully transparent — the buffer is cleared to this so the rounded outer corners stay see-through.
const TRANSPARENT: [f32; 4] = [0., 0., 0., 0.];
/// The popover's outer corner radius. **The date menu overrides the standard popup radius**:
/// `.popup-menu-content` is `$modal_radius * 1.25` = 20px (`_popovers.scss:30`), but
/// `.datemenu-popover` sets `$base_border_radius * 1.5 + $base_padding * 3` = 12 + 18 = 30px
/// (`_calendar.scss:8-10`), which is what actually applies here.
///
/// We used 20 — the base rule, missing the override — until the live shell was asked:
/// `DumpName calendarArea` reports `border_radius: [30, 30, 30, 30]` on the
/// `popup-menu-content datemenu-popover` ancestor (`tools/gnome-ui-dump`). Unlike the paddings
/// around it, this one is plain px, so it does not move with the font size.
pub const BOX_RADIUS: f64 = 30.;
const TEXT: [f32; 4] = [1., 1., 1., 1.];
/// The selected (non-today) day's subtle filled circle — gnome-shell's flat-button
/// selected state (a faint light fill), vs today's accent fill.
const SELECTED_BG: [f32; 4] = [0.28, 0.28, 0.28, 1.];
/// Out-of-month day numbers, dimmed.
const DIM: [f32; 4] = [0.5, 0.5, 0.5, 1.];
/// In-month weekend day number — gnome-shell's `.calendar-weekend { color: $insensitive_fg_color }`
/// (`_calendar.scss:87`), `$insensitive_fg_color` (dark) = `mix($fg_color, $bg_color, 50%)` ≈
/// `#9a9a9c`. Dimmer than a workday (white) so the work days pop.
const WEEKEND_FG: [f32; 4] = [0.606, 0.606, 0.614, 1.];
/// Weekday header + week numbers, muted.
const MUTED: [f32; 4] = [0.6, 0.6, 0.6, 1.];
/// Week-number pill (`.calendar-week-number`, `_calendar.scss:139-149`): a rounded box behind the
/// number, `border-radius: $base_border_radius * 0.5` = 4px, horizontal padding `$base_padding` =
/// 6px. GNOME's `background-color: transparentize($insensitive_fg_color, .8)` (#9a9a9c @ 20%)
/// composited over the popover bg resolves to ≈ the message-list card surface (`lighten($card_bg,
/// 5%)` ≈ #51515a); on-screen GNOME renders the bubble at that card tone, so we fill it opaquely
/// with the same card color rather than relying on the alpha coincidence. The number is [`MUTED`].
const WEEK_BOX_RADIUS: f64 = 4.;
const WEEK_BOX_BG: [f32; 4] = [
    0x51 as f32 / 255.,
    0x51 as f32 / 255.,
    0x5a as f32 / 255.,
    1.,
];
const WEEK_BOX_PAD_X: f64 = 6.;
const WEEK_BOX_PAD_Y: f64 = 3.;

// The "today" header card above the grid — gnome-shell's `TodayButton` (`js/ui/calendar.js`):
// a button showing the weekday name over the full date, tapping it snaps the selection back to
// today. GNOME stacks two labels (`.day-label` weekday over `.date-label` full date) inside a
// framed button. We draw one flat rounded card with the same two lines. Sizes are logical px.
const TODAY_PAD: f64 = 9.;
/// The two label bands, from the realized font rather than pinned px.
///
/// They were 18 and 26, and the card was still the right height, because those sum to the same
/// 44 as the shell's 19 and 25 — two errors in opposite directions cancelling in the total and
/// leaving the *split* between the lines a pixel off. They also could not follow a font-size
/// change. See [`crate::ui::line_height_px`].
fn day_row() -> f64 {
    crate::ui::line_height_px(DAY_LABEL_PT)
}
fn date_row() -> f64 {
    crate::ui::line_height_px(DATE_LABEL_PT)
}
/// `.datemenu-today-button` is `%card_flat` with `padding: $base_padding * 1.5`
/// (`_calendar.scss:23-25`) — so it reserves the same transparent 1px border every card does.
fn today_card_node() -> ThemeNode {
    ThemeNode {
        padding: Edges::uniform(TODAY_PAD),
        border: Edges::uniform(CARD_BORDER),
        border_radius: TODAY_RADIUS,
        ..ThemeNode::EMPTY
    }
}

/// 64 live: 1 + 9 + 19 + 25 + 9 + 1.
fn today_card_h() -> f64 {
    today_card_node()
        .allocation_for(Size::from((0., day_row() + date_row())))
        .h
}
const TODAY_RADIUS: f64 = 12.;
/// `.day-label` (weekday name) and `.date-label` (full date) point sizes. GNOME's date label is
/// heavier (800) but the rasterizer tops out at bold (700); both draw bold here.
const DAY_LABEL_PT: f64 = 11.;
const DATE_LABEL_PT: f64 = 15.;

/// Hover highlight: an additive fg-wash painted over an element's existing
/// background (behind its glyphs), and a faint standalone fill for flat
/// elements that have no base bg. GNOME raises a button's fg-wash by ~0.10 on
/// `:hover` (`_message-list.scss:72-75`; `_calendar.scss` flat day buttons use
/// a similar faint `transparentize($fg_color,…)`). Subtle by design; tune live.
const HOVER_WASH: [f32; 4] = [1., 1., 1., 0.10];

/// A calendar month view. Displayed month + the selected day; `today` is fixed
/// at construction. `week_start` is 0=Sunday..6=Saturday.
pub struct Calendar {
    /// Displayed month.
    year: i32,
    month: u32,
    /// The selected day (year, month, day).
    selected: Ymd,
    /// Today, for the accent highlight.
    today: Ymd,
    week_start: u8,
    show_week_numbers: bool,
    /// Accent color for the today disc (straight RGB from gsettings).
    accent: [f32; 4],
    /// The region the pointer is hovering, highlighted in `draw`.
    hovered: Option<CalHover>,
    /// Bumped on any content change to invalidate the rendered texture.
    revision: u64,
    cache: RefCell<TextureCache>,
}

/// A hoverable region of the calendar (the today card, a month-nav arrow, or a
/// grid cell by its 0-based index), for the hover highlight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CalHover {
    Today,
    Prev,
    Next,
    Cell(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ymd {
    pub year: i32,
    pub month: u32,
    pub day: u32,
}

struct TextureCache {
    context: Option<ContextId<VkTexture>>,
    textures: HashMap<NotNan<f64>, (u64, VkTexture)>,
}

impl Calendar {
    /// Open on today's month with today selected. `week_start` 0=Sun..6=Sat,
    /// `accent` straight RGB (e.g. `gnome_settings.accent_color`).
    pub fn new(week_start: u8, show_week_numbers: bool, accent: [u8; 3]) -> Self {
        let today = today();
        Self {
            year: today.year,
            month: today.month,
            selected: today,
            today,
            week_start: week_start.min(6),
            show_week_numbers,
            accent: [
                f32::from(accent[0]) / 255.,
                f32::from(accent[1]) / 255.,
                f32::from(accent[2]) / 255.,
                1.,
            ],
            hovered: None,
            revision: 0,
            cache: RefCell::new(TextureCache {
                context: None,
                textures: HashMap::new(),
            }),
        }
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn selected(&self) -> Ymd {
        self.selected
    }

    /// The calendar column's logical size (depends only on whether the week column shows).
    pub fn logical_size(&self) -> Size<f64, Logical> {
        Layout::new(self.show_week_numbers).bounds().size
    }

    /// Step the displayed month by `delta` (keeping the selection where it is).
    fn shift_month(&mut self, delta: i32) {
        let (y, m) = add_months(self.year, self.month, delta);
        self.year = y;
        self.month = m;
        self.revision += 1;
    }

    /// A wheel/scroll over the grid: up (`delta < 0`) → previous month, down →
    /// next (gnome-shell `Calendar.vfunc_scroll_event`, `js/ui/calendar.js:560-571`).
    pub fn scroll(&mut self, delta: f64) -> bool {
        if delta == 0. {
            return false;
        }
        self.shift_month(if delta < 0. { -1 } else { 1 });
        true
    }

    /// Select a grid date, following an out-of-month click into that month.
    fn select(&mut self, date: Ymd) {
        self.selected = date;
        if date.year != self.year || date.month != self.month {
            self.year = date.year;
            self.month = date.month;
        }
        self.revision += 1;
    }

    /// Handle a click at a calendar-local logical position. Returns whether it
    /// hit something (so the caller keeps the popover open on any interior click).
    pub fn pointer_click(&mut self, pos: Point<f64, Logical>) -> bool {
        let layout = Layout::new(self.show_week_numbers);
        // The today card: snap the selection back to today (matching gnome-shell's TodayButton).
        // Unlike gnome-shell, paging keeps the selection put (`shift_month`), so `selected` can
        // still be today while a different month is displayed — return-to-today must also snap the
        // *view* back, so we act whenever the selection OR the displayed month is off today.
        if layout.today_button().contains(pos) {
            let showing_today = self.year == self.today.year && self.month == self.today.month;
            if self.selected != self.today || !showing_today {
                self.select(self.today);
            }
            return true;
        }
        if layout.prev_arrow().contains(pos) {
            self.shift_month(-1);
            return true;
        }
        if layout.next_arrow().contains(pos) {
            self.shift_month(1);
            return true;
        }
        let grid = self.grid();
        for (i, cell) in grid.iter().enumerate() {
            if layout.cell(i / GRID_COLS, i % GRID_COLS).contains(pos) {
                self.select(*cell);
                return true;
            }
        }
        // A click inside the box background is still "handled" (keeps it open).
        layout.bounds().contains(pos)
    }

    /// Update the hovered region from a calendar-local position (`None` clears
    /// it, e.g. the pointer left the calendar). Returns whether the hover
    /// changed, so the caller can redraw.
    pub fn hover(&mut self, pos: Option<Point<f64, Logical>>) -> bool {
        let new = pos.and_then(|pos| self.hover_zone(pos));
        if new == self.hovered {
            return false;
        }
        self.hovered = new;
        self.revision += 1;
        true
    }

    fn hover_zone(&self, pos: Point<f64, Logical>) -> Option<CalHover> {
        let layout = Layout::new(self.show_week_numbers);
        if layout.today_button().contains(pos) {
            return Some(CalHover::Today);
        }
        if layout.prev_arrow().contains(pos) {
            return Some(CalHover::Prev);
        }
        if layout.next_arrow().contains(pos) {
            return Some(CalHover::Next);
        }
        for i in 0..GRID_ROWS * GRID_COLS {
            if layout.cell(i / GRID_COLS, i % GRID_COLS).contains(pos) {
                return Some(CalHover::Cell(i));
            }
        }
        None
    }

    /// The 42 dates filling the 6×7 grid, row-major from the week-start column of
    /// the row containing the 1st.
    fn grid(&self) -> [Ymd; GRID_ROWS * GRID_COLS] {
        month_grid(self.year, self.month, self.week_start)
    }

    /// The Unix-second `[since, until)` range covering the whole 42-cell grid —
    /// what the CalendarServer is asked to load (`js/ui/calendar.js:748`).
    /// `getEvents` then filters per selected day client-side.
    pub fn grid_range(&self) -> (i64, i64) {
        grid_range_of(self.year, self.month, self.week_start)
    }

    /// Local-midnight `[since, until)` bounds of the currently-selected day.
    pub fn selected_day_bounds(&self) -> (i64, i64) {
        local_day_bounds(self.selected)
    }
}

/// The 42 dates of a month's 6×7 grid, row-major from the week-start column of
/// the row containing the 1st. Free function so range math can reuse it without
/// a live [`Calendar`].
///
/// Divergence from gnome-shell: when the 1st falls exactly on the week-start
/// column, GNOME still pads a full leading week from the previous month to keep
/// the month in weeks 2–6 (`_rebuildCalendar`'s always-6-weeks policy,
/// `js/ui/calendar.js:645-666`); we start at the 1st and spill further into the
/// next month instead. Pre-existing (this only moved the logic); affects the
/// top/bottom row and the C6 grid-range edges — revisit as its own grid fix.
fn month_grid(year: i32, month: u32, week_start: u8) -> [Ymd; GRID_ROWS * GRID_COLS] {
    let first = Ymd {
        year,
        month,
        day: 1,
    };
    let col_of_first = (7 + weekday(first) as i32 - week_start as i32) % 7;
    let start = add_days(first, -(col_of_first as i64));
    let mut out = [first; GRID_ROWS * GRID_COLS];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = add_days(start, i as i64);
    }
    out
}

/// The Unix-second `[since, until)` grid range of an arbitrary month (used at
/// startup / while the popover is closed, when there is no live [`Calendar`]).
pub fn grid_range_of(year: i32, month: u32, week_start: u8) -> (i64, i64) {
    let cells = month_grid(year, month, week_start);
    let first = cells[0];
    let last = cells[GRID_ROWS * GRID_COLS - 1];
    (local_midnight(first), local_midnight(add_days(last, 1)))
}

/// Today's month, for priming the range before the popover first opens.
pub fn today_grid_range(week_start: u8) -> (i64, i64) {
    let t = today();
    grid_range_of(t.year, t.month, week_start)
}

/// Local-midnight `[00:00 today, 00:00 tomorrow)` of a date, Unix seconds — the
/// per-day interval `getEvents`/`EventsSection.setDate` filters on
/// (`js/ui/dateMenu.js:150-154`).
pub fn local_day_bounds(date: Ymd) -> (i64, i64) {
    (local_midnight(date), local_midnight(add_days(date, 1)))
}

/// A date's local midnight as Unix seconds. `mktime` interprets the broken-down
/// time in the local zone (DST-normalized via `tm_isdst = -1`), matching JS
/// `new Date(y, m, d)`.
fn local_midnight(date: Ymd) -> i64 {
    // SAFETY: a zeroed `tm` is valid; we set the fields mktime needs.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = date.year - 1900;
    tm.tm_mon = date.month as i32 - 1;
    tm.tm_mday = date.day as i32;
    tm.tm_isdst = -1;
    // SAFETY: tm is populated; mktime normalizes and returns the epoch time.
    unsafe { libc::mktime(&mut tm) as i64 }
}

/// Logical layout of the calendar's hit/draw regions, relative to its top-left.
struct Layout {
    week: bool,
}

impl Layout {
    fn new(show_week_numbers: bool) -> Self {
        Self {
            week: show_week_numbers,
        }
    }

    /// The whole column: both card boxes plus their margins. 329x391 live.
    fn bounds(&self) -> Rectangle<f64, Logical> {
        let card = self.calendar_card();
        let w = card.size.w + 2. * CARD_MARGIN;
        let h = card.loc.y + card.size.h + CARD_MARGIN;
        Rectangle::new(Point::from((0., 0.)), Size::from((w, h)))
    }

    /// The `.datemenu-today-button` card box, inset from the column by its margin. 321x64 live.
    fn today_button(&self) -> Rectangle<f64, Logical> {
        Rectangle::new(
            Point::from((CARD_MARGIN, CARD_MARGIN)),
            Size::from((self.calendar_card().size.w, today_card_h())),
        )
    }

    /// The `.calendar` card box — the *bordered* box, not its content. 321x309 live.
    ///
    /// Its top is the today card's box plus that card's bottom margin plus the column spacing;
    /// `.calendar` itself has `margin-top: 0` (`_calendar.scss:40`), which is why the gap is 10
    /// and not 14.
    fn calendar_card(&self) -> Rectangle<f64, Logical> {
        let inner_w = self.weekcol_w() + GRID_COLS as f64 * cell();
        let inner_h = header_h() + weekday_h() + GRID_ROWS as f64 * cell();
        let top = CARD_MARGIN + today_card_h() + CARD_MARGIN + COLUMN_SPACING;
        Rectangle::new(
            Point::from((CARD_MARGIN, top)),
            Size::from((inner_w + 2. * CARD_BORDER, inner_h + 2. * CARD_BORDER)),
        )
    }

    /// The `.calendar` card's content box — inside the reserved border. `.calendar` has
    /// `padding: 0` (`_calendar.scss:41`), so the border is the whole inset.
    fn calendar_content(&self) -> Rectangle<f64, Logical> {
        calendar_card_node().content_box(self.calendar_card())
    }

    fn weekcol_w(&self) -> f64 {
        if self.week {
            WEEKCOL_W
        } else {
            0.
        }
    }

    fn prev_arrow(&self) -> Rectangle<f64, Logical> {
        let content = self.calendar_content();
        Rectangle::new(content.loc, Size::from((header_h(), header_h())))
    }

    fn next_arrow(&self) -> Rectangle<f64, Logical> {
        let content = self.calendar_content();
        let x = content.loc.x + content.size.w - header_h();
        Rectangle::new(
            Point::from((x, content.loc.y)),
            Size::from((header_h(), header_h())),
        )
    }

    fn cell(&self, row: usize, col: usize) -> Rectangle<f64, Logical> {
        let x = self.grid_left() + col as f64 * cell();
        let y = self.grid_top() + row as f64 * cell();
        Rectangle::new(Point::from((x, y)), Size::from((cell(), cell())))
    }

    /// Left edge of the day columns: inside the card's border, past the week column.
    fn grid_left(&self) -> f64 {
        self.calendar_content().loc.x + self.weekcol_w()
    }

    /// Top edge of the day rows: inside the card's border, below the header and weekday band.
    fn grid_top(&self) -> f64 {
        self.calendar_content().loc.y + header_h() + weekday_h()
    }
}

/// `.calendar` is `%card_flat` with `padding: 0` and `margin-top: 0` (`_calendar.scss:38-41`).
fn calendar_card_node() -> ThemeNode {
    ThemeNode {
        border: Edges::uniform(CARD_BORDER),
        border_radius: TODAY_RADIUS,
        ..ThemeNode::EMPTY
    }
}

impl Calendar {
    /// Draw the calendar into an offscreen [`VkTexture`], caching per (scale,
    /// revision). Returns the sampleable texture to composite.
    pub fn texture(&self, renderer: &mut VulkanRenderer, scale: f64) -> anyhow::Result<VkTexture> {
        let scale_key = NotNan::new(scale).map_err(|_| anyhow::anyhow!("bad scale"))?;
        let mut cache = self.cache.borrow_mut();
        let context = renderer.context_id();
        if cache.context.as_ref() != Some(&context) {
            cache.textures.clear();
            cache.context = Some(context);
        }
        let fresh =
            matches!(cache.textures.get(&scale_key), Some((rev, _)) if *rev == self.revision);
        if !fresh {
            let tex = self.draw(renderer, scale)?;
            cache.textures.insert(scale_key, (self.revision, tex));
        }
        Ok(cache
            .textures
            .get(&scale_key)
            .map(|(_, t)| t.clone())
            .unwrap())
    }

    fn draw(&self, renderer: &mut VulkanRenderer, scale: f64) -> anyhow::Result<VkTexture> {
        let _span = tracy_client::span!("calendar::draw");

        let size = self.logical_size();
        let box_w = to_physical_precise_round::<i32>(scale, size.w).max(1);
        let box_h = to_physical_precise_round::<i32>(scale, size.h).max(1);
        let phys = Size::<i32, Physical>::from((box_w, box_h));
        let layout = Layout::new(self.show_week_numbers);

        // Shape every run up front (needs `&mut renderer`, before the bake frame opens).
        // `TextShaper` owns the pt → physical-px multiply — no `* scale` on the font sizes.
        // Month header: month name alone while viewing the current year, month + year otherwise —
        // gnome-shell's `sameYear(selectedDate, now)` split between `%OB` and `%OB %Y`
        // (`js/ui/calendar.js:755-757`). (`%OB`, the standalone month form, matters for declined-
        // noun locales; for our en scope it is identical to `%B`.)
        let first = Ymd {
            year: self.year,
            month: self.month,
            day: 1,
        };
        let title = if self.year == self.today.year {
            strftime_ymd(first, c"%B")
        } else {
            strftime_ymd(first, c"%B %Y")
        };
        let day_label = strftime_ymd(self.today, c"%A");
        let date_label = strftime_ymd(self.today, c"%B %-d %Y");
        let grid = self.grid();
        let (
            title_run,
            prev_run,
            next_run,
            weekday_runs,
            day_runs,
            week_runs,
            day_label_run,
            date_label_run,
        ) = {
            let mut shaper = TextShaper::new(renderer, scale);
            let title_run = shaper.shape(&title, TextStyle::new(HEADER_PT))?;
            let prev_run = shaper.shape("\u{2039}", TextStyle::new(ARROW_PT))?; // ‹
            let next_run = shaper.shape("\u{203a}", TextStyle::new(ARROW_PT))?; // ›
            let weekday_runs: Vec<ShapedText> = (0..GRID_COLS)
                .map(|c| {
                    let w = (self.week_start as usize + c) % 7;
                    shaper.shape(&weekday_abbrev(w as u32), TextStyle::new(WEEKDAY_PT).bold())
                })
                .collect::<Result<_, _>>()?;
            // Current-month days are bold (`.calendar-day font-weight:bold`), other-month days
            // normal (`.calendar-other-month font-weight:normal`, `_calendar.scss:82,96`) — this is
            // what makes the in-month days "pop".
            let day_runs: Vec<ShapedText> = grid
                .iter()
                .map(|d| {
                    let style = TextStyle::new(DAY_PT);
                    let style = if d.month == self.month {
                        style.bold()
                    } else {
                        style
                    };
                    shaper.shape(&d.day.to_string(), style)
                })
                .collect::<Result<_, _>>()?;
            let week_runs: Vec<ShapedText> = if self.show_week_numbers {
                (0..GRID_ROWS)
                    .map(|r| {
                        let d = grid[r * GRID_COLS];
                        shaper.shape(&iso_week(d).to_string(), TextStyle::new(WEEKDAY_PT))
                    })
                    .collect::<Result<_, _>>()?
            } else {
                Vec::new()
            };
            // Today card labels: weekday name over the full date, both bold (GNOME's TodayButton).
            let day_label_run = shaper.shape(&day_label, TextStyle::new(DAY_LABEL_PT).bold())?;
            let date_label_run = shaper.shape(&date_label, TextStyle::new(DATE_LABEL_PT).bold())?;
            (
                title_run,
                prev_run,
                next_run,
                weekday_runs,
                day_runs,
                week_runs,
                day_label_run,
                date_label_run,
            )
        };

        widget::bake_uncached_sized(renderer, phys, |frame| {
            let mut p = Painter::new(frame, scale, phys);
            // Transparent bg: the shared popover chrome (`PanelPopover::render`) draws the
            // `.popup-menu-content` box fill behind this column, so it need not fill its own box.
            p.clear(TRANSPARENT)?;

            // Logical center of a rect (Painter places by logical coords + Align).
            let lc = |rect: Rectangle<f64, Logical>| {
                Point::<f64, Logical>::from((
                    rect.loc.x + rect.size.w / 2.,
                    rect.loc.y + rect.size.h / 2.,
                ))
            };
            // A centered disc of DISC_DIAM around a logical center point.
            let disc_at = |c: Point<f64, Logical>| {
                Rectangle::new(
                    Point::<f64, Logical>::from((c.x - DISC_DIAM / 2., c.y - DISC_DIAM / 2.)),
                    Size::<f64, Logical>::from((DISC_DIAM, DISC_DIAM)),
                )
            };

            // Today button: weekday name over the full date. gnome-shell's `.datemenu-today-button`
            // extends `%card_flat` = `button(undecorated, flat)` — NO resting background (flat on
            // the popover bg); only a hover wash (`_calendar.scss:23`, `_common.scss:163`).
            let card = layout.today_button();
            if self.hovered == Some(CalHover::Today) {
                p.fill_rounded(card, TODAY_RADIUS, HOVER_WASH)?;
            }
            // Inside the card's own border+padding, via `content_box` — so the labels follow the
            // reserved border rather than being placed at a hand-added inset.
            let today_content = today_card_node().content_box(card);
            let label_x = today_content.loc.x;
            p.text(
                &day_label_run,
                Point::from((label_x, today_content.loc.y + day_row() / 2.)),
                Align::LEFT_MIDDLE,
                MUTED,
            )?;
            p.text(
                &date_label_run,
                Point::from((label_x, today_content.loc.y + day_row() + date_row() / 2.)),
                Align::LEFT_MIDDLE,
                // GNOME's `.date-label` and `.day-label` share `$fg_color` — no per-label
                // override (`_calendar.scss:28,33`); measured equal (~#909091) in the 50.1
                // reference. Match the weekday tone, not pure white.
                MUTED,
            )?;

            // Header: ‹ arrows › and the centered "Month Year". A hovered arrow
            // gets a circular highlight behind its chevron (GNOME's pager
            // buttons are circular flat buttons).
            let prev_c = lc(layout.prev_arrow());
            if self.hovered == Some(CalHover::Prev) {
                p.fill_rounded(disc_at(prev_c), DISC_DIAM / 2., HOVER_WASH)?;
            }
            p.text(&prev_run, prev_c, Align::CENTER, MUTED)?;
            let next_c = lc(layout.next_arrow());
            if self.hovered == Some(CalHover::Next) {
                p.fill_rounded(disc_at(next_c), DISC_DIAM / 2., HOVER_WASH)?;
            }
            p.text(&next_run, next_c, Align::CENTER, MUTED)?;
            p.text(
                &title_run,
                Point::from((
                    size.w / 2.,
                    layout.calendar_content().loc.y + header_h() / 2.,
                )),
                Align::CENTER,
                TEXT,
            )?;

            // Weekday header row.
            let wd_cy = layout.calendar_content().loc.y + header_h() + weekday_h() / 2.;
            for (c, run) in weekday_runs.iter().enumerate() {
                let cx = layout.grid_left() + (c as f64 + 0.5) * cell();
                p.text(run, Point::from((cx, wd_cy)), Align::CENTER, MUTED)?;
            }

            // Week-number column: a rounded `.calendar-week-number` pill behind each number.
            for (r, run) in week_runs.iter().enumerate() {
                let cx = layout.calendar_content().loc.x + WEEKCOL_W / 2.;
                let cy = layout.grid_top() + (r as f64 + 0.5) * cell();
                // `ink_bounds` is physical px; back to logical for the Painter (× scale
                // internally).
                let (_ix, _iy, iw, ih) = run.ink_bounds();
                let (iw, ih) = (iw as f64 / scale, ih as f64 / scale);
                let box_w = iw + 2. * WEEK_BOX_PAD_X;
                let box_h = ih + 2. * WEEK_BOX_PAD_Y;
                let pill = Rectangle::new(
                    Point::<f64, Logical>::from((cx - box_w / 2., cy - box_h / 2.)),
                    Size::<f64, Logical>::from((box_w, box_h)),
                );
                p.fill_rounded(pill, WEEK_BOX_RADIUS, WEEK_BOX_BG)?;
                p.text(run, Point::from((cx, cy)), Align::CENTER, MUTED)?;
            }

            // Day grid.
            for (i, date) in grid.iter().enumerate() {
                let c = lc(layout.cell(i / GRID_COLS, i % GRID_COLS));
                let is_today = *date == self.today;
                let is_selected = *date == self.selected;
                // Today: accent-filled circle; selected (not today): a subtle filled circle —
                // matching gnome-shell's circular calendar-day buttons. The day number draws on
                // top. A half-diameter radius clamps to a full circle in `sdf_rect.frag`.
                let is_hovered = self.hovered == Some(CalHover::Cell(i));
                if is_today || is_selected || is_hovered {
                    let disc = disc_at(c);
                    // Today: accent fill; selected: subtle fill; plain hover: a
                    // faint standalone disc. Hovering a today/selected day adds
                    // the wash on top of its existing disc.
                    if is_today || is_selected {
                        let bg = if is_today { self.accent } else { SELECTED_BG };
                        p.fill_rounded(disc, DISC_DIAM / 2., bg)?;
                    }
                    if is_hovered {
                        p.fill_rounded(disc, DISC_DIAM / 2., HOVER_WASH)?;
                    }
                }
                // Color (gnome-shell `_calendar.scss:73-99`): today draws white on its accent
                // disc; other-month days are dimmed (`.calendar-other-month`); in-month WEEKEND
                // days are the muted `$insensitive_fg_color` (`.calendar-weekend`) so the workdays
                // (full white, bold) pop; in-month workdays are white.
                let in_month = date.month == self.month;
                let is_weekend = matches!(weekday(*date), 0 | 6);
                let color = if is_today {
                    TEXT
                } else if !in_month {
                    DIM
                } else if is_weekend {
                    WEEKEND_FG
                } else {
                    TEXT
                };
                p.text(&day_runs[i], c, Align::CENTER, color)?;
            }

            Ok(())
        })
    }
}

// ---- The dateMenu popover: message list column + calendar ----

/// 1em at the 11pt base font.
fn list_em() -> f64 {
    crate::ui::pt_to_px(11.)
}
/// `.message-list` **content** width: `width: $_message_list_width` = 29em
/// (`_message-list.scss:3,7`). St's `width` is a content width — `adjust_preferred_width` adds
/// border and padding on top of it — so the box on screen is wider; see [`list_box_w`].
/// Rounded because St allocates in whole logical px. 425 live.
fn list_w() -> f64 {
    (29. * list_em()).round()
}

/// The `.message-list` **box**: its content plus `padding-right: $base_padding` and
/// `border-right-width: 1px` (`_message-list.scss:11`). 432 live.
///
/// This is also where the visible hairline between the two columns falls — the only border in
/// the popover that is actually drawn.
fn list_box_w() -> f64 {
    list_w() + LIST_PAD_R + LIST_BORDER_R
}
const LIST_PAD_R: f64 = 6.;
const LIST_BORDER_R: f64 = 1.;

/// Between the two columns: `.message-list:ltr margin-right: $base_margin` (4) plus
/// `.datemenu-calendar-column:ltr margin-left: $base_padding` (6, `_calendar.scss:15`). 10 live.
fn column_gap() -> f64 {
    LIST_MARGIN_R + COLUMN_MARGIN_L
}
const COLUMN_MARGIN_L: f64 = 6.;
/// `.popup-menu-content` padding (`_popovers.scss:28`) — the uniform inset between
/// the popover box and its content. Applied on the left (before the list column),
/// the right (after the calendar column), and the bottom (below the calendar
/// column) in [`DateMenu::logical_size`]; the top comes from the grid's own inset.
///
/// Two containers stack here: `.popup-menu-content` `padding: $base_padding` (6px,
/// `_popovers.scss:28`) and `#calendarArea` `padding: $base_margin` (4px, `_calendar.scss:5`).
/// **Measured** on a real GNOME shot (`gnomes.png`): the first card's left edge sits 10px inside
/// the popover border, and its top 10-11px below — matching 6+4, not 6.
const LIST_PAD: f64 = 10.;

/// [`LIST_PAD`], for tests that place sample points relative to the list rather than to a literal.
#[cfg(test)]
pub(crate) fn list_pad() -> f64 {
    LIST_PAD
}
/// `.message-list:ltr` margin-right, separating the two columns
/// (`_message-list.scss:11`).
const LIST_MARGIN_R: f64 = 4.;
/// Scrollbar room right of the cards: `.message-view:ltr { margin-right: $base_margin * 3 }`
/// (`_message-list.scss:30-31`).
///
/// This used to also include the list's own `padding-right: $base_padding`, which double-charged
/// the cards by 6 — that padding is *outside* the 29em content width (see [`list_w`]), not inside
/// it. Cards are 413 live; we were making them 407.
const LIST_SCROLL_R: f64 = 12.;
/// Card width in the list column.
fn card_w() -> f64 {
    list_w() - LIST_SCROLL_R
}
/// `.message` `margin-bottom: $base_padding * 2` (`_message-list.scss:37`).
const CARD_GAP: f64 = 12.;
/// List-card radius: `$modal_radius + 2px` (`_message-list.scss:39`).
const CARD_RADIUS: f64 = 18.;
/// `.message-group-header` padding (`_message-list.scss:61`).
const GROUP_HEADER_PAD: f64 = 6.;
/// The group title `%title_2` (15pt/800; the rasterizer caps at bold, like the
/// today date label) (`_common.scss:251-254`, `_message-list.scss:62-65`).
const GROUP_TITLE_PT: f64 = 15.;
/// `.message-group-title` side margin (`$base_margin`, `_message-list.scss:64`).
const GROUP_TITLE_MARGIN: f64 = 4.;
/// The header's collapse button (`.message-collapse-button`, `group-collapse-symbolic`):
/// a small icon-button, white@20% bg (`_message-list.scss:69-77`).
const GROUP_COLLAPSE_D: f64 = 24.;
const GROUP_COLLAPSE_BG: [f32; 4] = [1., 1., 1., 0.2];
/// Header block height: padding + the button row + padding.
const GROUP_HEADER_H: f64 = GROUP_HEADER_PAD + GROUP_COLLAPSE_D + GROUP_HEADER_PAD;
/// `.message-list-controls` padding: 12px sides/top, 9px bottom
/// (`_message-list.scss:44-47`).
const CONTROLS_PAD: f64 = 12.;
const CONTROLS_PAD_B: f64 = 9.;
/// The Clear pill (`.message-list-clear-button button`, forced-circular
/// radius): `%heading` 11pt/700 label.
const CLEAR_H: f64 = 28.;
const CLEAR_PAD_X: f64 = 14.;
const CLEAR_PT: f64 = 11.;
fn clear_px() -> f64 {
    crate::ui::pt_to_px(CLEAR_PT)
}
/// `.button` flat fill on the dark theme (matches the card action pills).
const CLEAR_BG: [f32; 4] = [1., 1., 1., 0.1];
/// `.message-list-placeholder` (`_message-list.scss:14-26`): 96px icon over a
/// `%title_3` (15pt/700) label, both at 45% fg.
const PLACEHOLDER_ICON: f64 = 96.;
const PLACEHOLDER_GAP: f64 = 12.;
const PLACEHOLDER_PT: f64 = 15.;
fn placeholder_px() -> f64 {
    crate::ui::pt_to_px(PLACEHOLDER_PT)
}
const PLACEHOLDER_FG: [f32; 4] = [1., 1., 1., 0.45];
/// Overlay scrollbar handle: `StScrollBar` min-width 8px with a 3px transparent
/// border → a ~6px visible handle (`_scrollbars.scss:10-25`), a
/// `forced_circular` pill, `mix($fg,$bg,30%)`. Sits in the reserved right strip
/// (`LIST_SCROLL_R`), a few px from the column edge.
const SCROLLBAR_W: f64 = 6.;
const SCROLLBAR_MIN_H: f64 = 24.;
const SCROLLBAR_EDGE_GAP: f64 = 4.;
const SCROLLBAR_THUMB: [f32; 4] = [0.45, 0.45, 0.47, 1.];

/// A visible media card's `(bus name, card rect, control rects)`, popover-local (test hook).
pub type MediaCardRects = (
    String,
    Rectangle<f64, Logical>,
    [Rectangle<f64, Logical>; 3],
);

/// A visible card's `(id, card rect, close-button rect)`, popover-local
/// (introspection/test hook).
pub type CardRects = (u32, Rectangle<f64, Logical>, Rectangle<f64, Logical>);

/// What a click inside the message-list column hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListHit {
    /// A card's close button: close that notification, reason Dismissed.
    Close(u32),
    /// A card's expand caret: toggle its body/action-row expansion — pure UI
    /// state, no store mutation (`js/ui/messageList.js:521-526`).
    ToggleExpand(u32),
    /// An action button on an expanded card: emit
    /// ActivationToken+ActionInvoked and destroy unless resident
    /// (`js/ui/notificationDaemon.js:224-227`, `js/ui/messageTray.js:430-442`).
    Action { id: u32, key: String },
    /// A card body: activate the notification (same semantics as a banner
    /// body click — the list card is a click-through to
    /// `notification.activate()`, `js/ui/messageList.js:730-732`).
    Body { id: u32, has_default: bool },
    /// A click anywhere on a collapsed multi-notification stack: expand the
    /// group into a vertical list (`js/ui/messageList.js:1113-1118`).
    ExpandGroup(SourceKey),
    /// The expanded group's header collapse button: fan it back into a stack
    /// (`js/ui/messageList.js:934,1809-1814`).
    CollapseGroup,
    /// The close button of a COLLAPSED group's top card: closes the WHOLE group
    /// (`js/ui/messageList.js:1106-1112`, `close()` :1236-1242).
    CloseGroup(Vec<u32>),
    /// The Clear pill: close everything.
    Clear,
    /// A media card's transport button (`js/ui/messageList.js:778-791`).
    MediaControl {
        bus_name: String,
        control: media_card::MediaControl,
    },
    /// A media card's body: raise the player and close the popover
    /// (`js/ui/messageList.js:799-804`).
    MediaBody(String),
}

/// A hoverable region of the message list, for the hover highlight. A card
/// (`.message` = `%card`) darkens its body whenever it is hovered anywhere;
/// `zone` additionally names the button under the pointer, which lightens on top
/// (`%notification_button:hover`) (`_common.scss:154-161`, `_message-list.scss`).
#[derive(Debug, Clone, PartialEq, Eq)]
enum ListHover {
    /// The card with this notification id is hovered; `zone` is the button under
    /// the pointer (close ×, caret, or an action pill), or `None` for the body.
    Card {
        id: u32,
        zone: Option<notification_card::CardZone>,
    },
    /// The expanded group header's collapse button.
    GroupCollapse,
    /// The Clear pill in the controls row.
    Clear,
    /// A media card, and the transport button under the pointer if any.
    Media {
        bus_name: String,
        control: Option<media_card::MediaControl>,
    },
}

/// A peeking lower card in a collapsed stack: `(origin, size, darkened bg)`.
type StackPeek = (Point<f64, Logical>, Size<f64, Logical>, [f32; 4]);
/// A visible group for introspection: `(source key, popover-local bounds, expanded?)`.
type GroupRect = (SourceKey, Rectangle<f64, Logical>, bool);

/// A media card laid out in the visible list, above every group.
struct MediaPlaced {
    /// Index into [`CalendarMessageList::players`].
    player: usize,
    origin: Point<f64, Logical>,
    layout: media_card::MediaLayout,
}

/// A group laid out in the visible list: the y-flow is computed once and
/// shared by the hit-test, the render, and the test hooks.
struct GroupLayout {
    /// Index into [`CalendarMessageList::groups`].
    group: usize,
    /// The group's total popover-local bounds (for the coarse hit-test).
    bounds: Rectangle<f64, Logical>,
    kind: GroupKind,
}

enum GroupKind {
    /// A single plain card — a one-notification group, laid out and hit-tested
    /// exactly like a flat card (`js/ui/messageList.js:951-954`).
    Single {
        origin: Point<f64, Logical>,
        layout: CardLayout,
    },
    /// A collapsed fanned stack: the interactive top card over darkened peeks
    /// (`js/ui/messageList.js:1370-1404`).
    Collapsed {
        origin: Point<f64, Logical>,
        top: CardLayout,
        /// Shallow-first (`(origin, size, bg)` of each peeking lower card):
        /// FIRST = topmost, so second-in-stack paints over lower-in-stack.
        peeks: Vec<StackPeek>,
        /// Every notification id in the group (a collapsed close closes them all).
        ids: Vec<u32>,
    },
    /// An expanded group: a header (title + collapse button) over each card
    /// laid out full-height (`js/ui/messageList.js:971-985,1276-1294`).
    Expanded {
        header: Rectangle<f64, Logical>,
        collapse: Rectangle<f64, Logical>,
        title: String,
        /// `(card index within the group, origin, layout)`.
        cards: Vec<(usize, Point<f64, Logical>, CardLayout)>,
    },
}

/// The content layout resolved against a popover height and the current scroll:
/// content-space group layouts plus the transform into popover-local space.
struct Placed {
    /// Media-card layouts in content space, above every group.
    media: Vec<MediaPlaced>,
    /// Group layouts in content space (y from 0, un-clipped).
    layouts: Vec<GroupLayout>,
    /// Total content height (all groups, no drop).
    content_h: f64,
    /// Content→popover y offset: `LIST_PAD - clamped_scroll`.
    off_y: f64,
    /// Clamped scroll offset (content px from the top).
    scroll: f64,
    /// The scroll viewport, popover-local (list top to the controls row).
    viewport: Rectangle<f64, Logical>,
}

impl Placed {
    /// Whether the content overflows the viewport (so the list clips + shows a
    /// scrollbar).
    fn overflowing(&self) -> bool {
        self.content_h > self.viewport.size.h + 0.5
    }

    /// A content-space rect mapped to popover-local space.
    fn to_popover(&self, rect: Rectangle<f64, Logical>) -> Rectangle<f64, Logical> {
        Rectangle::new(rect.loc + Point::from((0., self.off_y)), rect.size)
    }

    /// Whether a popover-local rect is at least partly inside the viewport (so
    /// it is visible / interactive).
    fn viewport_visible(&self, rect: Rectangle<f64, Logical>) -> bool {
        let v = self.viewport;
        rect.size.h > 0. && rect.loc.y < v.loc.y + v.size.h && v.loc.y < rect.loc.y + rect.size.h
    }
}

/// The message-list column of the calendar popover: a plain-data snapshot of
/// the notification store, grouped per source (`NotificationMessageGroup`).
/// One-notification groups render as plain cards; larger ones fan into a
/// collapsed stack that expands into a vertical list on click.
pub struct CalendarMessageList {
    groups: Vec<CardGroup>,
    /// The MPRIS players, each a card **above** every notification group: gnome-shell inserts a
    /// new player's message at index 0 of the view (`js/ui/messageList.js:1780-1784`), so this is
    /// held newest-first and rendered in order.
    players: Vec<media_card::MediaCardContent>,
    /// Notification ids whose card BODY is expanded (the header caret). Kept
    /// across snapshot pushes, dropped with the popover (recorded divergence).
    body_expanded: HashSet<u32>,
    /// The one group fanned open into a vertical list, keyed by its source
    /// (`js/ui/messageList._expandedGroup` — a single group at a time,
    /// `:1870-1897`). Retained across snapshot pushes while the source lives.
    group_expanded: Option<SourceKey>,
    /// The control the pointer is hovering, highlighted on render.
    hovered: Option<ListHover>,
    /// Bumped whenever the snapshot changes, to invalidate cached textures
    /// (cache keys carry the revision in their high 32 bits).
    revision: u64,
    /// Scroll offset (content px from the top), like gnome-shell's
    /// `St.ScrollView` over the message view. Clamped to `[0, max_scroll]` at
    /// every use against the live popover height, so a content shrink (a
    /// collapse, a close) re-snaps it without a separate hook.
    scroll_y: f64,
    cache: RefCell<CardCache>,
    /// The whole list content baked into one texture (all groups, no drop),
    /// keyed by `(scale, revision)` — so scrolling only moves the src-crop
    /// window and never re-bakes. Only populated on the overflow path.
    content_cache: RefCell<Option<ContentTex>>,
}

/// A cached bake of the full (un-clipped) list content; scrolling re-samples a
/// window of it rather than re-rendering.
struct ContentTex {
    scale: NotNan<f64>,
    revision: u64,
    /// The scroll offset (rounded px) the window was baked at — the bake is
    /// viewport-sized, so it re-bakes when the scroll moves.
    scroll: i64,
    context: ContextId<VkTexture>,
    tex: VkTexture,
}

impl CalendarMessageList {
    pub fn new(groups: Vec<CardGroup>) -> Self {
        Self {
            groups,
            players: Vec::new(),
            body_expanded: HashSet::new(),
            group_expanded: None,
            hovered: None,
            revision: 0,
            scroll_y: 0.,
            cache: RefCell::new(CardCache::new()),
            content_cache: RefCell::new(None),
        }
    }

    /// Nothing at all to show — what puts the "No Notifications" placeholder up
    /// (`MessageView.empty`, `js/ui/messageList.js:1521-1523`).
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty() && self.players.is_empty()
    }

    /// Whether the Clear pill shows: `canClear` is "some message can close"
    /// (`js/ui/messageList.js:1525-1527`), and a media card cannot
    /// (`canClose() = false`, `:668-670`) — so players alone show no pill.
    pub fn can_clear(&self) -> bool {
        !self.groups.is_empty()
    }

    /// Replace the player snapshot (newest first). Returns whether anything changed.
    pub fn set_players(&mut self, players: Vec<media_card::MediaCardContent>) -> bool {
        if self.players == players {
            return false;
        }
        self.players = players;
        // Same reasoning as `set_groups`: the cards just moved, so a captured hover is stale.
        self.hovered = None;
        self.revision += 1;
        true
    }

    /// An album-art load landed: bump the revision if a card is showing that source, so the list
    /// re-bakes with the art in place of the themed fallback. Returns whether anything changed.
    ///
    /// The cards' cache keys are *positional* and revision-scoped — nothing in them hashes the
    /// content — so an async load arriving is invisible to the cache unless it says so here.
    /// Without this the first frame's fallback (and the backdrop baked behind it) would stay up
    /// until some unrelated change bumped the revision.
    pub fn note_art_decoded(&mut self, source: &ImageSource) -> bool {
        if !self.players.iter().any(|p| p.art.as_ref() == Some(source)) {
            return false;
        }
        self.revision += 1;
        true
    }

    /// The art sources the list is currently showing.
    pub fn art_sources(&self) -> impl Iterator<Item = &ImageSource> {
        self.players.iter().filter_map(|p| p.art.as_ref())
    }

    /// Total notification count across every group.
    pub fn len(&self) -> usize {
        self.groups.iter().map(|g| g.cards.len()).sum()
    }

    /// Replace the snapshot (a store change pushed while the popover is open).
    /// Returns whether anything changed.
    pub fn set_groups(&mut self, groups: Vec<CardGroup>) -> bool {
        if self.groups == groups {
            return false;
        }
        self.groups = groups;
        // Drop expansion state for ids/sources that are gone.
        let live: HashSet<u32> = self
            .groups
            .iter()
            .flat_map(|g| g.cards.iter().map(|c| c.id))
            .collect();
        self.body_expanded.retain(|id| live.contains(id));
        // Drop the expanded-group state when its source is gone OR has shrunk
        // to a single notification — gnome-shell collapses a group down to one
        // (`messageList.js:1170-1173`), so a later arrival shows a fresh
        // collapsed stack, not a resurrected expansion.
        if let Some(key) = &self.group_expanded {
            let still_grouped = self
                .groups
                .iter()
                .any(|g| &g.key == key && g.cards.len() > 1);
            if !still_grouped {
                self.group_expanded = None;
            }
        }
        // The cards just shifted (added/removed/reordered), so a hover captured
        // against the old layout is stale — drop it; the next pointer motion
        // re-resolves it against the new layout.
        self.hovered = None;
        self.revision += 1;
        true
    }

    /// Toggle a card's body expansion (its caret was clicked).
    fn toggle_body_expanded(&mut self, id: u32) {
        if !self.body_expanded.remove(&id) {
            self.body_expanded.insert(id);
        }
        self.revision += 1;
    }

    /// Un-expand every member body of the group keyed by `key` — gnome-shell's
    /// `collapse()` runs `message.unexpand()` on all members
    /// (`js/ui/messageList.js:988`), so a re-expand shows collapsed bodies.
    fn unexpand_group_bodies(&mut self, key: &SourceKey) {
        if let Some(group) = self.groups.iter().find(|g| &g.key == key) {
            for card in &group.cards {
                self.body_expanded.remove(&card.id);
            }
        }
    }

    /// Expand `key`'s stack (collapsing any other), or collapse it if it is
    /// already the expanded group (`js/ui/messageList.js:1809-1814`).
    fn toggle_group(&mut self, key: SourceKey) {
        self.group_expanded = if self.group_expanded.as_ref() == Some(&key) {
            self.unexpand_group_bodies(&key);
            None
        } else {
            Some(key)
        };
        self.revision += 1;
    }

    fn collapse_group(&mut self) {
        if let Some(key) = self.group_expanded.take() {
            self.unexpand_group_bodies(&key);
            self.revision += 1;
        }
    }

    /// Height of the bottom controls row (the Clear pill), when shown.
    fn controls_h(&self) -> f64 {
        if !self.can_clear() {
            0.
        } else {
            CONTROLS_PAD + CLEAR_H + CONTROLS_PAD_B
        }
    }

    /// One card's layout, body-caret-aware. An expanded body shows its full
    /// (≤`EXPAND_LINES`) wrap; the list scrolls to reach overflow, so there is
    /// no popover-height clamp any more. `caret` reserves the header expand
    /// slot (false on a collapsed stack's top card, where the whole stack is
    /// the click target).
    fn card_layout(&self, content: &CardContent, caret: bool) -> CardLayout {
        let expanded = caret && self.body_expanded.contains(&content.id);
        notification_card::layout(content, card_w(), expanded, caret)
    }

    /// Collapsed-stack geometry for a group with >1 card: the top card layout,
    /// the peeking lower cards' rects (back-to-front), and the total stack
    /// height (`js/ui/messageList.js:1314-1350,1370-1404`).
    fn stack_geometry(
        &self,
        group: &CardGroup,
        origin: Point<f64, Logical>,
    ) -> (CardLayout, Vec<StackPeek>, f64) {
        use notification_card::{
            stack_bg, STACK_HEIGHT_OFFSET, STACK_HEIGHT_REDUCTION, STACK_MAX_VISIBLE,
            STACK_WIDTH_INSET,
        };
        // The top card carries no caret: the whole stack is one click target.
        let top = notification_card::layout(&group.cards[0], card_w(), false, false);
        let top_h = top.size.h;
        let visible = group.cards.len().min(STACK_MAX_VISIBLE);
        let mut peeks = Vec::new();
        let mut cumulative = 0.;
        let mut offset = STACK_HEIGHT_OFFSET;
        for depth in 1..visible {
            cumulative += offset;
            offset /= STACK_HEIGHT_REDUCTION;
            let inset = STACK_WIDTH_INSET * depth as f64;
            peeks.push((
                Point::from((origin.x + inset, origin.y + cumulative)),
                Size::from((card_w() - 2. * inset, top_h)),
                stack_bg(depth),
            ));
        }
        // `peeks` is shallow-first (depth 1, 2, …). The element convention is
        // FIRST = topmost, so pushing them after the top card in this order
        // paints the second-in-stack ABOVE the lower-in-stack (matching
        // gnome-shell's reversed child paint, `messageList.js:1179-1191`).
        (top, peeks, top_h + cumulative)
    }

    /// Lay out every group in content space (y from 0), never dropping — the
    /// list scrolls to reach overflow (`js/ui/calendar.js:816` `St.ScrollView`).
    /// Returns the layouts and the total content height.
    fn layout(&self) -> (Vec<MediaPlaced>, Vec<GroupLayout>, f64) {
        let mut out = Vec::new();
        let mut y = 0.0_f64;
        let mut content_h = 0.0_f64;

        // Media cards sit above every group, in `players` order — the view holds them at index 0
        // and each new player pushes the older ones down (`js/ui/messageList.js:1780-1784`).
        let mut media = Vec::new();
        for (i, _) in self.players.iter().enumerate() {
            let origin = Point::from((LIST_PAD, y));
            let layout = media_card::layout(card_w());
            let h = layout.size.h;
            media.push(MediaPlaced {
                player: i,
                origin,
                layout,
            });
            content_h = content_h.max(y + h);
            y += h + CARD_GAP;
        }

        for (g, group) in self.groups.iter().enumerate() {
            // The store never yields an empty source (`notifications.rs:619`),
            // but `CardGroup` is public — skip rather than index-panic.
            debug_assert!(!group.cards.is_empty(), "a CardGroup must have >=1 card");
            if group.cards.is_empty() {
                continue;
            }
            let expanded = self.group_expanded.as_ref() == Some(&group.key);
            if group.cards.len() <= 1 {
                let origin = Point::from((LIST_PAD, y));
                let layout = self.card_layout(&group.cards[0], true);
                let h = layout.size.h;
                out.push(GroupLayout {
                    group: g,
                    bounds: Rectangle::new(origin, layout.size),
                    kind: GroupKind::Single { origin, layout },
                });
                content_h = content_h.max(y + h);
                y += h + CARD_GAP;
            } else if expanded {
                let header = Rectangle::new(
                    Point::from((LIST_PAD, y)),
                    Size::from((card_w(), GROUP_HEADER_H)),
                );
                let collapse = Rectangle::new(
                    Point::from((
                        LIST_PAD + card_w() - GROUP_HEADER_PAD - GROUP_COLLAPSE_D,
                        y + (GROUP_HEADER_H - GROUP_COLLAPSE_D) / 2.,
                    )),
                    Size::from((GROUP_COLLAPSE_D, GROUP_COLLAPSE_D)),
                );
                let mut cy = y + GROUP_HEADER_H;
                let mut cards = Vec::new();
                for (ci, content) in group.cards.iter().enumerate() {
                    let layout = self.card_layout(content, true);
                    let h = layout.size.h;
                    cards.push((ci, Point::from((LIST_PAD, cy)), layout));
                    cy += h + CARD_GAP;
                }
                let group_bottom = cards
                    .last()
                    .map_or(cy, |(_, o, l)| o.y + l.size.h)
                    .max(header.loc.y + GROUP_HEADER_H);
                out.push(GroupLayout {
                    group: g,
                    bounds: Rectangle::new(
                        Point::from((LIST_PAD, y)),
                        Size::from((card_w(), group_bottom - y)),
                    ),
                    kind: GroupKind::Expanded {
                        header,
                        collapse,
                        title: group.source_title.clone(),
                        cards,
                    },
                });
                content_h = content_h.max(group_bottom);
                // The card margin plus an expanded group's extra bottom margin.
                y = group_bottom + CARD_GAP + notification_card::GROUP_BOTTOM_MARGIN;
            } else {
                let origin = Point::from((LIST_PAD, y));
                let (top, peeks, stack_h) = self.stack_geometry(group, origin);
                let ids = group.cards.iter().map(|c| c.id).collect();
                out.push(GroupLayout {
                    group: g,
                    bounds: Rectangle::new(origin, Size::from((card_w(), stack_h))),
                    kind: GroupKind::Collapsed {
                        origin,
                        top,
                        peeks,
                        ids,
                    },
                });
                content_h = content_h.max(y + stack_h);
                y += stack_h + CARD_GAP;
            }
        }
        (media, out, content_h)
    }

    /// The scroll viewport height: the list top (`LIST_PAD`) down to the
    /// controls row.
    fn viewport_h(&self, height: f64) -> f64 {
        (height - self.controls_h() - LIST_PAD).max(0.)
    }

    /// Lay out and resolve the scroll transform for `height` in one pass.
    fn placed(&self, height: f64) -> Placed {
        let (media, layouts, content_h) = self.layout();
        let vh = self.viewport_h(height);
        let scroll = self.scroll_y.clamp(0., (content_h - vh).max(0.));
        Placed {
            media,
            layouts,
            content_h,
            off_y: LIST_PAD - scroll,
            scroll,
            viewport: Rectangle::new(Point::from((0., LIST_PAD)), Size::from((list_w(), vh))),
        }
    }

    /// Scroll by `delta` content px (positive = down); returns whether it moved
    /// (so the caller can request a redraw). The delta is applied to the
    /// *clamped* current offset, so a scroll right after the content shrank
    /// (leaving `scroll_y` stale-too-large) still moves instead of eating a
    /// notch.
    fn scroll_by(&mut self, delta: f64, height: f64) -> bool {
        let vh = self.viewport_h(height);
        let max = (self.content_h() - vh).max(0.);
        let cur = self.scroll_y.clamp(0., max);
        let after = (cur + delta).clamp(0., max);
        self.scroll_y = after;
        (after - cur).abs() > f64::EPSILON
    }

    /// Total content height (all groups, no drop).
    fn content_h(&self) -> f64 {
        self.layout().2
    }

    /// The list's full, un-scrolled height: top pad + group content + the
    /// controls row. The popover grows to this (capped to the work area) so the
    /// list scrolls only once it would exceed the screen, matching gnome-shell's
    /// work-area `max-height` on the menu (`js/ui/panelMenu.js:177-185`).
    fn natural_height(&self) -> f64 {
        LIST_PAD + self.content_h() + self.controls_h()
    }

    /// The Clear pill's popover-local rect (only meaningful when non-empty).
    fn clear_rect(&self, height: f64) -> Rectangle<f64, Logical> {
        let label_w =
            synoik_vk::text::measure_line_width_weighted("Clear", clear_px() as f32, true);
        Rectangle::new(
            Point::from((LIST_PAD + CONTROLS_PAD, height - CONTROLS_PAD_B - CLEAR_H)),
            Size::from((label_w + 2. * CLEAR_PAD_X, CLEAR_H)),
        )
    }

    /// Resolve a card click at card-local `local` against `layout` for the
    /// notification `content` — the shared per-card interaction (close, caret,
    /// action, body).
    fn card_hit(local: Point<f64, Logical>, content: &CardContent, layout: &CardLayout) -> ListHit {
        if layout.close.contains(local) {
            return ListHit::Close(content.id);
        }
        if layout
            .expand
            .filter(|_| layout.can_expand)
            .is_some_and(|e| e.contains(local))
        {
            return ListHit::ToggleExpand(content.id);
        }
        for (idx, rect) in layout.actions.iter().enumerate() {
            if rect.contains(local) {
                return ListHit::Action {
                    id: content.id,
                    key: content.actions[idx].0.clone(),
                };
            }
        }
        ListHit::Body {
            id: content.id,
            has_default: content.has_default_action,
        }
    }

    /// Hit-test a click at popover-local `pos` inside the list column.
    fn hit(&self, pos: Point<f64, Logical>, height: f64) -> Option<ListHit> {
        // The Clear pill lives in the fixed controls row, below the viewport.
        if self.can_clear() && self.clear_rect(height).contains(pos) {
            return Some(ListHit::Clear);
        }
        let p = self.placed(height);
        // Clicks register only inside the scroll viewport — the scrollbar strip
        // and the padding around the list are consumed but hit nothing.
        if !p.viewport.contains(pos) {
            return None;
        }
        // Map the click into content space (undo the scroll translation).
        let cpos = pos - Point::from((0., p.off_y));
        for m in &p.media {
            if !Rectangle::new(m.origin, m.layout.size).contains(cpos) {
                continue;
            }
            let content = &self.players[m.player];
            let bus_name = content.bus_name.clone();
            // An insensitive skip button is `reactive = false` (`js/ui/messageList.js:836-838`),
            // so the click passes through it to the message itself — which raises the player.
            let control = m
                .layout
                .control_at(cpos - m.origin)
                .filter(|c| content.is_sensitive(*c));
            return Some(match control {
                Some(control) => ListHit::MediaControl { bus_name, control },
                None => ListHit::MediaBody(bus_name),
            });
        }
        for gl in &p.layouts {
            if !gl.bounds.contains(cpos) {
                continue;
            }
            let group = &self.groups[gl.group];
            match &gl.kind {
                GroupKind::Single { origin, layout } => {
                    return Some(Self::card_hit(cpos - *origin, &group.cards[0], layout));
                }
                GroupKind::Collapsed {
                    origin, top, ids, ..
                } => {
                    // The top card's close closes the whole group; anything
                    // else on the stack expands it.
                    if top.close.contains(cpos - *origin) {
                        return Some(ListHit::CloseGroup(ids.clone()));
                    }
                    return Some(ListHit::ExpandGroup(group.key.clone()));
                }
                GroupKind::Expanded {
                    collapse, cards, ..
                } => {
                    if collapse.contains(cpos) {
                        return Some(ListHit::CollapseGroup);
                    }
                    for (ci, origin, layout) in cards {
                        if Rectangle::new(*origin, layout.size).contains(cpos) {
                            return Some(Self::card_hit(cpos - *origin, &group.cards[*ci], layout));
                        }
                    }
                    // A click on the header (off the button) or an inter-card
                    // gap collapses the group — gnome-shell's group-wide gesture
                    // fires `expand-toggle-requested` on any unclaimed click
                    // (`js/ui/messageList.js:879,934-935`).
                    return Some(ListHit::CollapseGroup);
                }
            }
        }
        None
    }

    /// Update the hovered control from a popover-local position (`None` clears
    /// it). Returns whether the hover changed, so the caller can redraw. A hover
    /// change bumps `revision`, re-baking the affected textures with the wash.
    pub fn hover(&mut self, pos: Option<Point<f64, Logical>>, height: f64) -> bool {
        let new = pos.and_then(|pos| self.hover_zone(pos, height));
        if new == self.hovered {
            return false;
        }
        // A card/collapse wash is baked into the (revision-keyed) card and header
        // textures, so entering/leaving one must re-bake them. The Clear pill
        // lives in the separately-keyed bg texture (its own clear-hover bit), so
        // a Clear/None transition needs a redraw but NOT a full card re-bake —
        // don't bump the revision for it. (A card→card crossing still re-bakes
        // every visible card; acceptable at popover card counts.)
        let touches_cards = |h: &Option<ListHover>| {
            matches!(
                h,
                Some(ListHover::Card { .. })
                    | Some(ListHover::GroupCollapse)
                    | Some(ListHover::Media { .. })
            )
        };
        if touches_cards(&self.hovered) || touches_cards(&new) {
            self.revision += 1;
        }
        self.hovered = new;
        true
    }

    /// The hoverable control at popover-local `pos`, mirroring [`hit`](Self::hit)
    /// but only for controls that carry a visible highlight (card buttons, the
    /// group collapse button, the Clear pill — not card bodies or the stack).
    fn hover_zone(&self, pos: Point<f64, Logical>, height: f64) -> Option<ListHover> {
        if self.can_clear() && self.clear_rect(height).contains(pos) {
            return Some(ListHover::Clear);
        }
        let p = self.placed(height);
        if !p.viewport.contains(pos) {
            return None;
        }
        let cpos = pos - Point::from((0., p.off_y));
        for m in &p.media {
            if !Rectangle::new(m.origin, m.layout.size).contains(cpos) {
                continue;
            }
            let content = &self.players[m.player];
            return Some(ListHover::Media {
                bus_name: content.bus_name.clone(),
                // Only a reactive button lights up.
                control: m
                    .layout
                    .control_at(cpos - m.origin)
                    .filter(|c| content.is_sensitive(*c)),
            });
        }
        for gl in &p.layouts {
            if !gl.bounds.contains(cpos) {
                continue;
            }
            let group = &self.groups[gl.group];
            match &gl.kind {
                GroupKind::Single { origin, layout } => {
                    return Self::card_hover(cpos - *origin, &group.cards[0], layout);
                }
                GroupKind::Collapsed { origin, top, .. } => {
                    // Hovering the collapsed stack darkens its (interactive) top
                    // card; over the top card's close, that button also lightens
                    // — it closes the whole group.
                    let zone = top
                        .close
                        .contains(cpos - *origin)
                        .then_some(notification_card::CardZone::Close);
                    return group
                        .cards
                        .first()
                        .map(|c| ListHover::Card { id: c.id, zone });
                }
                GroupKind::Expanded {
                    collapse, cards, ..
                } => {
                    if collapse.contains(cpos) {
                        return Some(ListHover::GroupCollapse);
                    }
                    for (ci, origin, layout) in cards {
                        if Rectangle::new(*origin, layout.size).contains(cpos) {
                            return Self::card_hover(cpos - *origin, &group.cards[*ci], layout);
                        }
                    }
                    return None;
                }
            }
        }
        None
    }

    /// Hovering anywhere on a card marks it hovered (its body darkens); if the
    /// pointer is over a button (close/caret/action), that `zone` is named so it
    /// also lightens.
    fn card_hover(
        local: Point<f64, Logical>,
        content: &CardContent,
        layout: &CardLayout,
    ) -> Option<ListHover> {
        use notification_card::CardZone;
        let zone = if layout.close.contains(local) {
            Some(CardZone::Close)
        } else if layout
            .expand
            .filter(|_| layout.can_expand)
            .is_some_and(|e| e.contains(local))
        {
            Some(CardZone::Caret)
        } else {
            layout
                .actions
                .iter()
                .position(|rect| rect.contains(local))
                .map(CardZone::Action)
        };
        Some(ListHover::Card {
            id: content.id,
            zone,
        })
    }

    /// Whether the card with notification `id` is hovered (body darkens) and,
    /// if so, which of its buttons the pointer is over (that button lightens) —
    /// fed into [`notification_card::card_elements`].
    fn media_hover_for(&self, bus_name: &str) -> (bool, Option<media_card::MediaControl>) {
        match &self.hovered {
            Some(ListHover::Media {
                bus_name: hovered,
                control,
            }) if hovered == bus_name => (true, *control),
            _ => (false, None),
        }
    }

    fn card_hover_for(&self, id: u32) -> (bool, Option<notification_card::CardZone>) {
        match &self.hovered {
            Some(ListHover::Card { id: hid, zone }) if *hid == id => (true, *zone),
            _ => (false, None),
        }
    }

    /// The render elements (textures + icons), popover-relative to `origin`.
    /// When the content fits, groups are placed directly; when it overflows,
    /// the whole content is baked into one texture and a scrolled, clipped
    /// window of it is presented, with an overlay scrollbar thumb on top
    /// (gnome-shell's `St.ScrollView`, `js/ui/calendar.js:816`).
    #[allow(clippy::too_many_arguments)]
    fn render(
        &self,
        renderer: &mut VulkanRenderer,
        icons: &IconCache,
        app_icons: &AppIconCache,
        images: &ImageCache,
        scale: f64,
        origin: Point<f64, Logical>,
        height: f64,
    ) -> Vec<TextureRenderElement<VkTexture>> {
        let p = self.placed(height);
        if !p.overflowing() {
            // Everything fits: place the groups directly (scroll is 0, so
            // `off_y == LIST_PAD`).
            let base = origin + Point::from((0., p.off_y));
            return self.render_groups(
                renderer, icons, app_icons, images, scale, base, &p.media, &p.layouts,
            );
        }

        // Overflow: bake just the visible window (a viewport-sized texture, so
        // its dimensions stay bounded however much content there is) and
        // present it, with the scrollbar thumb composited on top (FIRST =
        // topmost). On a bake failure, draw nothing — never a lone thumb over
        // an empty column.
        let mut elements = Vec::new();
        match self.content_texture(renderer, icons, app_icons, images, scale, &p) {
            Ok(tex) => {
                if let Some(thumb) = self.scrollbar_thumb(renderer, scale, origin, &p) {
                    elements.push(thumb);
                }
                let buffer = TextureBuffer::from_texture(
                    renderer,
                    tex,
                    scale,
                    Transform::Normal,
                    Vec::new(),
                );
                elements.push(TextureRenderElement::from_texture_buffer(
                    buffer,
                    origin + p.viewport.loc,
                    1.,
                    None,
                    Some(p.viewport.size),
                    Kind::Unspecified,
                ));
            }
            Err(err) => tracing::error!("error baking the message list: {err:#}"),
        }
        elements
    }

    /// Build the group render elements at `base` (a content-space origin),
    /// managing the per-card texture cache. Shared by the direct (in-place) and
    /// the baked (offscreen → clipped) render paths.
    #[allow(clippy::too_many_arguments)]
    fn render_groups(
        &self,
        renderer: &mut VulkanRenderer,
        icons: &IconCache,
        app_icons: &AppIconCache,
        images: &ImageCache,
        scale: f64,
        base: Point<f64, Logical>,
        media: &[MediaPlaced],
        layouts: &[GroupLayout],
    ) -> Vec<TextureRenderElement<VkTexture>> {
        let mut elements = Vec::new();
        let mut cache = self.cache.borrow_mut();
        let rev = self.revision & 0xffff_ffff;
        cache.retain(|key| key >> 32 == rev);
        // A monotonic per-render key (top 32 bits = revision) so every texture
        // (cards, peeks, headers) gets a distinct, revision-scoped cache slot.
        let mut next = 0u64;
        let mut key = || {
            let k = (rev << 32) | next;
            next += 1;
            k
        };
        for m in media {
            let content = &self.players[m.player];
            let (card_hovered, control) = self.media_hover_for(&content.bus_name);
            elements.extend(media_card::media_card_elements(
                renderer,
                icons,
                app_icons,
                images,
                &mut cache,
                key(),
                content,
                &m.layout,
                CARD_RADIUS,
                base + m.origin,
                1.,
                scale,
                card_hovered,
                control,
            ));
        }
        for gl in layouts {
            let group = &self.groups[gl.group];
            match &gl.kind {
                GroupKind::Single { origin: o, layout } => {
                    let (card_hovered, button) = self.card_hover_for(group.cards[0].id);
                    elements.extend(notification_card::card_elements(
                        renderer,
                        icons,
                        app_icons,
                        &mut cache,
                        key(),
                        &group.cards[0],
                        layout,
                        CARD_RADIUS,
                        base + *o,
                        1.,
                        scale,
                        card_hovered,
                        button,
                    ));
                }
                GroupKind::Collapsed {
                    origin: o,
                    top,
                    peeks,
                    ..
                } => {
                    // Top card on top, then the darkened peeks below it.
                    let (card_hovered, button) = self.card_hover_for(group.cards[0].id);
                    elements.extend(notification_card::card_elements(
                        renderer,
                        icons,
                        app_icons,
                        &mut cache,
                        key(),
                        &group.cards[0],
                        top,
                        CARD_RADIUS,
                        base + *o,
                        1.,
                        scale,
                        card_hovered,
                        button,
                    ));
                    for (peek_o, size, bg) in peeks {
                        if let Some(elem) = notification_card::stack_shadow_element(
                            renderer,
                            &mut cache,
                            key(),
                            *size,
                            CARD_RADIUS,
                            *bg,
                            base + *peek_o,
                            scale,
                        ) {
                            elements.push(elem);
                        }
                    }
                }
                GroupKind::Expanded {
                    header,
                    collapse,
                    title,
                    cards,
                } => {
                    elements.extend(self.header_elements(
                        renderer,
                        icons,
                        &mut cache,
                        key(),
                        title,
                        *header,
                        *collapse,
                        base,
                        scale,
                        self.hovered == Some(ListHover::GroupCollapse),
                    ));
                    for (ci, card_o, layout) in cards {
                        let (card_hovered, button) = self.card_hover_for(group.cards[*ci].id);
                        elements.extend(notification_card::card_elements(
                            renderer,
                            icons,
                            app_icons,
                            &mut cache,
                            key(),
                            &group.cards[*ci],
                            layout,
                            CARD_RADIUS,
                            base + *card_o,
                            1.,
                            scale,
                            card_hovered,
                            button,
                        ));
                    }
                }
            }
        }
        elements
    }

    /// Bake the visible window of the list into a viewport-sized texture, the
    /// content shifted up by the scroll offset (elements outside the window are
    /// clipped by the texture bounds). Sizing to the viewport — not the full
    /// content — keeps the texture dimensions bounded regardless of how many
    /// notifications there are. Cached by `(scale, revision, scroll)`: idle
    /// re-renders reuse it, a scroll re-bakes (bounded, cheap: it re-composites
    /// the already-cached per-card textures).
    fn content_texture(
        &self,
        renderer: &mut VulkanRenderer,
        icons: &IconCache,
        app_icons: &AppIconCache,
        images: &ImageCache,
        scale: f64,
        p: &Placed,
    ) -> anyhow::Result<VkTexture> {
        let scale_key = NotNan::new(scale).map_err(|_| anyhow::anyhow!("bad scale"))?;
        let scroll_key = p.scroll.round() as i64;
        let context = renderer.context_id();
        {
            let cache = self.content_cache.borrow();
            if let Some(c) = cache.as_ref() {
                if c.scale == scale_key
                    && c.revision == self.revision
                    && c.scroll == scroll_key
                    && c.context == context
                {
                    return Ok(c.tex.clone());
                }
            }
        }
        let px = |v: f64| to_physical_precise_round::<i32>(scale, v);
        let phys = Size::<i32, Physical>::from((px(list_w()).max(1), px(p.viewport.size.h).max(1)));
        // Content y=scroll lands at the texture top; everything above/below the
        // window falls outside the buffer and is clipped.
        let base = Point::from((0., -p.scroll));
        let elements = self.render_groups(
            renderer, icons, app_icons, images, scale, base, &p.media, &p.layouts,
        );
        // `render_groups` returns FIRST = topmost (the compositor's convention,
        // `notification_card.rs:604`), but `render_to_texture` paints in
        // iteration order (first = backmost). Reverse so the bake matches what
        // the compositor would draw — else the card background paints over the
        // close-× / caret icons (empty circles) and peek/group z-order inverts.
        let (tex, _sync) = render_to_texture(
            renderer,
            phys,
            Scale::from(scale),
            Transform::Normal,
            NATIVE_FOURCC,
            elements.into_iter().rev(),
        )?;
        renderer.make_offscreen_sampleable(&tex)?;
        *self.content_cache.borrow_mut() = Some(ContentTex {
            scale: scale_key,
            revision: self.revision,
            scroll: scroll_key,
            context,
            tex: tex.clone(),
        });
        Ok(tex)
    }

    /// The overlay scrollbar thumb (drawn on top of the clipped content) when
    /// the content overflows; its length tracks the visible fraction and its
    /// position the scroll offset. The texture is cached by revision (only its
    /// location moves as you scroll).
    fn scrollbar_thumb(
        &self,
        renderer: &mut VulkanRenderer,
        scale: f64,
        origin: Point<f64, Logical>,
        p: &Placed,
    ) -> Option<TextureRenderElement<VkTexture>> {
        let vh = p.viewport.size.h;
        let max_scroll = (p.content_h - vh).max(0.);
        if max_scroll <= 0. {
            return None;
        }
        // Track the visible fraction, floored at a min handle — but never
        // above `vh` (a very short viewport must not make `min > max`).
        let thumb_h = (vh * vh / p.content_h).clamp(SCROLLBAR_MIN_H.min(vh), vh);
        let thumb_y = p.viewport.loc.y + (p.scroll / max_scroll) * (vh - thumb_h);
        let thumb_x = list_w() - SCROLLBAR_W - SCROLLBAR_EDGE_GAP;
        let mut cache = self.cache.borrow_mut();
        let rev = self.revision & 0xffff_ffff;
        // A fixed key well above `render_groups`' running counter (which stays
        // small), so the thumb never collides with a card slot.
        let key = (rev << 32) | 0x00ff_ffff;
        notification_card::stack_shadow_element(
            renderer,
            &mut cache,
            key,
            Size::from((SCROLLBAR_W, thumb_h)),
            SCROLLBAR_W / 2.,
            SCROLLBAR_THUMB,
            origin + Point::from((thumb_x, thumb_y)),
            scale,
        )
    }

    /// The expanded group's header: the title glyphs + collapse-button circle
    /// (one cached texture), with the `group-collapse-symbolic` chevron
    /// composited on top.
    #[allow(clippy::too_many_arguments)]
    fn header_elements(
        &self,
        renderer: &mut VulkanRenderer,
        icons: &IconCache,
        cache: &mut CardCache,
        key: u64,
        title: &str,
        header: Rectangle<f64, Logical>,
        collapse: Rectangle<f64, Logical>,
        origin: Point<f64, Logical>,
        scale: f64,
        hover_collapse: bool,
    ) -> Vec<TextureRenderElement<VkTexture>> {
        use notification_card::SMALL_ICON;

        let mut elements = Vec::new();
        let Ok(scale_key) = NotNan::new(scale) else {
            return elements;
        };
        cache.ensure_context(renderer);
        // The collapse chevron, composited over the header texture.
        let icon_center = origin
            + Point::from((
                collapse.loc.x + collapse.size.w / 2.,
                collapse.loc.y + collapse.size.h / 2.,
            ));
        if let Some(tb) =
            icons.texture(renderer, "group-collapse-symbolic", SMALL_ICON, scale, TEXT)
        {
            let logical = tb.logical_size();
            let loc = icon_center - Point::from((logical.w / 2., logical.h / 2.));
            elements.push(TextureRenderElement::from_texture_buffer(
                tb,
                loc,
                1.,
                None,
                None,
                Kind::Unspecified,
            ));
        }
        // The header texture (title + button circle) below the chevron.
        if !cache.has_card(scale_key, key) {
            match self.draw_group_header(
                renderer,
                scale,
                title,
                &collapse,
                header.loc,
                hover_collapse,
            ) {
                Ok(texture) => cache.insert_card(scale_key, key, texture),
                Err(err) => tracing::error!("error drawing a group header: {err:#}"),
            }
        }
        if let Some(texture) = cache.get_card(scale_key, key) {
            let buffer = TextureBuffer::from_texture(
                renderer,
                texture,
                scale,
                Transform::Normal,
                Vec::new(),
            );
            elements.push(TextureRenderElement::from_texture_buffer(
                buffer,
                origin + header.loc,
                1.,
                None,
                None,
                Kind::Unspecified,
            ));
        }
        elements
    }

    /// Draw the expanded group's header into a texture: the bold title, and the
    /// collapse button's white@20% circle (the chevron composites on top).
    fn draw_group_header(
        &self,
        renderer: &mut VulkanRenderer,
        scale: f64,
        title: &str,
        collapse: &Rectangle<f64, Logical>,
        header_origin: Point<f64, Logical>,
        // header rect is `(card_w(), GROUP_HEADER_H)` at `header_origin`.
        hover_collapse: bool,
    ) -> anyhow::Result<VkTexture> {
        let px = |v: f64| to_physical_precise_round::<i32>(scale, v);
        let phys = Size::<i32, Physical>::from((px(card_w()).max(1), px(GROUP_HEADER_H).max(1)));
        let title_run = {
            let mut shaper = TextShaper::new(renderer, scale);
            shaper.shape(title, TextStyle::new(GROUP_TITLE_PT).bold())?
        };

        widget::bake_uncached_sized(renderer, phys, |frame| {
            let mut p = Painter::new(frame, scale, phys);
            p.clear(TRANSPARENT)?;

            // The collapse button circle (header-local coordinates).
            let btn = Rectangle::new(
                Point::<f64, Logical>::from((
                    collapse.loc.x - header_origin.x,
                    collapse.loc.y - header_origin.y,
                )),
                collapse.size,
            );
            p.fill_rounded(btn, GROUP_COLLAPSE_D / 2., GROUP_COLLAPSE_BG)?;
            if hover_collapse {
                p.fill_rounded(btn, GROUP_COLLAPSE_D / 2., HOVER_WASH)?;
            }

            // The title, left-aligned at the header padding + title margin,
            // vertically centered.
            p.text(
                &title_run,
                Point::from((GROUP_HEADER_PAD + GROUP_TITLE_MARGIN, GROUP_HEADER_H / 2.)),
                Align::LEFT_MIDDLE,
                TEXT,
            )?;

            Ok(())
        })
    }

    /// The visible per-card interactions: single-card groups and every card of
    /// an EXPANDED group (a collapsed stack's top card is not here — its close
    /// closes the whole group, so it goes through the group hooks instead).
    /// `(id, popover-local origin, layout)`.
    /// Test hook: the visible media cards `(bus name, card rect, control rects)`, popover-local.
    fn visible_media_cards(&self, height: f64) -> Vec<MediaCardRects> {
        let p = self.placed(height);
        let shift = Point::from((0., p.off_y));
        p.media
            .iter()
            .filter_map(|m| {
                let rect = Rectangle::new(m.origin + shift, m.layout.size);
                p.viewport_visible(rect).then(|| {
                    let controls = std::array::from_fn(|i| {
                        Rectangle::new(
                            m.origin + shift + m.layout.controls[i].loc,
                            m.layout.controls[i].size,
                        )
                    });
                    (self.players[m.player].bus_name.clone(), rect, controls)
                })
            })
            .collect()
    }

    fn visible_interactive_cards(
        &self,
        height: f64,
    ) -> Vec<(u32, Point<f64, Logical>, CardLayout)> {
        let Placed {
            layouts,
            off_y,
            viewport,
            ..
        } = self.placed(height);
        let shift = Point::from((0., off_y));
        let vis = |rect: Rectangle<f64, Logical>| {
            rect.size.h > 0.
                && rect.loc.y < viewport.loc.y + viewport.size.h
                && viewport.loc.y < rect.loc.y + rect.size.h
        };
        let mut out = Vec::new();
        for gl in layouts {
            let group = &self.groups[gl.group];
            match gl.kind {
                GroupKind::Single { origin, layout } => {
                    let po = origin + shift;
                    if vis(Rectangle::new(po, layout.size)) {
                        out.push((group.cards[0].id, po, layout));
                    }
                }
                GroupKind::Expanded { cards, .. } => {
                    for (ci, origin, layout) in cards {
                        let po = origin + shift;
                        if vis(Rectangle::new(po, layout.size)) {
                            out.push((group.cards[ci].id, po, layout));
                        }
                    }
                }
                GroupKind::Collapsed { .. } => {}
            }
        }
        out
    }

    /// Visible groups as `(source key, popover-local bounds, expanded?)` — the
    /// test/introspection view of the grouping (scrolled-off groups omitted).
    fn visible_groups(&self, height: f64) -> Vec<GroupRect> {
        let p = self.placed(height);
        p.layouts
            .iter()
            .filter_map(|gl| {
                let bounds = p.to_popover(gl.bounds);
                p.viewport_visible(bounds).then(|| {
                    let key = self.groups[gl.group].key.clone();
                    let expanded = matches!(gl.kind, GroupKind::Expanded { .. });
                    (key, bounds, expanded)
                })
            })
            .collect()
    }

    /// A collapsed stack's top-card close button, popover-local (`None` unless
    /// that group is a visible collapsed stack).
    fn stack_close_rect(&self, key: &SourceKey, height: f64) -> Option<Rectangle<f64, Logical>> {
        let p = self.placed(height);
        p.layouts.iter().find_map(|gl| {
            if &self.groups[gl.group].key != key {
                return None;
            }
            match &gl.kind {
                GroupKind::Collapsed { origin, top, .. } => {
                    let rect =
                        p.to_popover(Rectangle::new(*origin + top.close.loc, top.close.size));
                    p.viewport_visible(rect).then_some(rect)
                }
                _ => None,
            }
        })
    }

    /// An expanded group's collapse button, popover-local.
    fn group_collapse_rect(&self, key: &SourceKey, height: f64) -> Option<Rectangle<f64, Logical>> {
        let p = self.placed(height);
        p.layouts.iter().find_map(|gl| {
            if &self.groups[gl.group].key != key {
                return None;
            }
            match &gl.kind {
                GroupKind::Expanded { collapse, .. } => {
                    let rect = p.to_popover(*collapse);
                    p.viewport_visible(rect).then_some(rect)
                }
                _ => None,
            }
        })
    }
}

// ---- Events section (`js/ui/dateMenu.js:111`, `_calendar.scss:153-195`) ----

/// Column/`datemenu-displays-box` spacing above the section (`$base_padding`).
const EVENTS_GAP: f64 = 6.;
/// `%card` margin (`$base_margin`, `_common.scss:141`).
const EVENTS_MARGIN: f64 = 4.;
/// `%card` radius (`$base_border_radius * 1.5`).
const EVENTS_CARD_RADIUS: f64 = 12.;
/// `%card` padding (`$scaled_padding * 2`).
const EVENTS_CARD_PAD: f64 = 12.;

/// The `%card` box model shared by the display-section cards (`.events-button`,
/// `.world-clocks-button`).
///
/// The border is the point: `%card` is `border: 1px solid $card_shadow_border_color`, which is
/// **transparent** in the dark theme (`_colors.scss:31`) — but St is border-box, so it still
/// reserves its 2px. Invisible and load-bearing, which is why these cards were 2px short of the
/// live shell (events 68 where GNOME allocates 70) while looking perfectly fine.
///
/// Going through [`ThemeNode::allocation_for`] instead of adding `+ 2.` is what keeps the next
/// card from re-acquiring the same bug: the reserved pixel becomes structural rather than a
/// constant someone has to remember. [`ThemeNode::paint`] already skips an alpha-0 border, so
/// nothing about the drawing changes.
fn display_card_node() -> ThemeNode {
    ThemeNode {
        padding: Edges::uniform(EVENTS_CARD_PAD),
        border: Edges::uniform(1.),
        border_radius: EVENTS_CARD_RADIUS,
        ..ThemeNode::EMPTY
    }
}
/// `.events-title` is `%heading` (11pt); `.event-summary` too; `.event-time` is
/// `%caption` (9pt) (`_calendar.scss:161-184`, `_common.scss:266,280`).
const EVENTS_TITLE_PT: f64 = 11.;
const EVENT_SUMMARY_PT: f64 = 11.;
const EVENT_TIME_PT: f64 = 9.;
/// `.events-title` padding-bottom, `.event-box` spacing, `.events-list` spacing —
/// all `$base_padding` (`_calendar.scss:166,172,177`).
const EVENTS_TITLE_PB: f64 = 6.;
const EVENT_BOX_GAP: f64 = 6.;
const EVENTS_LIST_GAP: f64 = 6.;
/// EN DASH between the parts of an event's time range (`EN_CHAR`, `dateMenu.js`).
const EN_DASH: &str = "\u{2013}";
/// Cap on rows built (and thus shaped) per day. The card clips to the popover, so
/// far more than this can never be seen; a pathological day (thousands of events)
/// must not shape thousands of paragraphs per bake. Divergence: GNOME builds every
/// row into a scrolling list; we render a clipped card, so a hard cap is safe.
const MAX_EVENT_ROWS: usize = 128;

// World Clocks section (`.world-clocks-button`/`.world-clocks-grid`,
// `_calendar.scss:200-236`). The card shares the `%card` box with events
// (`EVENTS_MARGIN`/`EVENTS_CARD_RADIUS`/`EVENTS_CARD_PAD`) and the
// `datemenu-displays-box` gap (`EVENTS_GAP`).
/// `.world-clocks-header` is `%heading` (11pt).
const WC_HEADER_PT: f64 = 11.;
/// `.world-clocks-city` inherits the base body size; `.world-clocks-time` is
/// `%numeric` bold (same size).
const WC_CITY_PT: f64 = 11.;
const WC_TIME_PT: f64 = 11.;
/// `.world-clocks-timezone` is `%numeric %caption` (9pt), muted.
const WC_OFFSET_PT: f64 = 9.;
/// `.world-clocks-grid spacing-rows: $base_padding`.
const WC_ROW_GAP: f64 = 6.;
/// `.world-clocks-grid spacing-columns: $base_padding * 2`.
const WC_COL_GAP: f64 = 12.;

/// A logical text line height (the file's `pt * 1.3` convention, see
/// [`placeholder_centers`]) — now the shared [`crate::ui::line_height_px`], which this
/// file's convention became.
fn line_h(pt: f64) -> f64 {
    crate::ui::line_height_px(pt)
}

/// One formatted event row: the summary over its time string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRow {
    pub summary: String,
    pub time: String,
}

/// The Events section as the compositor formats it for a given day — all
/// clock-dependent formatting happens here (before the widget), so the renderer
/// is a pure function of this model.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventsSectionModel {
    /// `has-calendars` gates visibility (`_sync`, `js/ui/dateMenu.js:326`).
    pub visible: bool,
    /// "Today" / "Yesterday" / "Tomorrow" / a date (`_updateTitle`, `dateMenu.js:172-190`).
    pub title: String,
    /// The day's events, newest sort order; empty → "No Events" placeholder.
    pub rows: Vec<EventRow>,
}

/// Build the section model for `selected` day from the store (`getEvents` +
/// `_updateTitle` + `_formatEventTime`, `js/ui/dateMenu.js`).
pub fn events_section_model(
    store: &CalendarEventStore,
    selected: Ymd,
    now_secs: i64,
    is_24h: bool,
) -> EventsSectionModel {
    let (day_start, day_end) = local_day_bounds(selected);
    let rows = store
        .events_for(day_start, day_end)
        .iter()
        .take(MAX_EVENT_ROWS)
        .map(|e| EventRow {
            summary: e.summary.clone(),
            time: format_event_time(e.start, e.end, day_start, day_end, now_secs, is_24h),
        })
        .collect();
    EventsSectionModel {
        visible: store.has_calendars(),
        title: events_title(selected, now_secs),
        rows,
    }
}

/// The section title (`_updateTitle`, `js/ui/dateMenu.js:172-190`). English only,
/// like the rest of our string scope.
fn events_title(selected: Ymd, now_secs: i64) -> String {
    const DAY: i64 = 86_400;
    let (start, end) = local_day_bounds(selected);
    if start <= now_secs && now_secs < end {
        "Today".to_string()
    } else if end <= now_secs && now_secs - end < DAY {
        "Yesterday".to_string()
    } else if start > now_secs && start - now_secs <= DAY {
        "Tomorrow".to_string()
    } else if selected.year == year_of_secs(now_secs) {
        strftime_ymd(selected, c"%B %-d")
    } else {
        strftime_ymd(selected, c"%B %-d %Y")
    }
}

/// An event's time string for a given day (`_formatEventTime`,
/// `js/ui/dateMenu.js:196-257`). LTR only — the RTL swaps are the repo-wide
/// deferred-RTL divergence.
fn format_event_time(
    start: i64,
    end: i64,
    day_start: i64,
    day_end: i64,
    now_secs: i64,
    is_24h: bool,
) -> String {
    if start == day_start && end == day_end {
        return "All Day".to_string();
    }
    let starts_before = start < day_start;
    let ends_after = end > day_end;
    if starts_before || ends_after {
        // Multi-day: date (+ time unless at midnight) on each side.
        let this_year = year_of_secs(now_secs);
        let starts_mid = is_midnight_secs(start);
        let ends_mid = is_midnight_secs(end);
        // A midnight end displays as the previous *calendar* day — GNOME steps
        // `eventEnd.setDate(getDate() - 1)` (`dateMenu.js:227-231`), a local-date
        // step (DST-safe), NOT a fixed 86400s subtraction.
        let disp_end = if ends_mid {
            add_days(ymd_of_secs(end), -1)
        } else {
            ymd_of_secs(end)
        };
        let use_md = year_of_secs(start) == this_year && this_year == disp_end.year;
        let fmt: &CStr = if use_md { c"%m/%d" } else { c"%x" };
        let start_date = strftime_secs(start, fmt);
        let end_date = strftime_ymd(disp_end, fmt);
        if starts_mid && ends_mid {
            format!("{start_date} {EN_DASH} {end_date}")
        } else {
            // Times come from the *unadjusted* end (`dateMenu.js:208`).
            let start_time = format_time(start, is_24h);
            let end_time = format_time(end, is_24h);
            format!("{start_date} {start_time} {EN_DASH} {end_date} {end_time}")
        }
    } else if start == end {
        // GNOME's `eventStart === eventEnd` compares object identity (always
        // false) — dead code; we compare timestamps, showing just the start.
        format_time(start, is_24h)
    } else {
        format!(
            "{} {EN_DASH} {}",
            format_time(start, is_24h),
            format_time(end, is_24h)
        )
    }
}

/// Locale time-of-day: 24h `%H:%M`, else 12h `%-I:%M %p` (`formatTime`).
fn format_time(secs: i64, is_24h: bool) -> String {
    strftime_secs(secs, if is_24h { c"%H:%M" } else { c"%-I:%M %p" })
}

/// `strftime` of a Unix timestamp in local time.
fn strftime_secs(secs: i64, fmt: &CStr) -> String {
    // SAFETY: localtime returns a pointer into a static buffer; read immediately.
    unsafe {
        let t = secs as libc::time_t;
        let tm = libc::localtime(&t);
        if tm.is_null() {
            return String::new();
        }
        let mut buf = [0u8; 64];
        let n = libc::strftime(
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            fmt.as_ptr(),
            tm,
        );
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }
}

fn year_of_secs(secs: i64) -> i32 {
    // SAFETY: as above.
    unsafe {
        let t = secs as libc::time_t;
        let tm = libc::localtime(&t);
        if tm.is_null() {
            return 1970;
        }
        (*tm).tm_year + 1900
    }
}

fn is_midnight_secs(secs: i64) -> bool {
    // SAFETY: as above.
    unsafe {
        let t = secs as libc::time_t;
        let tm = libc::localtime(&t);
        if tm.is_null() {
            return false;
        }
        let tm = &*tm;
        tm.tm_hour == 0 && tm.tm_min == 0 && tm.tm_sec == 0
    }
}

/// The local calendar date (Y/M/D) a timestamp falls on. Paired with [`add_days`]
/// this gives a DST-safe calendar-day step, matching JS `Date.setDate()`.
fn ymd_of_secs(secs: i64) -> Ymd {
    // SAFETY: localtime returns a pointer into a static buffer; read immediately.
    unsafe {
        let t = secs as libc::time_t;
        let tm = libc::localtime(&t);
        if tm.is_null() {
            return Ymd {
                year: 1970,
                month: 1,
                day: 1,
            };
        }
        let tm = &*tm;
        Ymd {
            year: tm.tm_year + 1900,
            month: (tm.tm_mon + 1) as u32,
            day: tm.tm_mday as u32,
        }
    }
}

/// The dateMenu popover content: the message-list column (left) beside the
/// calendar column (`js/ui/dateMenu.js:917-940`).
pub struct DateMenu {
    pub calendar: Calendar,
    list: CalendarMessageList,
    /// Max popover height (the work area minus margins, set by the popover from
    /// the output). The content grows to its natural height up to this cap; past
    /// it, the message list scrolls — gnome-shell's work-area `max-height`
    /// (`js/ui/panelMenu.js:177-185`). `INFINITY` until set (tests, pre-layout).
    available_h: f64,
    /// The Events section model (title + rows), formatted by the compositor for
    /// the calendar's selected day; empty/hidden until the CalendarServer reports.
    events: EventsSectionModel,
    /// Bumped whenever `events` changes, to key its texture cache.
    events_rev: u64,
    /// The events card is an `St.Button` (`%card:hover` + click launches the
    /// calendar app); tracks its hover wash and revision bit.
    events_button: widget::CardButton,
    /// The events-card texture, cached per scale.
    events_cache: RefCell<TextureCache>,
    /// The World Clocks section model (header + rows), formatted by the compositor
    /// at the current instant; empty/hidden until GNOME Clocks reports locations.
    world_clocks: crate::world_clocks::WorldClocksModel,
    /// Bumped whenever `world_clocks` changes, to key its texture cache.
    world_clocks_rev: u64,
    /// The world-clocks card is an `St.Button` (`%card:hover` + click launches
    /// GNOME Clocks); tracks its hover wash and revision bit.
    world_clocks_button: widget::CardButton,
    /// The world-clocks-card texture, cached per scale.
    world_clocks_cache: RefCell<TextureCache>,
    /// The popover background (rounded box + placeholder label / Clear pill),
    /// cached per scale; the stored revision is 0/1 for empty/non-empty.
    bg_cache: RefCell<TextureCache>,
}

/// The x where the calendar column starts (also the list column's width).
fn calendar_col_x() -> f64 {
    LIST_PAD + list_box_w() + column_gap()
}

impl DateMenu {
    pub fn new(
        week_start: u8,
        show_week_numbers: bool,
        accent: [u8; 3],
        groups: Vec<CardGroup>,
    ) -> Self {
        Self {
            calendar: Calendar::new(week_start, show_week_numbers, accent),
            list: CalendarMessageList::new(groups),
            available_h: f64::INFINITY,
            events: EventsSectionModel::default(),
            events_rev: 0,
            events_button: widget::CardButton::default(),
            events_cache: RefCell::new(TextureCache {
                context: None,
                textures: HashMap::new(),
            }),
            world_clocks: crate::world_clocks::WorldClocksModel::default(),
            world_clocks_rev: 0,
            world_clocks_button: widget::CardButton::default(),
            world_clocks_cache: RefCell::new(TextureCache {
                context: None,
                textures: HashMap::new(),
            }),
            bg_cache: RefCell::new(TextureCache {
                context: None,
                textures: HashMap::new(),
            }),
        }
    }

    /// Adopt a freshly-formatted Events section model. Returns whether it changed.
    pub fn set_events(&mut self, model: EventsSectionModel) -> bool {
        if self.events == model {
            return false;
        }
        self.events = model;
        self.events_rev += 1;
        true
    }

    /// The Events section model (for tests / introspection).
    pub fn events(&self) -> &EventsSectionModel {
        &self.events
    }

    /// Height of one `.event-box` (summary over time, `EVENT_BOX_GAP` between).
    fn event_box_h() -> f64 {
        line_h(EVENT_SUMMARY_PT) + EVENT_BOX_GAP + line_h(EVENT_TIME_PT)
    }

    /// The events card's inner content height (title + its padding-bottom + the
    /// rows, or one placeholder line when empty).
    fn events_content_h(&self) -> f64 {
        let rows_h = if self.events.rows.is_empty() {
            line_h(EVENTS_TITLE_PT) // "No Events" placeholder line
        } else {
            let n = self.events.rows.len() as f64;
            n * Self::event_box_h() + (n - 1.) * EVENTS_LIST_GAP
        };
        line_h(EVENTS_TITLE_PT) + EVENTS_TITLE_PB + rows_h
    }

    /// The events card's outer height (content + padding).
    fn events_card_h(&self) -> f64 {
        display_card_node()
            .allocation_for(Size::from((0., self.events_content_h())))
            .h
    }

    /// The section texture's natural height (card + its margins), clamped to the
    /// space left below the grid. When the column would overflow the popover the
    /// bottom rows clip — the displays-section ScrollView is deferred (see the
    /// module docs).
    fn events_alloc_h(&self) -> f64 {
        if !self.events.visible {
            return 0.;
        }
        let section = self.events_card_h() + 2. * EVENTS_MARGIN;
        let cal_h = self.calendar.logical_size().h;
        let room = (self.available_h - cal_h - EVENTS_GAP).max(0.);
        section.min(room)
    }

    /// The events section's total contribution to the calendar column height
    /// (the gap above it plus the section), 0 when hidden or with no room.
    fn events_height(&self) -> f64 {
        let alloc = self.events_alloc_h();
        if alloc > 0. {
            EVENTS_GAP + alloc
        } else {
            0.
        }
    }

    /// Adopt a freshly-formatted World Clocks section model. Returns whether it changed.
    pub fn set_world_clocks(&mut self, model: crate::world_clocks::WorldClocksModel) -> bool {
        if self.world_clocks == model {
            return false;
        }
        self.world_clocks = model;
        self.world_clocks_rev += 1;
        true
    }

    /// The World Clocks section model (for tests / introspection).
    pub fn world_clocks(&self) -> &crate::world_clocks::WorldClocksModel {
        &self.world_clocks
    }

    /// The world-clocks card's inner content height: the header, then (when there
    /// are clocks) the row-gap and one line per clock, `WC_ROW_GAP` between.
    fn world_clocks_content_h(&self) -> f64 {
        let header = line_h(WC_HEADER_PT);
        if self.world_clocks.rows.is_empty() {
            header
        } else {
            let n = self.world_clocks.rows.len() as f64;
            header + WC_ROW_GAP + n * line_h(WC_TIME_PT) + (n - 1.) * WC_ROW_GAP
        }
    }

    /// The world-clocks card's outer height (content + padding).
    fn world_clocks_card_h(&self) -> f64 {
        display_card_node()
            .allocation_for(Size::from((0., self.world_clocks_content_h())))
            .h
    }

    /// The section texture's natural height (card + margins), clamped to the room
    /// left below the grid AND the events card. Overflow clips (ScrollView deferred).
    fn world_clocks_alloc_h(&self) -> f64 {
        if !self.world_clocks.visible {
            return 0.;
        }
        let section = self.world_clocks_card_h() + 2. * EVENTS_MARGIN;
        let cal_h = self.calendar.logical_size().h;
        // Leave the `.popup-menu-content` bottom padding (`LIST_PAD`) below the card.
        let room =
            (self.available_h - cal_h - self.events_height() - EVENTS_GAP - LIST_PAD).max(0.);
        section.min(room)
    }

    /// The world-clocks section's contribution to the calendar column height (the
    /// gap above it plus the section), 0 when hidden or with no room.
    fn world_clocks_height(&self) -> f64 {
        let alloc = self.world_clocks_alloc_h();
        if alloc > 0. {
            EVENTS_GAP + alloc
        } else {
            0.
        }
    }

    /// The events card rect (popover-local), when the section is shown — clicking
    /// anywhere on it launches the calendar app (`EventsSection.vfunc_clicked`,
    /// `dateMenu.js:300-310`). The card box (inset from the section allocation by
    /// `EVENTS_MARGIN`), sitting one `EVENTS_GAP` below the grid.
    fn events_rect(&self) -> Option<Rectangle<f64, Logical>> {
        let alloc = self.events_alloc_h();
        if alloc <= 0. {
            return None;
        }
        let cal = self.calendar.logical_size();
        let top = cal.h + EVENTS_GAP + EVENTS_MARGIN;
        Some(Rectangle::new(
            Point::from((calendar_col_x() + EVENTS_MARGIN, top)),
            Size::from((
                (cal.w - 2. * EVENTS_MARGIN).max(0.),
                (alloc - 2. * EVENTS_MARGIN).max(0.),
            )),
        ))
    }

    /// The world-clocks card rect (popover-local), when the section is shown —
    /// clicking anywhere on it launches GNOME Clocks (`vfunc_clicked`). This is the
    /// card box (the `%card` margin ring lies outside the button, like GNOME's
    /// `St.Button` hit area), inset from the section allocation by `EVENTS_MARGIN`.
    fn world_clocks_rect(&self) -> Option<Rectangle<f64, Logical>> {
        let alloc = self.world_clocks_alloc_h();
        if alloc <= 0. {
            return None;
        }
        let cal = self.calendar.logical_size();
        let top = cal.h + self.events_height() + EVENTS_GAP + EVENTS_MARGIN;
        Some(Rectangle::new(
            Point::from((calendar_col_x() + EVENTS_MARGIN, top)),
            Size::from((
                (cal.w - 2. * EVENTS_MARGIN).max(0.),
                (alloc - 2. * EVENTS_MARGIN).max(0.),
            )),
        ))
    }

    /// Set the popover's height budget (work area minus margins). The dateMenu
    /// grows to fit its content up to this; beyond it, the message list scrolls.
    pub fn set_available_height(&mut self, available_h: f64) {
        self.available_h = available_h;
    }

    /// The popover grows to the taller column's natural height (the calendar is
    /// fixed; the message list grows with its cards), capped at the available
    /// work-area height — never below the calendar, so it stays fully visible.
    pub fn logical_size(&self) -> Size<f64, Logical> {
        let cal = self.calendar.logical_size();
        // The calendar column is the grid plus the Events and World Clocks sections
        // stacked below it (`datemenu-displays-box`, `dateMenu.js:960-964`), then the
        // `.popup-menu-content` bottom padding so the last card isn't flush with the
        // box edge (the left/right get the same `LIST_PAD` inset, below).
        let column_h = cal.h + self.events_height() + self.world_clocks_height() + LIST_PAD;
        let natural = column_h.max(self.list.natural_height());
        let h = natural.min(self.available_h.max(column_h));
        Size::from((calendar_col_x() + cal.w + LIST_PAD, h))
    }

    /// The popover box corner radius — for the drop shadow behind it.
    pub fn corner_radius(&self) -> f64 {
        BOX_RADIUS
    }

    pub fn list(&self) -> &CalendarMessageList {
        &self.list
    }

    /// Push a fresh store snapshot into the list. Returns whether it changed.
    /// Push a fresh player snapshot into the message list (newest first).
    pub fn set_media_players(&mut self, players: Vec<media_card::MediaCardContent>) -> bool {
        self.list.set_players(players)
    }

    pub fn set_notifications(&mut self, groups: Vec<CardGroup>) -> bool {
        self.list.set_groups(groups)
    }

    /// An album-art decode landed — see [`CalendarMessageList::note_art_decoded`].
    pub fn note_art_decoded(&mut self, source: &ImageSource) -> bool {
        self.list.note_art_decoded(source)
    }

    /// Route a click at content-local `pos`: list hits map to notification
    /// actions; everything else goes to the calendar (all consumed — the
    /// popover stays open, like gnome-shell's grab).
    pub fn pointer_click(&mut self, pos: Point<f64, Logical>) -> PopoverAction {
        let size = self.logical_size();
        if pos.x < calendar_col_x() {
            return match self.list.hit(pos, size.h) {
                Some(ListHit::Close(id)) => PopoverAction::CloseNotification(id),
                Some(ListHit::ToggleExpand(id)) => {
                    // Pure UI state: expand/collapse the body in place.
                    self.list.toggle_body_expanded(id);
                    PopoverAction::Consumed
                }
                Some(ListHit::Action { id, key }) => {
                    PopoverAction::InvokeNotificationAction { id, key }
                }
                Some(ListHit::Body { id, has_default }) => {
                    PopoverAction::ActivateNotification { id, has_default }
                }
                Some(ListHit::ExpandGroup(key)) => {
                    // Fan the stack open (pure UI state, popover stays).
                    self.list.toggle_group(key);
                    PopoverAction::Consumed
                }
                Some(ListHit::CollapseGroup) => {
                    self.list.collapse_group();
                    PopoverAction::Consumed
                }
                Some(ListHit::CloseGroup(ids)) => PopoverAction::CloseNotificationGroup(ids),
                Some(ListHit::Clear) => PopoverAction::ClearNotifications,
                Some(ListHit::MediaControl { bus_name, control }) => {
                    PopoverAction::MediaControl { bus_name, control }
                }
                Some(ListHit::MediaBody(bus_name)) => PopoverAction::RaiseMediaPlayer(bus_name),
                None => PopoverAction::Consumed,
            };
        }
        // A click anywhere on the Events card launches the calendar app and closes
        // the popover (`EventsSection.vfunc_clicked`, `dateMenu.js:300-310`).
        // Divergence: GNOME launches the default `text/calendar` handler resolved
        // via the app system; lacking one, we launch GNOME Calendar (as world-clocks
        // launches GNOME Clocks) and treat "has calendars" as implying it exists.
        if self.events_rect().is_some_and(|r| r.contains(pos)) {
            return PopoverAction::Spawn(vec![
                "gtk-launch".to_string(),
                "org.gnome.Calendar".to_string(),
            ]);
        }
        // A click anywhere on the World Clocks card launches GNOME Clocks and closes
        // the popover (`WorldClocksSection.vfunc_clicked`, `dateMenu.js:376-382`).
        if self.world_clocks_rect().is_some_and(|r| r.contains(pos)) {
            return PopoverAction::Spawn(vec![
                "gtk-launch".to_string(),
                "org.gnome.clocks".to_string(),
            ]);
        }
        self.calendar
            .pointer_click(pos - Point::from((calendar_col_x(), 0.)));
        PopoverAction::Consumed
    }

    /// Update the hovered element from a popover-local pointer position (`None`
    /// clears the hover, e.g. the pointer left the popover). Routes to whichever
    /// column the pointer is over and clears the other's hover. Returns whether
    /// anything changed (so the caller can redraw).
    pub fn pointer_hover(&mut self, pos: Option<Point<f64, Logical>>) -> bool {
        let size = self.logical_size();
        // The events and world-clocks cards are hoverable buttons below the grid;
        // while the pointer is on either, the calendar column gets no hover.
        let over_events = pos.is_some_and(|p| self.events_rect().is_some_and(|r| r.contains(p)));
        let over_wc = pos.is_some_and(|p| self.world_clocks_rect().is_some_and(|r| r.contains(p)));
        let (list_pos, cal_pos) = match pos {
            Some(p) if p.x < calendar_col_x() => (Some(p), None),
            Some(_) if over_events || over_wc => (None, None),
            Some(p) => (None, Some(p - Point::from((calendar_col_x(), 0.)))),
            None => (None, None),
        };
        let mut changed = self.list.hover(list_pos, size.h);
        changed |= self.calendar.hover(cal_pos);
        changed |= self.events_button.set_hovered(over_events);
        changed |= self.world_clocks_button.set_hovered(over_wc);
        changed
    }

    /// A wheel/scroll of `delta` over the popover. Over the message-list column
    /// it scrolls the list (content px); over the calendar column it pages the
    /// month (gnome-shell `Calendar.vfunc_scroll_event`). Returns whether
    /// anything changed (so the caller can redraw).
    pub fn scroll(&mut self, pos: Point<f64, Logical>, delta: f64) -> bool {
        if pos.x < calendar_col_x() {
            self.list.scroll_by(delta, self.logical_size().h)
        } else {
            self.calendar.scroll(delta)
        }
    }

    /// Test hooks: the visible per-card interactive rects (single-card groups +
    /// expanded-group cards), and the Clear pill.
    pub fn card_rects(&self) -> Vec<CardRects> {
        let h = self.logical_size().h;
        self.list
            .visible_interactive_cards(h)
            .into_iter()
            .map(|(id, origin, layout)| {
                let close = Rectangle::new(origin + layout.close.loc, layout.close.size);
                (id, Rectangle::new(origin, layout.size), close)
            })
            .collect()
    }

    /// Test hook: the visible media cards, above the notification groups.
    pub fn media_card_rects(&self) -> Vec<MediaCardRects> {
        self.list.visible_media_cards(self.logical_size().h)
    }

    /// Test hooks: the visible groups `(key, bounds, expanded)`, a collapsed
    /// stack's top-card close, and an expanded group's collapse button.
    pub fn group_rects(&self) -> Vec<GroupRect> {
        self.list.visible_groups(self.logical_size().h)
    }

    pub fn stack_close_rect(&self, key: &SourceKey) -> Option<Rectangle<f64, Logical>> {
        self.list.stack_close_rect(key, self.logical_size().h)
    }

    pub fn group_collapse_rect(&self, key: &SourceKey) -> Option<Rectangle<f64, Logical>> {
        self.list.group_collapse_rect(key, self.logical_size().h)
    }

    pub fn clear_pill_rect(&self) -> Option<Rectangle<f64, Logical>> {
        self.list
            .can_clear()
            .then(|| self.list.clear_rect(self.logical_size().h))
    }

    /// Test hook: a visible card's live expand-caret rect (popover-local);
    /// `None` when the card isn't visible or its caret isn't live.
    pub fn card_expand_rect(&self, id: u32) -> Option<Rectangle<f64, Logical>> {
        let h = self.logical_size().h;
        self.list
            .visible_interactive_cards(h)
            .into_iter()
            .find(|(cid, _, _)| *cid == id)
            .and_then(|(_, origin, layout)| {
                layout
                    .expand
                    .filter(|_| layout.can_expand)
                    .map(|e| Rectangle::new(origin + e.loc, e.size))
            })
    }

    /// Test hook: a visible card's action-button rects (popover-local; empty
    /// unless expanded).
    pub fn card_action_rects(&self, id: u32) -> Vec<Rectangle<f64, Logical>> {
        let h = self.logical_size().h;
        self.list
            .visible_interactive_cards(h)
            .into_iter()
            .find(|(cid, _, _)| *cid == id)
            .map(|(_, origin, layout)| {
                layout
                    .actions
                    .iter()
                    .map(|a| Rectangle::new(origin + a.loc, a.size))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Draw the popover background: the full-size rounded box, plus the
    /// placeholder label (empty) or the Clear pill (non-empty). The calendar
    /// texture composites over the right column in the same bg color.
    fn bg_texture(&self, renderer: &mut VulkanRenderer, scale: f64) -> anyhow::Result<VkTexture> {
        let scale_key = NotNan::new(scale).map_err(|_| anyhow::anyhow!("bad scale"))?;
        // The bg carries the Clear pill, so its hover must re-bake it too.
        let clear_hover = matches!(self.list.hovered, Some(ListHover::Clear));
        // The box grows/shrinks with the list (`logical_size`), so its physical
        // height must be part of the freshness key — otherwise a bg baked at the
        // open-time height stays frozen while the list keeps growing, and the
        // extra cards render below the (stale, too-short) background.
        let height_key = to_physical_precise_round::<i64>(scale, self.logical_size().h).max(0);
        let revision = widget::Revision::new()
            .of(self.list.is_empty())
            .of(self.list.can_clear())
            .of(clear_hover)
            .of(height_key)
            .done();
        let mut cache = self.bg_cache.borrow_mut();
        let context = renderer.context_id();
        if cache.context.as_ref() != Some(&context) {
            cache.textures.clear();
            cache.context = Some(context);
        }
        let fresh = matches!(cache.textures.get(&scale_key), Some((rev, _)) if *rev == revision);
        if !fresh {
            let tex = self.draw_bg(renderer, scale)?;
            cache.textures.insert(scale_key, (revision, tex));
        }
        Ok(cache
            .textures
            .get(&scale_key)
            .map(|(_, t)| t.clone())
            .unwrap())
    }

    fn draw_bg(&self, renderer: &mut VulkanRenderer, scale: f64) -> anyhow::Result<VkTexture> {
        let _span = tracy_client::span!("DateMenu::draw_bg");

        let size = self.logical_size();
        let px = |v: f64| to_physical_precise_round::<i32>(scale, v);
        let phys = Size::<i32, Physical>::from((px(size.w).max(1), px(size.h).max(1)));

        // Shape the text up front (needs `&mut renderer`, before the bake frame opens).
        // `TextShaper` owns the pt → physical-px multiply — no `* scale` on the font sizes.
        let (placeholder_run, clear_run) = {
            let mut shaper = TextShaper::new(renderer, scale);
            let placeholder_run = self
                .list
                .is_empty()
                .then(|| shaper.shape("No Notifications", TextStyle::new(PLACEHOLDER_PT).bold()))
                .transpose()?;
            let clear_run = self
                .list
                .can_clear()
                .then(|| shaper.shape("Clear", TextStyle::new(CLEAR_PT).bold()))
                .transpose()?;
            (placeholder_run, clear_run)
        };

        widget::bake_uncached_sized(renderer, phys, |frame| {
            let mut p = Painter::new(frame, scale, phys);
            // Transparent bg: the shared popover chrome (`PanelPopover::render`) draws the
            // `.popup-menu-content` box fill behind this texture; only the list-column chrome
            // (separator / placeholder / Clear pill) lives here.
            p.clear(TRANSPARENT)?;

            // The 1px separator on the list column's right edge (`.message-list` border-right,
            // `_message-list.scss:8,11`). This layer bakes transparent and composites over the
            // popover box, so the translucent `$borders_color` blends correctly at composite —
            // `Painter::hairline` keeps it crisp (an SDF fill would AA-dim a 1px line away).
            p.hairline(
                Rectangle::new(
                    // On the box's right EDGE — past the list's own padding-right — not at the
                    // content edge. `list_box_w` includes the border itself, hence the -1.
                    Point::from((LIST_PAD + list_box_w() - LIST_BORDER_R, 0.)),
                    Size::from((LIST_BORDER_R, size.h)),
                ),
                widget::style::BORDERS,
            )?;

            if let Some(run) = &placeholder_run {
                // Centered under the (separately composited) 96px icon.
                let (_, cy) = placeholder_centers(size.h);
                p.text(
                    run,
                    Point::from((LIST_PAD + list_w() / 2., cy)),
                    Align::CENTER,
                    PLACEHOLDER_FG,
                )?;
            }

            if let Some(run) = &clear_run {
                let pill = self.list.clear_rect(size.h);
                p.fill_rounded(pill, CLEAR_H / 2., CLEAR_BG)?;
                if matches!(self.list.hovered, Some(ListHover::Clear)) {
                    p.fill_rounded(pill, CLEAR_H / 2., HOVER_WASH)?;
                }
                let center =
                    Point::from((pill.loc.x + pill.size.w / 2., pill.loc.y + pill.size.h / 2.));
                p.text_clipped(run, center, Align::CENTER, TEXT, pill)?;
            }

            Ok(())
        })
    }

    /// All the popover's render elements at `origin`, in output stacking
    /// order (FIRST = topmost): message-list cards / placeholder icon, then
    /// the calendar column, then the background box (carrying the
    /// rounded-corner-aware opaque region) at the bottom.
    /// Draw the Events section into an offscreen texture, cached per (scale,
    /// `events_rev`).
    fn events_texture(
        &self,
        renderer: &mut VulkanRenderer,
        scale: f64,
    ) -> anyhow::Result<VkTexture> {
        let scale_key = NotNan::new(scale).map_err(|_| anyhow::anyhow!("bad scale"))?;
        // The card is clipped to `events_alloc_h()`, which follows the popover cap
        // (`available_h`) — a value that changes on output resize WITHOUT touching
        // `events_rev`. Fold its physical height in so a re-cap re-bakes rather than
        // clipping against a stale height (mirrors `bg_texture`'s `height_key`).
        let height_key = to_physical_precise_round::<i64>(scale, self.events_alloc_h()).max(0);
        let revision = self
            .events_button
            .revision(self.events_rev, height_key as u64);
        let mut cache = self.events_cache.borrow_mut();
        let context = renderer.context_id();
        if cache.context.as_ref() != Some(&context) {
            cache.textures.clear();
            cache.context = Some(context);
        }
        let fresh = matches!(cache.textures.get(&scale_key), Some((rev, _)) if *rev == revision);
        if !fresh {
            let tex = self.draw_events(renderer, scale)?;
            cache.textures.insert(scale_key, (revision, tex));
        }
        Ok(cache
            .textures
            .get(&scale_key)
            .map(|(_, t)| t.clone())
            .unwrap())
    }

    fn draw_events(&self, renderer: &mut VulkanRenderer, scale: f64) -> anyhow::Result<VkTexture> {
        let cal_w = self.calendar.logical_size().w;
        let alloc_h = self.events_alloc_h();
        let phys = Size::<i32, Physical>::from((
            to_physical_precise_round::<i32>(scale, cal_w).max(1),
            to_physical_precise_round::<i32>(scale, alloc_h).max(1),
        ));

        // Shape every run before the bake frame opens (needs `&mut renderer`).
        let (title_run, row_runs, placeholder_run) = {
            let mut shaper = TextShaper::new(renderer, scale);
            let title_run =
                shaper.shape(&self.events.title, TextStyle::new(EVENTS_TITLE_PT).bold())?;
            let placeholder_run = shaper.shape("No Events", TextStyle::new(EVENTS_TITLE_PT))?;
            let mut row_runs = Vec::with_capacity(self.events.rows.len());
            for row in &self.events.rows {
                let summary =
                    shaper.shape(&row.summary, TextStyle::new(EVENT_SUMMARY_PT).bold())?;
                let time = shaper.shape(&row.time, TextStyle::new(EVENT_TIME_PT))?;
                row_runs.push((summary, time));
            }
            (title_run, row_runs, placeholder_run)
        };

        let card_h = self.events_card_h();
        let hovered = self.events_button.hovered();
        widget::bake_uncached_sized(renderer, phys, move |frame| {
            let mut p = Painter::new(frame, scale, phys);
            p.clear(TRANSPARENT)?;

            // The `%card` (bg #47474c, radius 12, padding 12), inset by its margin.
            let card = Rectangle::new(
                Point::<f64, Logical>::from((EVENTS_MARGIN, EVENTS_MARGIN)),
                Size::<f64, Logical>::from((cal_w - 2. * EVENTS_MARGIN, card_h)),
            );
            p.fill_rounded(card, EVENTS_CARD_RADIUS, widget::style::CARD_BG)?;
            // `%card:hover` — a lighten wash over the whole card (the button state).
            widget::CardButton::paint_hover(&mut p, hovered, card, EVENTS_CARD_RADIUS)?;

            // Through `content_box`, so the text starts inside the border the card now
            // reserves (13 in, not 12) — the same 1px the height grew by.
            let content = display_card_node().content_box(card);
            let cx = content.loc.x;
            let mut y = content.loc.y;

            // Title (`.events-title`, muted heading), then its padding-bottom.
            p.text(
                &title_run,
                Point::from((cx, y + line_h(EVENTS_TITLE_PT) / 2.)),
                Align::LEFT_MIDDLE,
                MUTED,
            )?;
            y += line_h(EVENTS_TITLE_PT) + EVENTS_TITLE_PB;

            if row_runs.is_empty() {
                // `.event-placeholder` — muted (italic deferred: no italic face).
                p.text(
                    &placeholder_run,
                    Point::from((cx, y + line_h(EVENTS_TITLE_PT) / 2.)),
                    Align::LEFT_MIDDLE,
                    MUTED,
                )?;
            } else {
                for (i, (summary, time)) in row_runs.iter().enumerate() {
                    if i > 0 {
                        y += EVENTS_LIST_GAP;
                    }
                    // `.event-summary` (heading, full fg) over `.event-time`
                    // (caption, muted).
                    p.text(
                        summary,
                        Point::from((cx, y + line_h(EVENT_SUMMARY_PT) / 2.)),
                        Align::LEFT_MIDDLE,
                        TEXT,
                    )?;
                    y += line_h(EVENT_SUMMARY_PT) + EVENT_BOX_GAP;
                    p.text(
                        time,
                        Point::from((cx, y + line_h(EVENT_TIME_PT) / 2.)),
                        Align::LEFT_MIDDLE,
                        MUTED,
                    )?;
                    y += line_h(EVENT_TIME_PT);
                }
            }
            Ok(())
        })
    }

    /// The world-clocks card texture, cached per scale (keyed by `world_clocks_rev`
    /// and the physical clip height, like the events card).
    fn world_clocks_texture(
        &self,
        renderer: &mut VulkanRenderer,
        scale: f64,
    ) -> anyhow::Result<VkTexture> {
        let scale_key = NotNan::new(scale).map_err(|_| anyhow::anyhow!("bad scale"))?;
        let height_key =
            to_physical_precise_round::<i64>(scale, self.world_clocks_alloc_h()).max(0);
        let revision = self
            .world_clocks_button
            .revision(self.world_clocks_rev, height_key as u64);
        let mut cache = self.world_clocks_cache.borrow_mut();
        let context = renderer.context_id();
        if cache.context.as_ref() != Some(&context) {
            cache.textures.clear();
            cache.context = Some(context);
        }
        let fresh = matches!(cache.textures.get(&scale_key), Some((rev, _)) if *rev == revision);
        if !fresh {
            let tex = self.draw_world_clocks(renderer, scale)?;
            cache.textures.insert(scale_key, (revision, tex));
        }
        Ok(cache
            .textures
            .get(&scale_key)
            .map(|(_, t)| t.clone())
            .unwrap())
    }

    fn draw_world_clocks(
        &self,
        renderer: &mut VulkanRenderer,
        scale: f64,
    ) -> anyhow::Result<VkTexture> {
        let cal_w = self.calendar.logical_size().w;
        let alloc_h = self.world_clocks_alloc_h();
        let phys = Size::<i32, Physical>::from((
            to_physical_precise_round::<i32>(scale, cal_w).max(1),
            to_physical_precise_round::<i32>(scale, alloc_h).max(1),
        ));

        // Shape header + the three cells per row before the bake frame opens.
        let (header_run, row_runs) = {
            let mut shaper = TextShaper::new(renderer, scale);
            let header_run = shaper.shape(
                &self.world_clocks.header,
                TextStyle::new(WC_HEADER_PT).bold(),
            )?;
            let mut row_runs = Vec::with_capacity(self.world_clocks.rows.len());
            for row in &self.world_clocks.rows {
                let city = shaper.shape(&row.city, TextStyle::new(WC_CITY_PT))?;
                let time = shaper.shape(&row.time, TextStyle::new(WC_TIME_PT).bold())?;
                let offset = shaper.shape(&row.tz_offset, TextStyle::new(WC_OFFSET_PT))?;
                row_runs.push((city, time, offset));
            }
            (header_run, row_runs)
        };

        // Column widths: the time and offset columns are as wide as their widest
        // cell; the city column expands into the rest (`world-clocks-grid`).
        let ink_w = |run: &widget::ShapedText| run.ink_bounds().2 as f64 / scale;
        let offset_col_w = row_runs
            .iter()
            .map(|(_, _, o)| ink_w(o))
            .fold(0.0_f64, f64::max);
        let time_col_w = row_runs
            .iter()
            .map(|(_, t, _)| ink_w(t))
            .fold(0.0_f64, f64::max);

        // The header is `$fg_color` (`.no-world-clocks`) when empty, else muted.
        let header_color = if self.world_clocks.empty { TEXT } else { MUTED };
        let card_h = self.world_clocks_card_h();
        let hovered = self.world_clocks_button.hovered();
        widget::bake_uncached_sized(renderer, phys, move |frame| {
            let mut p = Painter::new(frame, scale, phys);
            p.clear(TRANSPARENT)?;

            let card = Rectangle::new(
                Point::<f64, Logical>::from((EVENTS_MARGIN, EVENTS_MARGIN)),
                Size::<f64, Logical>::from((cal_w - 2. * EVENTS_MARGIN, card_h)),
            );
            p.fill_rounded(card, EVENTS_CARD_RADIUS, widget::style::CARD_BG)?;
            // `%card:hover` — a lighten wash over the whole card (the button state).
            widget::CardButton::paint_hover(&mut p, hovered, card, EVENTS_CARD_RADIUS)?;

            // Through `content_box`, so the columns start inside the border the card now
            // reserves (see `display_card_node`).
            let content = display_card_node().content_box(card);
            let inner_left = content.loc.x;
            let inner_right = content.loc.x + content.size.w;
            let mut y = content.loc.y;

            // All cells are centered by their font line box (not ink), so the
            // three side-by-side columns share a baseline regardless of descenders.
            let card_clip = Rectangle::new(card.loc, card.size);

            // Header (`.world-clocks-header`), spanning the city/time columns.
            p.text_band(
                &header_run,
                inner_left,
                widget::HAlign::Left,
                y,
                line_h(WC_HEADER_PT),
                header_color,
                card_clip,
            )?;
            y += line_h(WC_HEADER_PT);

            // The right edges of the offset and time columns.
            let offset_x = inner_right;
            let time_x = inner_right - offset_col_w - WC_COL_GAP;
            let city_clip_right = time_x - time_col_w - WC_COL_GAP;
            let band = line_h(WC_TIME_PT);
            for (city, time, offset) in &row_runs {
                y += WC_ROW_GAP;
                // City (start-aligned, full fg), clipped short of the time column.
                p.text_band(
                    city,
                    inner_left,
                    widget::HAlign::Left,
                    y,
                    band,
                    TEXT,
                    Rectangle::new(
                        Point::from((inner_left, y)),
                        Size::from(((city_clip_right - inner_left).max(0.), band)),
                    ),
                )?;
                // Time (bold, right-aligned) then the muted offset at the far right.
                p.text_band(
                    time,
                    time_x,
                    widget::HAlign::Right,
                    y,
                    band,
                    TEXT,
                    card_clip,
                )?;
                p.text_band(
                    offset,
                    offset_x,
                    widget::HAlign::Right,
                    y,
                    band,
                    MUTED,
                    card_clip,
                )?;
                y += band;
            }
            Ok(())
        })
    }

    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        icons: &IconCache,
        app_icons: &AppIconCache,
        images: &ImageCache,
        scale: f64,
        origin: Point<f64, Logical>,
    ) -> Vec<TextureRenderElement<VkTexture>> {
        let mut elements = Vec::new();
        let size = self.logical_size();

        // The message-list cards, or the placeholder icon when empty.
        if self.list.is_empty() {
            let (icon_cy, _) = placeholder_centers(size.h);
            let center = Point::from((LIST_PAD + list_w() / 2., icon_cy));
            if let Some(tb) = icons.texture(
                renderer,
                "no-notifications-symbolic",
                PLACEHOLDER_ICON,
                scale,
                PLACEHOLDER_FG,
            ) {
                let logical = tb.logical_size();
                let loc = origin + center - Point::from((logical.w / 2., logical.h / 2.));
                elements.push(TextureRenderElement::from_texture_buffer(
                    tb,
                    loc,
                    1.,
                    None,
                    None,
                    Kind::Unspecified,
                ));
            }
        } else {
            elements.extend(
                self.list
                    .render(renderer, icons, app_icons, images, scale, origin, size.h),
            );
        }

        // The calendar column (its own rounded box in the same bg color, so
        // the seam is invisible; its right corners align with the popover's).
        match self.calendar.texture(renderer, scale) {
            Ok(texture) => {
                let buffer = TextureBuffer::from_texture(
                    renderer,
                    texture,
                    scale,
                    Transform::Normal,
                    Vec::new(),
                );
                elements.push(TextureRenderElement::from_texture_buffer(
                    buffer,
                    origin + Point::from((calendar_col_x(), 0.)),
                    1.,
                    None,
                    None,
                    Kind::Unspecified,
                ));
            }
            Err(err) => {
                tracing::error!("error drawing the calendar popover: {err:#}");
            }
        }

        // The Events section card, below the grid in the calendar column
        // (`datemenu-displays-box`, `js/ui/dateMenu.js:960`).
        if self.events_height() > 0. {
            let cal_h = self.calendar.logical_size().h;
            match self.events_texture(renderer, scale) {
                Ok(texture) => {
                    let buffer = TextureBuffer::from_texture(
                        renderer,
                        texture,
                        scale,
                        Transform::Normal,
                        Vec::new(),
                    );
                    elements.push(TextureRenderElement::from_texture_buffer(
                        buffer,
                        origin + Point::from((calendar_col_x(), cal_h + EVENTS_GAP)),
                        1.,
                        None,
                        None,
                        Kind::Unspecified,
                    ));
                }
                Err(err) => {
                    tracing::error!("error drawing the events section: {err:#}");
                }
            }
        }

        // The World Clocks section card, below the Events card in the calendar
        // column (`datemenu-displays-box`, `js/ui/dateMenu.js:963`).
        if self.world_clocks_height() > 0. {
            let cal_h = self.calendar.logical_size().h;
            let top = cal_h + self.events_height() + EVENTS_GAP;
            match self.world_clocks_texture(renderer, scale) {
                Ok(texture) => {
                    let buffer = TextureBuffer::from_texture(
                        renderer,
                        texture,
                        scale,
                        Transform::Normal,
                        Vec::new(),
                    );
                    elements.push(TextureRenderElement::from_texture_buffer(
                        buffer,
                        origin + Point::from((calendar_col_x(), top)),
                        1.,
                        None,
                        None,
                        Kind::Unspecified,
                    ));
                }
                Err(err) => {
                    tracing::error!("error drawing the world clocks section: {err:#}");
                }
            }
        }

        // The list-column chrome (separator / placeholder / Clear pill) on a transparent bg, at
        // the bottom of the content stack. Reports NO opaque region: the `.popup-menu-content`
        // box fill (and its rounded opaque region) is now drawn by the shared popover chrome
        // behind this texture (`PanelPopover::render`).
        match self.bg_texture(renderer, scale) {
            Ok(texture) => {
                let buffer = TextureBuffer::from_texture(
                    renderer,
                    texture,
                    scale,
                    Transform::Normal,
                    Vec::new(),
                );
                elements.push(TextureRenderElement::from_texture_buffer(
                    buffer,
                    origin,
                    1.,
                    None,
                    None,
                    Kind::Unspecified,
                ));
            }
            Err(err) => {
                tracing::error!("error drawing the calendar popover background: {err:#}");
            }
        }

        elements
    }
}

/// The placeholder's vertical centers `(icon_cy, label_cy)`: the 96px icon
/// over the label with a 12px gap, the stack centered in the column.
fn placeholder_centers(height: f64) -> (f64, f64) {
    // Through the shared line box, not a second copy of the old factor: `placeholder_px` is
    // already px, so it skips the pt conversion.
    let label_h = synoik_vk::text::line_box_px(placeholder_px());
    let total = PLACEHOLDER_ICON + PLACEHOLDER_GAP + label_h;
    let top = (height - total) / 2.;
    (
        top + PLACEHOLDER_ICON / 2.,
        top + PLACEHOLDER_ICON + PLACEHOLDER_GAP + label_h / 2.,
    )
}

// ---- libc-backed date math ----

fn is_leap(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

pub fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(year) {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

/// Add `delta` months to (year, month), returning the normalized month.
pub fn add_months(year: i32, month: u32, delta: i32) -> (i32, u32) {
    let idx = year * 12 + (month as i32 - 1) + delta;
    (idx.div_euclid(12), (idx.rem_euclid(12) + 1) as u32)
}

fn tm_for(date: Ymd) -> libc::tm {
    // SAFETY: a zeroed `tm` is valid; we set the fields timegm needs.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = date.year - 1900;
    tm.tm_mon = date.month as i32 - 1;
    tm.tm_mday = date.day as i32;
    tm.tm_hour = 12;
    tm.tm_isdst = -1;
    tm
}

/// The weekday of a date, 0=Sunday..6=Saturday. TZ-independent (via `timegm`).
pub fn weekday(date: Ymd) -> u32 {
    let mut tm = tm_for(date);
    // SAFETY: tm is a valid, populated struct; timegm normalizes it in place.
    unsafe { libc::timegm(&mut tm) };
    tm.tm_wday as u32
}

/// A date shifted by `delta` days, normalized across month/year boundaries.
pub fn add_days(date: Ymd, delta: i64) -> Ymd {
    let mut tm = tm_for(date);
    tm.tm_mday += delta as i32;
    // SAFETY: timegm normalizes the out-of-range mday in place.
    unsafe { libc::timegm(&mut tm) };
    Ymd {
        year: tm.tm_year + 1900,
        month: (tm.tm_mon + 1) as u32,
        day: tm.tm_mday as u32,
    }
}

/// ISO 8601 week number of a date (`strftime %V`).
fn iso_week(date: Ymd) -> u32 {
    strftime_ymd(date, c"%V").parse().unwrap_or(0)
}

/// Today's local date.
fn today() -> Ymd {
    // SAFETY: localtime returns a pointer into a static buffer; read immediately.
    unsafe {
        let now = libc::time(null_mut());
        let tm = libc::localtime(&now);
        if tm.is_null() {
            return Ymd {
                year: 1970,
                month: 1,
                day: 1,
            };
        }
        let tm = &*tm;
        Ymd {
            year: tm.tm_year + 1900,
            month: (tm.tm_mon + 1) as u32,
            day: tm.tm_mday as u32,
        }
    }
}

/// Locale-aware `strftime` of a calendar date (no time-of-day fields needed).
fn strftime_ymd(date: Ymd, fmt: &CStr) -> String {
    let mut tm = tm_for(date);
    // SAFETY: timegm populates wday/yday that some specifiers (%V, %a) need.
    unsafe {
        libc::timegm(&mut tm);
        let mut buf = [0u8; 64];
        let n = libc::strftime(
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            fmt.as_ptr(),
            &tm,
        );
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }
}

/// The locale's abbreviated name for weekday `w` (0=Sunday..6=Saturday).
/// The single-letter grid abbreviation for weekday `w` (0 = Sunday). gnome-shell shows one letter
/// per column ("S M T W T F S"), NOT the 3-char `%a` form (`_getCalendarDayAbbreviation`,
/// `js/ui/calendar.js:53,538`). We take the first character of the localized abbreviated name,
/// which for en yields exactly GNOME's S/M/T/W/T/F/S.
fn weekday_abbrev(w: u32) -> String {
    // 2023-01-01 was a Sunday; add w days to land on the wanted weekday.
    let date = add_days(
        Ymd {
            year: 2023,
            month: 1,
            day: 1,
        },
        w as i64,
    );
    let abbrev = strftime_ymd(date, c"%a");
    abbrev.chars().next().map(String::from).unwrap_or(abbrev)
}

#[cfg(test)]
mod tests {
    use smithay::backend::allocator::Fourcc;
    use smithay::utils::Buffer as BufferCoord;

    use super::*;

    fn file_source(path: &str) -> ImageSource {
        ImageSource::File(std::path::PathBuf::from(path))
    }

    fn player_with_art(bus: &str, art: Option<&str>) -> media_card::MediaCardContent {
        media_card::MediaCardContent {
            bus_name: bus.to_owned(),
            source_title: "Rhythmbox".into(),
            source_icon: None,
            title: "So What".into(),
            body: "Miles Davis".into(),
            playing: true,
            can_go_next: false,
            can_go_previous: false,
            art: art.map(|a| ImageSource::File(std::path::PathBuf::from(a))),
        }
    }

    /// An album-art decode landing has to invalidate the card that drew the fallback while it was
    /// in flight. The list's cache keys are positional and revision-scoped — nothing hashes the
    /// content — so the arrival is invisible unless it bumps the revision, and the first frame's
    /// themed plate would stay baked in until some unrelated change moved it.
    ///
    /// It must bump for a path a card is *showing* and for nothing else: every bump re-bakes the
    /// whole list, and app-icon decodes land on this same hook.
    #[test]
    fn an_album_art_decode_invalidates_only_the_list_showing_it() {
        let mut list = CalendarMessageList::new(Vec::new());
        assert!(list.set_players(vec![
            player_with_art("org.mpris.MediaPlayer2.a", Some("/tmp/cover-a.png")),
            player_with_art("org.mpris.MediaPlayer2.b", None),
        ]));
        let before = list.revision;

        assert!(
            !list.note_art_decoded(&file_source("/tmp/some-other-cover.png")),
            "an unrelated decode must not re-bake the list"
        );
        assert_eq!(list.revision, before);

        assert!(list.note_art_decoded(&file_source("/tmp/cover-a.png")));
        assert!(
            list.revision > before,
            "the shown cover's decode must invalidate the cards"
        );

        // And the eviction hook sees exactly the covers on screen — the player without art
        // contributes nothing to keep alive.
        let live: Vec<_> = list.art_sources().cloned().collect();
        assert_eq!(live, [file_source("/tmp/cover-a.png")]);
    }

    #[test]
    fn days_in_month_handles_leap_years() {
        assert_eq!(days_in_month(2023, 2), 28);
        assert_eq!(days_in_month(2024, 2), 29); // leap
        assert_eq!(days_in_month(2000, 2), 29); // /400
        assert_eq!(days_in_month(1900, 2), 28); // /100 not /400
        assert_eq!(days_in_month(2023, 4), 30);
        assert_eq!(days_in_month(2023, 12), 31);
    }

    #[test]
    fn weekday_is_known_good() {
        // 2024-01-01 was a Monday (=1), 2023-01-01 a Sunday (=0).
        assert_eq!(
            weekday(Ymd {
                year: 2024,
                month: 1,
                day: 1
            }),
            1
        );
        assert_eq!(
            weekday(Ymd {
                year: 2023,
                month: 1,
                day: 1
            }),
            0
        );
    }

    #[test]
    fn add_days_and_months_normalize() {
        assert_eq!(
            add_days(
                Ymd {
                    year: 2024,
                    month: 2,
                    day: 28
                },
                1
            ),
            Ymd {
                year: 2024,
                month: 2,
                day: 29
            } // leap
        );
        assert_eq!(
            add_days(
                Ymd {
                    year: 2023,
                    month: 12,
                    day: 31
                },
                1
            ),
            Ymd {
                year: 2024,
                month: 1,
                day: 1
            }
        );
        assert_eq!(add_months(2024, 1, -1), (2023, 12));
        assert_eq!(add_months(2024, 12, 1), (2025, 1));
    }

    #[test]
    fn grid_starts_on_the_week_start_column_and_covers_the_month() {
        // January 2024: the 1st is a Monday. With week_start=Sunday(0), the 1st
        // sits in column 1, so the grid opens on Sunday 2023-12-31.
        let cal = Calendar {
            year: 2024,
            month: 1,
            selected: Ymd {
                year: 2024,
                month: 1,
                day: 1,
            },
            today: Ymd {
                year: 2024,
                month: 1,
                day: 1,
            },
            week_start: 0,
            show_week_numbers: false,
            accent: [0., 0., 0., 1.],
            hovered: None,
            revision: 0,
            cache: RefCell::new(TextureCache {
                context: None,
                textures: HashMap::new(),
            }),
        };
        let grid = cal.grid();
        assert_eq!(
            grid[0],
            Ymd {
                year: 2023,
                month: 12,
                day: 31
            },
            "week_start=Sunday must open the grid on the Sunday before the 1st"
        );
        // Column 1 (Monday) is the 1st.
        assert_eq!(
            grid[1],
            Ymd {
                year: 2024,
                month: 1,
                day: 1
            }
        );
        // 42 consecutive days.
        assert_eq!(grid[41], add_days(grid[0], 41));
        // Every day of the month appears in-grid.
        for day in 1..=days_in_month(2024, 1) {
            assert!(grid.iter().any(|d| d.month == 1 && d.day == day));
        }
    }

    #[test]
    fn week_start_monday_rotates_the_grid() {
        // Same month, week_start=Monday(1): the 1st (a Monday) is column 0.
        let mut cal = Calendar::new(1, false, [0, 0, 0]);
        cal.year = 2024;
        cal.month = 1;
        let grid = cal.grid();
        assert_eq!(
            grid[0],
            Ymd {
                year: 2024,
                month: 1,
                day: 1
            },
            "week_start=Monday must put a Monday-1st in column 0"
        );
    }

    #[test]
    fn clicking_pagers_and_days_updates_state() {
        let mut cal = Calendar::new(0, false, [0, 0, 0]);
        cal.year = 2024;
        cal.month = 6;
        cal.revision = 0;
        let layout = Layout::new(false);

        // Next-month pager.
        let next = layout.next_arrow();
        let hit = cal.pointer_click(Point::from((
            next.loc.x + next.size.w / 2.,
            next.loc.y + next.size.h / 2.,
        )));
        assert!(hit);
        assert_eq!((cal.year, cal.month), (2024, 7));

        // Prev twice → back to May.
        let prev = layout.prev_arrow();
        let p = Point::from((prev.loc.x + prev.size.w / 2., prev.loc.y + prev.size.h / 2.));
        cal.pointer_click(p);
        cal.pointer_click(p);
        assert_eq!((cal.year, cal.month), (2024, 5));

        // Clicking a day cell selects that date.
        let before = cal.revision;
        let cell = layout.cell(2, 3);
        cal.pointer_click(Point::from((
            cell.loc.x + cell.size.w / 2.,
            cell.loc.y + cell.size.h / 2.,
        )));
        assert!(cal.revision > before);
    }

    /// The popover's two columns add up to what the live shell allocates across.
    ///
    /// GNOME's `.datemenu-popover` is 793 wide: `1 border + 6 .popup-menu-content padding + 4
    /// #calendarArea padding` on each side, around `432 .message-list + 4 its margin-right + 6 the
    /// column's margin-left + 329 column`. Our size is measured from inside the popover border
    /// (the chrome adds it), so the target here is 791.
    ///
    /// The subtle one is `.message-list`: `width: 29em` is a **content** width, and St adds the
    /// `padding-right: 6` and `border-right: 1` on top — so the box is 432, not 425. Reading it as
    /// the box width made the popover 15 short *and* the cards 6 narrow, because the same 6px was
    /// then also subtracted inside as scrollbar room.
    #[test]
    fn popover_columns_match_the_live_shell() {
        assert_eq!(list_w(), 425., ".message-list content is 29em");
        assert_eq!(
            list_box_w(),
            432.,
            ".message-list box adds padding-right 6 and a 1px border"
        );
        assert_eq!(
            card_w(),
            413.,
            ".message is 413 wide (content less .message-view's 12)"
        );
        assert_eq!(
            column_gap(),
            10.,
            "list margin-right 4 + column margin-left 6"
        );

        let dm = DateMenu::new(0, true, [0, 0, 0], vec![]);
        assert_eq!(
            dm.logical_size().w,
            791.,
            "10 + 432 + 10 + 329 + 10, inside the popover's own border"
        );
    }

    /// The calendar column matches what a live GNOME 50.3 shell allocates, box for box.
    ///
    /// From a mapped actor dump at the default font: `.datemenu-today-button` 321x64 and
    /// `.calendar` 321x309, each a `%card` with its own transparent 1px border and 4px margin,
    /// separated by 10 (the today card's bottom margin plus the column's 6px spacing — `.calendar`
    /// has `margin-top: 0`). The column that contains them is 329 wide, 391 tall.
    ///
    /// Ours was 358x406: the day cell was `em(3.0)` of the *base* 11pt font (44) where GNOME's
    /// `3em` is of the cell's own 9pt font (36 + 2px margins = 40), and the header, weekday band
    /// and week column were each short as well. Every number here is an allocation read off the
    /// shell, not a re-derivation of our own constants — the assertion this replaced rebuilt
    /// `logical_size` from the same constants it was checking, so it could not fail.
    #[test]
    #[cfg_attr(
        not(feature = "reference-env"),
        ignore = "measures shaped text, so it needs the reference font stack; \
run it with --features reference-env, as the fedora CI job does"
    )]
    fn calendar_matches_the_live_shell() {
        let layout = Layout::new(true);

        assert_eq!(
            layout.today_button().size,
            Size::from((321., 64.)),
            ".datemenu-today-button is 321x64 (1 + 9 + 19 + 25 + 9 + 1 tall)"
        );

        let card = layout.calendar_card();
        assert_eq!(
            card.size,
            Size::from((321., 309.)),
            ".calendar is 321x309 (1 + 38 + 29 + 6*40 + 1 tall, 1 + 39 + 7*40 + 1 wide)"
        );
        assert_eq!(
            card.loc.y - (layout.today_button().loc.y + layout.today_button().size.h),
            10.,
            "the cards are 10 apart: the today card's 4px bottom margin plus 6px column spacing"
        );

        assert_eq!(
            cell(),
            40.,
            "day-cell pitch is 3em at 9pt (36) plus 2px margins"
        );
        assert_eq!(
            Calendar::new(0, true, [0, 0, 0]).logical_size(),
            Size::from((329., 391.)),
            "the column is both card boxes plus their margins"
        );
    }

    #[test]
    fn today_button_returns_to_today_only_when_off_today() {
        // A fresh calendar is already on today: clicking the card is a no-op (no revision bump).
        let mut cal = Calendar::new(0, false, [0, 0, 0]);
        cal.revision = 0;
        let layout = Layout::new(false);
        let card = layout.today_button();
        let center = Point::from((card.loc.x + card.size.w / 2., card.loc.y + card.size.h / 2.));
        assert!(cal.pointer_click(center), "the card is a hit region");
        assert_eq!(
            cal.revision, 0,
            "clicking the card while on today's month changes nothing"
        );
        assert_eq!(cal.selected, cal.today);

        // Page away WITHOUT selecting another day: `selected` stays today but the view shows a
        // different month (our divergence — paging keeps the selection). Clicking the card must
        // still snap the *view* back, even though the selection never moved.
        let next = layout.next_arrow();
        let next_c = Point::from((next.loc.x + next.size.w / 2., next.loc.y + next.size.h / 2.));
        cal.pointer_click(next_c);
        assert_eq!(cal.selected, cal.today, "paging keeps the selection put");
        assert_ne!(
            (cal.year, cal.month),
            (cal.today.year, cal.today.month),
            "paged off today's month"
        );
        let before = cal.revision;
        assert!(cal.pointer_click(center));
        assert_eq!(
            (cal.year, cal.month),
            (cal.today.year, cal.today.month),
            "the card snaps the view back to today's month"
        );
        assert!(
            cal.revision > before,
            "snapping the view back bumps the revision"
        );

        // Page away AND select a different day, then click the card: selection + view both return.
        cal.pointer_click(next_c);
        let cell = layout.cell(3, 3);
        cal.pointer_click(Point::from((
            cell.loc.x + cell.size.w / 2.,
            cell.loc.y + cell.size.h / 2.,
        )));
        assert_ne!(cal.selected, cal.today, "moved off today");
        let before = cal.revision;
        assert!(cal.pointer_click(center));
        assert_eq!(
            cal.selected, cal.today,
            "the card returns the selection to today"
        );
        assert_eq!((cal.year, cal.month), (cal.today.year, cal.today.month));
        assert!(
            cal.revision > before,
            "returning to today bumps the revision"
        );
    }

    #[test]
    fn draws_the_calendar_with_glyph_coverage() {
        use smithay::backend::renderer::{Bind, ExportMem, Texture as _};

        let mut vk = match VulkanRenderer::new() {
            Ok(r) => r,
            Err(e) => {
                eprintln!(
                    "skipping draws_the_calendar_with_glyph_coverage: no Vulkan device ({e})"
                );
                return;
            }
        };
        // A vivid accent so the today disc is unmistakable.
        let cal = Calendar::new(0, true, [0xff, 0x00, 0x00]);
        let mut tex = cal.draw(&mut vk, 1.).expect("calendar texture");
        let size = tex.size();
        assert!(size.w > 0 && size.h > 0);

        let fb = vk.bind(&mut tex).expect("bind for readback");
        let region = Rectangle::<i32, BufferCoord>::from_size(size);
        let mapping = vk
            .copy_framebuffer(&fb, region, Fourcc::Abgr8888)
            .expect("copy_framebuffer");
        let pixels = vk.map_texture(&mapping).expect("map_texture").to_vec();

        let px_at = |x: i32, y: i32| {
            let i = ((y * size.w + x) * 4) as usize;
            [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
        };
        // The calendar column bakes with a TRANSPARENT bg now — the shared popover chrome
        // (`PanelPopover::render`) draws the `.popup-menu-content` box fill behind it. Sample the
        // left-edge padding (empty, mid-height): fully transparent.
        let interior = px_at(3, size.h / 2);
        assert_eq!(
            interior[3], 0,
            "calendar column must be transparent (chrome draws the bg), got {interior:?}"
        );

        // Bright glyph ink (day numbers / header).
        let bright = pixels
            .chunks_exact(4)
            .filter(|p| p[0] > 150 && p[1] > 150 && p[2] > 150)
            .count();
        assert!(bright > 60, "expected visible day glyphs, got {bright}");

        // The today disc paints red (accent) pixels: high R, low G/B, opaque.
        let accent = pixels
            .chunks_exact(4)
            // Abgr8888 in memory is [A,B,G,R]? render reads as RGBA; the readback
            // is Abgr8888 mapped to RGBA bytes, so p[0]=R here as elsewhere.
            .filter(|p| p[0] > 150 && p[1] < 80 && p[2] < 80 && p[3] > 150)
            .count();
        assert!(accent > 20, "expected the accent today-disc, got {accent}");
    }

    // ---- The dateMenu two-column popover (message list + calendar) ----

    fn sample_card(id: u32) -> CardContent {
        CardContent {
            id,
            source_title: "App".to_owned(),
            source_icon: None,
            source_app_icon: None,
            title: format!("title {id}"),
            body: "body".to_owned(),
            icon: None,
            actions: Vec::new(),
            has_default_action: false,
            critical: false,
            time_text: "Just now".to_owned(),
        }
    }

    fn center(rect: Rectangle<f64, Logical>) -> Point<f64, Logical> {
        Point::from((rect.loc.x + rect.size.w / 2., rect.loc.y + rect.size.h / 2.))
    }

    /// A one-notification group (renders as a plain card), keyed distinctly by
    /// the card id so each is its own source.
    fn single_group(card: CardContent) -> CardGroup {
        CardGroup {
            key: SourceKey::PidName(card.id, "App".to_owned()),
            source_title: card.source_title.clone(),
            source_icon: card.source_icon.clone(),
            has_urgent: card.critical,
            cards: vec![card],
        }
    }

    /// A multi-notification group under one source (a fanned stack).
    fn multi_group(pid: u32, cards: Vec<CardContent>) -> CardGroup {
        CardGroup {
            key: SourceKey::PidName(pid, "App".to_owned()),
            source_title: "App".to_owned(),
            source_icon: None,
            has_urgent: cards.iter().any(|c| c.critical),
            cards,
        }
    }

    /// An expanded card shows its full (≤`EXPAND_LINES`) wrap regardless of the
    /// popover height — the list scrolls to reach overflow, so there is no
    /// height-dependent clamp any more.
    #[test]
    fn message_list_expansion_shows_full_wrap_regardless_of_height() {
        let mut card = sample_card(1);
        card.body = "word ".repeat(120).trim_end().to_owned();
        card.actions = vec![("ok".to_owned(), "OK".to_owned())];
        let mut list = CalendarMessageList::new(vec![single_group(card)]);
        list.toggle_body_expanded(1);

        // The expanded layout is identical at a roomy and a tight height (the
        // body wraps to the full EXPAND_LINES budget either way).
        let roomy = list.visible_interactive_cards(400.);
        let tight = list.visible_interactive_cards(150.);
        assert_eq!(roomy.len(), 1);
        assert_eq!(tight.len(), 1);
        assert!(roomy[0].2.expanded && tight[0].2.expanded);
        assert_eq!(
            roomy[0].2.body_lines.len(),
            notification_card::EXPAND_LINES,
            "a long body expands to the full line budget"
        );
        assert_eq!(
            roomy[0].2.size.h, tight[0].2.size.h,
            "the expansion no longer depends on the popover height"
        );
    }

    #[test]
    fn date_menu_is_two_columns_list_first() {
        // The message-list column comes FIRST (left in LTR), the calendar
        // second (`js/ui/dateMenu.js:917-940`); the popover keeps the
        // calendar's height.
        let dm = DateMenu::new(0, false, [0, 0, 0], vec![single_group(sample_card(1))]);
        let cal = dm.calendar.logical_size();
        let size = dm.logical_size();
        // Width/height include the `.popup-menu-content` padding on the right/bottom
        // (`LIST_PAD`); the single-card list is shorter than the calendar column.
        assert_eq!(size.w, calendar_col_x() + cal.w + LIST_PAD);
        assert_eq!(size.h, cal.h + LIST_PAD);
        assert!(
            calendar_col_x() > 29. * list_em(),
            "the list column is 29em wide plus its margins"
        );
    }

    #[test]
    fn date_menu_routes_clicks_to_list_and_calendar() {
        let mut dm = DateMenu::new(
            0,
            false,
            [0, 0, 0],
            vec![single_group(sample_card(1)), single_group(sample_card(2))],
        );

        // A calendar-column click still works through the composition: the
        // next-month pager is at the calendar's own coordinates shifted by
        // the list column.
        let before = (dm.calendar.year, dm.calendar.month);
        let next = Layout::new(false).next_arrow();
        let action =
            dm.pointer_click(center(next) + Point::<f64, Logical>::from((calendar_col_x(), 0.)));
        assert_eq!(action, PopoverAction::Consumed);
        assert_ne!(
            (dm.calendar.year, dm.calendar.month),
            before,
            "the pager click must reach the calendar"
        );

        // Card hits: close button, then body (with and without a default
        // action), then the Clear pill.
        let rects = dm.card_rects();
        assert_eq!(rects.len(), 2);
        let (id0, card0, close0) = rects[0];
        assert_eq!(id0, 1, "cards render in snapshot order");
        assert_eq!(
            dm.pointer_click(center(close0)),
            PopoverAction::CloseNotification(1)
        );
        // A body click low in the card (away from the close button).
        let body_pos = Point::from((card0.loc.x + 30., card0.loc.y + card0.size.h - 10.));
        assert_eq!(
            dm.pointer_click(body_pos),
            PopoverAction::ActivateNotification {
                id: 1,
                has_default: false
            }
        );
        let mut with_default = sample_card(3);
        with_default.has_default_action = true;
        assert!(dm.set_notifications(vec![single_group(with_default)]));
        let (_, card, _) = dm.card_rects()[0];
        assert_eq!(
            dm.pointer_click(Point::from((
                card.loc.x + 30.,
                card.loc.y + card.size.h - 10.
            ))),
            PopoverAction::ActivateNotification {
                id: 3,
                has_default: true
            }
        );
        let pill = dm
            .clear_pill_rect()
            .expect("non-empty list has a Clear pill");
        assert_eq!(
            dm.pointer_click(center(pill)),
            PopoverAction::ClearNotifications
        );

        // Empty list-column space is consumed without an action (the popover
        // stays open).
        let dead = Point::from((LIST_PAD + 5., dm.logical_size().h - 120.));
        assert_eq!(dm.pointer_click(dead), PopoverAction::Consumed);
    }

    #[test]
    fn date_menu_placeholder_and_snapshot_pushes() {
        let mut dm = DateMenu::new(0, false, [0, 0, 0], Vec::new());
        assert!(dm.list().is_empty());
        assert!(
            dm.clear_pill_rect().is_none(),
            "the Clear pill hides with the placeholder"
        );
        assert!(!dm.set_notifications(Vec::new()), "no change, no redraw");
        assert!(dm.set_notifications(vec![single_group(sample_card(1))]));
        assert_eq!(dm.list().len(), 1);
        assert!(dm.clear_pill_rect().is_some());
        assert!(
            !dm.set_notifications(vec![single_group(sample_card(1))]),
            "an identical snapshot must not invalidate the cards"
        );
    }

    #[test]
    fn message_list_scrolls_to_reveal_overflow() {
        // More single-card groups than fit: the viewport shows a subset, and
        // scrolling reveals the rest (gnome-shell's St.ScrollView).
        let groups: Vec<_> = (1..=10).map(|i| single_group(sample_card(i))).collect();
        let mut list = CalendarMessageList::new(groups);
        let h = 346.;
        assert!(
            list.placed(h).overflowing(),
            "10 cards overflow the popover"
        );

        let top_ids: Vec<u32> = list
            .visible_interactive_cards(h)
            .iter()
            .map(|c| c.0)
            .collect();
        assert!(!top_ids.is_empty());
        assert!(top_ids.len() < 10, "not all 10 fit the viewport at once");
        assert!(
            top_ids.contains(&1),
            "the newest card is visible at the top"
        );
        assert!(
            !top_ids.contains(&10),
            "the last card is initially scrolled off"
        );

        // Scroll to the bottom: the last card becomes visible, the first leaves.
        assert!(list.scroll_by(10_000., h), "a big scroll moves the offset");
        assert!(
            !list.scroll_by(10_000., h),
            "scrolling past the end is clamped (no further move)"
        );
        let bottom_ids: Vec<u32> = list
            .visible_interactive_cards(h)
            .iter()
            .map(|c| c.0)
            .collect();
        assert!(bottom_ids.contains(&10), "scrolling reveals the last card");
        assert!(!bottom_ids.contains(&1), "the first card scrolled away");

        // Scroll back up past the top: clamped to 0, first card visible again.
        assert!(list.scroll_by(-10_000., h));
        assert!(!list.scroll_by(-10_000., h), "clamped at the top");
        assert!(list.visible_interactive_cards(h).iter().any(|c| c.0 == 1));
    }

    /// The popover grows to fit its content, capped at the available work-area
    /// height; past the cap the message list scrolls
    /// (`js/ui/panelMenu.js:177-185`).
    #[test]
    fn date_menu_grows_to_fit_then_caps_at_available_height() {
        let cal_h = Calendar::new(0, false, [0, 0, 0]).logical_size().h;
        let groups: Vec<_> = (1..=10).map(|i| single_group(sample_card(i))).collect();
        let mut dm = DateMenu::new(0, false, [0, 0, 0], groups);

        // Unbounded (the INFINITY default): grow past the calendar to fit the
        // whole list, and at that natural height nothing overflows.
        let grown = dm.logical_size().h;
        assert!(
            grown > cal_h,
            "popover grows past the calendar to fit the list ({grown} vs {cal_h})"
        );
        assert!(
            !dm.list().placed(grown).overflowing(),
            "at its natural height the list fully fits (no scroll)"
        );

        // Capped to the calendar column: the popover clamps at the calendar grid
        // plus the `.popup-menu-content` bottom pad (its floor — the grid can't be
        // clipped) and the list now overflows, so it scrolls.
        let cap = cal_h + LIST_PAD;
        dm.set_available_height(cap);
        assert_eq!(dm.logical_size().h, cap, "clamped to the available height");
        assert!(
            dm.list().placed(cap).overflowing(),
            "past the cap the list scrolls"
        );

        // An empty popover never grows below/above the bare calendar.
        let mut empty = DateMenu::new(0, false, [0, 0, 0], Vec::new());
        empty.set_available_height(10_000.);
        assert_eq!(
            empty.logical_size().h,
            cal_h + LIST_PAD,
            "an empty popover is the calendar height plus the content-box bottom pad"
        );
    }

    /// The popover background re-bakes when the list grows or shrinks — its
    /// physical height must track `logical_size`, never stay frozen at the
    /// height it was first baked at. (Regression: the bg cache keyed only on
    /// empty-vs-nonempty + Clear-hover, so a bg baked at open time stayed short
    /// while notifications kept arriving and the extra cards rendered *below*
    /// the stale, too-short background.)
    #[test]
    fn date_menu_background_tracks_the_growing_and_shrinking_height() {
        use smithay::backend::renderer::Texture as _;

        let mut vk = match VulkanRenderer::new() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping bg-tracks-height: no Vulkan device ({e})");
                return;
            }
        };
        let px = |dm: &DateMenu| to_physical_precise_round::<i32>(1., dm.logical_size().h).max(1);

        // Open with a couple of cards: the bg bakes to this (small) height.
        let mut dm = DateMenu::new(
            0,
            false,
            [0, 0, 0],
            vec![single_group(sample_card(1)), single_group(sample_card(2))],
        );
        let small = px(&dm);
        assert_eq!(
            dm.bg_texture(&mut vk, 1.).expect("bake small").size().h,
            small,
            "the freshly-baked bg matches the popover height"
        );

        // Notifications arrive while open: the popover grows, and re-baking must
        // yield a taller texture (not the cached short one).
        let groups: Vec<_> = (1..=12).map(|i| single_group(sample_card(i))).collect();
        assert!(dm.set_notifications(groups));
        let grown = px(&dm);
        assert!(grown > small, "the popover grew ({grown} vs {small})");
        assert_eq!(
            dm.bg_texture(&mut vk, 1.).expect("bake grown").size().h,
            grown,
            "the bg re-bakes to the grown height, not the stale short one"
        );

        // Notifications are dismissed: the popover shrinks, and the bg must
        // re-bake smaller again.
        assert!(dm.set_notifications(vec![single_group(sample_card(1))]));
        let shrunk = px(&dm);
        assert!(shrunk < grown, "the popover shrank ({shrunk} vs {grown})");
        assert_eq!(
            dm.bg_texture(&mut vk, 1.).expect("bake shrunk").size().h,
            shrunk,
            "the bg re-bakes to the shrunk height"
        );
    }

    /// A scroll over the calendar column pages the month (up→prev, down→next,
    /// `js/ui/calendar.js:560-571`); a scroll over the list column does not.
    #[test]
    fn date_menu_scroll_routes_to_the_right_column() {
        let mut dm = DateMenu::new(0, false, [0, 0, 0], vec![single_group(sample_card(1))]);
        dm.calendar.year = 2024;
        dm.calendar.month = 6;

        let cal_pt = Point::from((calendar_col_x() + 10., 50.));
        assert!(
            dm.scroll(cal_pt, 1.),
            "down over the calendar pages a month"
        );
        assert_eq!((dm.calendar.year, dm.calendar.month), (2024, 7));
        assert!(dm.scroll(cal_pt, -1.), "up over the calendar pages back");
        assert_eq!((dm.calendar.year, dm.calendar.month), (2024, 6));

        // Over the list column, the month must not change.
        let list_pt = Point::from((LIST_PAD + 10., 50.));
        dm.scroll(list_pt, 1.);
        assert_eq!(
            (dm.calendar.year, dm.calendar.month),
            (2024, 6),
            "a list-column scroll must not page the calendar"
        );
    }

    /// A multi-notification group renders as a collapsed fanned stack: one
    /// interactive card (no separate card per notification), a taller block
    /// than a lone card, and up to three visible members
    /// (`js/ui/messageList.js:1314-1350`).
    #[test]
    fn message_list_collapses_a_multi_notification_group() {
        let cards: Vec<_> = (1..=4).map(sample_card).collect();
        let dm = DateMenu::new(0, false, [0, 0, 0], vec![multi_group(7, cards)]);

        // Collapsed: not broken out into interactive cards.
        assert!(
            dm.card_rects().is_empty(),
            "a collapsed stack exposes no per-card rects"
        );
        let groups = dm.group_rects();
        assert_eq!(groups.len(), 1);
        let (_, bounds, expanded) = &groups[0];
        assert!(!expanded);
        // The stack adds the two visible peeks' offsets (10 + 10/1.4) to a lone card, i.e. it is
        // taller but not by a whole card. The lone card's height comes from the box model rather
        // than a literal, so a padding fix does not read as a stacking regression.
        let expected = lone_card_h() + STACK_HEIGHT_OFFSET_LOCAL;
        assert!(
            (bounds.size.h - expected).abs() < 0.01,
            "stack height {} vs {expected}",
            bounds.size.h
        );
    }

    // The two visible peeks add 10 + 10/1.4 to the top card.
    const STACK_HEIGHT_OFFSET_LOCAL: f64 = 10. + 10. / 1.4;

    /// One collapsed `.message` with a body icon: the reference box model
    /// (`_message-list.scss:83,118-120,160`).
    fn lone_card_h() -> f64 {
        use notification_card::{header_band, BODY_ICON, BORDER, PAD};
        BORDER + PAD + header_band() + PAD + BODY_ICON + PAD * 2. + BORDER
    }

    /// Clicking a collapsed stack expands it into a header + vertical list;
    /// the close button in the collapsed state closes the WHOLE group, and the
    /// header collapse button fans it back (`js/ui/messageList.js:1106-1118`).
    #[test]
    fn message_list_group_expand_collapse_and_group_close() {
        // Two cards so the expanded list fits the popover height (the no-scroll
        // list would otherwise drop overflow — a separate, tested divergence).
        let cards: Vec<_> = (1..=2).map(sample_card).collect();
        let key = SourceKey::PidName(7, "App".to_owned());
        let mut dm = DateMenu::new(0, false, [0, 0, 0], vec![multi_group(7, cards)]);

        // The collapsed stack's top-card close closes the whole group.
        let close = dm
            .stack_close_rect(&key)
            .expect("collapsed stack has a close");
        assert_eq!(
            dm.pointer_click(center(close)),
            PopoverAction::CloseNotificationGroup(vec![1, 2])
        );

        // A click elsewhere on the stack expands the group.
        let (_, bounds, _) = dm.group_rects()[0].clone();
        // A point clear of the close button (lower-left of the stack).
        let expand_pt = Point::from((bounds.loc.x + 20., bounds.loc.y + bounds.size.h - 6.));
        assert_eq!(dm.pointer_click(expand_pt), PopoverAction::Consumed);
        let (_, _, expanded) = &dm.group_rects()[0];
        assert!(expanded, "clicking the stack expanded the group");
        // Now both cards are individually interactive.
        assert_eq!(dm.card_rects().len(), 2);

        // The header collapse button fans it back to a stack.
        let collapse = dm
            .group_collapse_rect(&key)
            .expect("expanded group has a collapse button");
        assert_eq!(dm.pointer_click(center(collapse)), PopoverAction::Consumed);
        assert!(
            !dm.group_rects()[0].2,
            "the collapse button re-fanned the stack"
        );
        assert!(dm.card_rects().is_empty());
    }

    /// A group that shrinks to one notification collapses; a later arrival must
    /// show a fresh COLLAPSED stack, not resurrect the earlier expansion
    /// (`js/ui/messageList.js:1170-1173`).
    #[test]
    fn message_list_group_expansion_does_not_resurrect_after_shrink() {
        let key = SourceKey::PidName(7, "App".to_owned());
        let mut list =
            CalendarMessageList::new(vec![multi_group(7, vec![sample_card(1), sample_card(2)])]);
        list.toggle_group(key.clone());
        assert!(list.visible_groups(400.)[0].2, "the group is expanded");

        // Shrink to one notification: renders as a plain card and drops the
        // expansion state entirely.
        assert!(list.set_groups(vec![multi_group(7, vec![sample_card(1)])]));
        assert!(
            list.group_expanded.is_none(),
            "shrink-to-one clears the expanded-group state"
        );

        // A later arrival re-grows the source: a COLLAPSED stack, not a
        // resurrected expansion.
        assert!(list.set_groups(vec![multi_group(7, vec![sample_card(1), sample_card(2)])]));
        assert!(
            !list.visible_groups(400.)[0].2,
            "the re-grown group is collapsed, not expanded"
        );
    }

    /// Collapsing a group un-expands every member's body — gnome-shell's
    /// `collapse()` runs `message.unexpand()` on all members
    /// (`js/ui/messageList.js:988`), so re-expanding shows collapsed bodies.
    #[test]
    fn message_list_collapse_unexpands_member_bodies() {
        let key = SourceKey::PidName(7, "App".to_owned());
        let mut list =
            CalendarMessageList::new(vec![multi_group(7, vec![sample_card(1), sample_card(2)])]);
        list.toggle_group(key.clone());
        list.toggle_body_expanded(1);
        assert!(list.body_expanded.contains(&1));
        list.collapse_group();
        assert!(
            !list.body_expanded.contains(&1),
            "collapsing the group un-expanded the member body"
        );
    }

    /// Clicking the expanded group's header background (off the collapse
    /// button) collapses it — gnome-shell's group-wide gesture fires on any
    /// unclaimed click (`js/ui/messageList.js:879,934-935`).
    #[test]
    fn message_list_header_background_click_collapses_the_group() {
        let mut dm = DateMenu::new(
            0,
            false,
            [0, 0, 0],
            vec![multi_group(7, vec![sample_card(1), sample_card(2)])],
        );
        let (_, bounds, _) = dm.group_rects()[0].clone();
        let expand_pt = Point::from((bounds.loc.x + 20., bounds.loc.y + bounds.size.h - 6.));
        dm.pointer_click(expand_pt);
        assert!(dm.group_rects()[0].2, "the stack expanded");

        // The header row (top of the group), to the left of the collapse
        // button at the far right.
        let (_, hbounds, _) = dm.group_rects()[0].clone();
        let header_pt = Point::from((hbounds.loc.x + 10., hbounds.loc.y + 5.));
        assert_eq!(dm.pointer_click(header_pt), PopoverAction::Consumed);
        assert!(
            !dm.group_rects()[0].2,
            "clicking the header background collapsed the group"
        );
    }

    /// The calendar tracks the hovered region (today card, month-nav arrows,
    /// grid cells) and clears it when the pointer leaves; each change bumps the
    /// revision so the texture re-bakes with the highlight.
    #[test]
    fn calendar_hover_tracks_regions() {
        let mut cal = Calendar::new(0, false, [0, 0, 0]);
        let layout = Layout::new(false);

        let today = layout.today_button();
        let today_pt = today.loc + Point::from((today.size.w / 2., today.size.h / 2.));
        let rev0 = cal.revision();
        assert!(cal.hover(Some(today_pt)));
        assert_eq!(cal.hovered, Some(CalHover::Today));
        assert!(cal.revision() > rev0, "a hover change bumps the revision");
        // Re-hovering the same region is a no-op (no redraw, no revision bump).
        let rev1 = cal.revision();
        assert!(!cal.hover(Some(today_pt)));
        assert_eq!(cal.revision(), rev1);

        let next = layout.next_arrow();
        let next_pt = next.loc + Point::from((next.size.w / 2., next.size.h / 2.));
        assert!(cal.hover(Some(next_pt)));
        assert_eq!(cal.hovered, Some(CalHover::Next));

        let cell = layout.cell(2, 3);
        let cell_pt = cell.loc + Point::from((cell.size.w / 2., cell.size.h / 2.));
        assert!(cal.hover(Some(cell_pt)));
        assert_eq!(cal.hovered, Some(CalHover::Cell(2 * GRID_COLS + 3)));

        assert!(cal.hover(None), "leaving the calendar clears the hover");
        assert_eq!(cal.hovered, None);
    }

    /// The message list highlights a card's buttons (close/caret/action), the
    /// group collapse button, and the Clear pill — but not a card body — and
    /// clears the highlight when the pointer leaves.
    #[test]
    fn message_list_hover_tracks_card_buttons() {
        use notification_card::CardZone;
        let mut card = sample_card(1);
        card.actions = vec![("ok".to_owned(), "OK".to_owned())];
        let mut list =
            CalendarMessageList::new(vec![single_group(card), single_group(sample_card(2))]);
        list.toggle_body_expanded(1);
        let h = 600.;

        let (_, origin, layout) = list
            .visible_interactive_cards(h)
            .into_iter()
            .find(|(id, _, _)| *id == 1)
            .expect("card 1 is visible");

        // The close button: the card is hovered AND the close zone is named.
        let close = layout.close;
        let close_pt = origin + close.loc + Point::from((close.size.w / 2., close.size.h / 2.));
        assert!(list.hover(Some(close_pt), h));
        assert_eq!(
            list.hovered,
            Some(ListHover::Card {
                id: 1,
                zone: Some(CardZone::Close)
            })
        );

        // An action pill: named by its index.
        let action = layout.actions[0];
        let action_pt = origin + action.loc + Point::from((action.size.w / 2., action.size.h / 2.));
        assert!(list.hover(Some(action_pt), h));
        assert_eq!(
            list.hovered,
            Some(ListHover::Card {
                id: 1,
                zone: Some(CardZone::Action(0))
            })
        );

        // The card body: the card is still hovered (it darkens), with no button
        // zone (mid-card, clear of the top-right buttons and the action row).
        let body_pt = origin + Point::from((layout.size.w / 2., layout.size.h / 2.));
        assert!(list.hover(Some(body_pt), h));
        assert_eq!(list.hovered, Some(ListHover::Card { id: 1, zone: None }));

        // The Clear pill highlights.
        let clear = list.clear_rect(h);
        let clear_pt = clear.loc + Point::from((clear.size.w / 2., clear.size.h / 2.));
        assert!(list.hover(Some(clear_pt), h));
        assert_eq!(list.hovered, Some(ListHover::Clear));

        assert!(list.hover(None, h), "leaving the list clears the hover");
        assert_eq!(list.hovered, None);
    }

    /// A content change (a notification arriving/closing) shifts the cards, so a
    /// hover captured against the old layout must clear — otherwise its wash
    /// re-bakes onto whatever card now sits where the old one was, until the
    /// next pointer motion.
    #[test]
    fn message_list_hover_clears_on_content_change() {
        let mut list = CalendarMessageList::new(vec![
            single_group(sample_card(1)),
            single_group(sample_card(2)),
        ]);
        let h = 600.;
        let (_, origin, layout) = list
            .visible_interactive_cards(h)
            .into_iter()
            .find(|(id, _, _)| *id == 1)
            .unwrap();
        let close = layout.close;
        let close_pt = origin + close.loc + Point::from((close.size.w / 2., close.size.h / 2.));
        list.hover(Some(close_pt), h);
        assert!(matches!(list.hovered, Some(ListHover::Card { id: 1, .. })));

        list.set_groups(vec![
            single_group(sample_card(1)),
            single_group(sample_card(2)),
            single_group(sample_card(3)),
        ]);
        assert_eq!(
            list.hovered, None,
            "a content change clears the now-stale hover"
        );
    }

    /// The Clear pill lives in the separately-keyed bg texture, so hovering it
    /// must NOT bump the card revision (which would needlessly re-bake every
    /// card); only card/collapse hovers do.
    #[test]
    fn message_list_clear_hover_does_not_rebake_cards() {
        let mut list = CalendarMessageList::new(vec![single_group(sample_card(1))]);
        let h = 600.;
        let rev0 = list.revision;
        let clear = list.clear_rect(h);
        let clear_pt = clear.loc + Point::from((clear.size.w / 2., clear.size.h / 2.));
        assert!(list.hover(Some(clear_pt), h));
        assert_eq!(list.hovered, Some(ListHover::Clear));
        assert_eq!(
            list.revision, rev0,
            "a Clear-pill hover must not bump the card revision"
        );
    }

    #[test]
    fn day_bounds_span_one_local_day() {
        // The start is strictly before the end in every zone; the span is 24h ±
        // an hour on a DST-transition day (tolerated so the test is TZ-robust).
        let (since, until) = local_day_bounds(Ymd {
            year: 2026,
            month: 3,
            day: 10,
        });
        assert!(since < until);
        assert!(
            (82_800..=90_000).contains(&(until - since)),
            "a local day is ~24h (±DST), got {}",
            until - since
        );
    }

    #[test]
    fn grid_range_covers_the_whole_month_grid() {
        // The 42-cell grid spans six weeks (~42 days, ± an hour at a DST edge)
        // and contains today's own day interval.
        let (since, until) = grid_range_of(2026, 7, 0);
        let span_days = (until - since) / 86_400;
        assert!(
            (41..=43).contains(&span_days),
            "grid spans ~42 days, got {span_days}"
        );
        // July 15 sits inside July's grid.
        let (day_since, day_until) = local_day_bounds(Ymd {
            year: 2026,
            month: 7,
            day: 15,
        });
        assert!(since <= day_since && day_until <= until);
    }

    #[test]
    fn grid_range_tracks_the_displayed_month() {
        // Paging the month shifts the range the CalendarServer is asked to load
        // (`js/ui/calendar.js:748` re-requests on every rebuild).
        let mut cal = Calendar::new(0, false, [0, 0, 0]);
        let r0 = cal.grid_range();
        assert!(cal.scroll(1.0), "a nonzero scroll pages the month");
        let r1 = cal.grid_range();
        assert_ne!(r0, r1, "the next month's grid range differs");
    }

    #[test]
    fn events_title_relative_buckets() {
        // Relative to today() + now, the buckets are TZ-consistent (both sides
        // use the same local zone), so this is host-robust.
        let now = unsafe { libc::time(null_mut()) } as i64;
        let today = today();
        assert_eq!(events_title(today, now), "Today");
        assert_eq!(events_title(add_days(today, -1), now), "Yesterday");
        assert_eq!(events_title(add_days(today, 1), now), "Tomorrow");
        // A day well away from now is a formatted date, not a bucket.
        let far = events_title(add_days(today, 40), now);
        assert!(!["Today", "Yesterday", "Tomorrow"].contains(&far.as_str()) && !far.is_empty());
    }

    #[test]
    fn format_event_time_structure() {
        let day_start = 1_600_000_000i64; // arbitrary; only relative structure matters
        let day_end = day_start + 86_400;
        let now = day_start;
        assert_eq!(
            format_event_time(day_start, day_end, day_start, day_end, now, true),
            "All Day"
        );
        // In-day range → EN DASH between two times.
        let r = format_event_time(
            day_start + 3600,
            day_start + 7200,
            day_start,
            day_end,
            now,
            true,
        );
        assert!(r.contains(EN_DASH), "range uses en dash: {r}");
        // Zero-length in-day → a single time, no dash.
        let z = format_event_time(
            day_start + 3600,
            day_start + 3600,
            day_start,
            day_end,
            now,
            true,
        );
        assert!(
            !z.contains(EN_DASH) && !z.is_empty(),
            "zero-length shows one time: {z}"
        );
        // Ends after today → multi-day, still a dash-joined range.
        let m = format_event_time(
            day_start + 3600,
            day_end + 7200,
            day_start,
            day_end,
            now,
            true,
        );
        assert!(m.contains(EN_DASH), "multi-day uses en dash: {m}");
    }

    #[test]
    fn midnight_end_shows_the_previous_calendar_day() {
        // A multi-day event ending exactly at midnight displays its end as the
        // previous *calendar* day (`dateMenu.js:227-231` steps `setDate(-1)`). We
        // build the bounds through the same libc path the code uses, so the
        // assertion is timezone-agnostic; DST-boundary correctness rides on the
        // calendar-day `add_days` step (not a fixed 86400s), which is what makes
        // this a day-step and not a seconds-step — verified by construction.
        let d = Ymd {
            year: 2026,
            month: 6,
            day: 15,
        };
        let (day_start, day_end) = local_day_bounds(d);
        let end_midnight = local_day_bounds(add_days(d, 2)).0; // 00:00 of D+2
        let now = day_start;
        let s = format_event_time(
            day_start - 3600,
            end_midnight,
            day_start,
            day_end,
            now,
            true,
        );
        // Same year → %m/%d; the shown end date is D+1, never D+2.
        let want = strftime_ymd(add_days(d, 1), c"%m/%d");
        let never = strftime_ymd(add_days(d, 2), c"%m/%d");
        assert!(s.contains(&want), "midnight end shows {want}: {s}");
        assert!(
            !s.contains(&never),
            "midnight end must not show {never}: {s}"
        );
    }

    #[test]
    fn events_section_grows_the_calendar_column() {
        let mut dm = DateMenu::new(0, false, [0, 0, 0], vec![]);
        let base = dm.logical_size().h;
        // A hidden section (no calendars) adds no height.
        assert!(dm.set_events(EventsSectionModel {
            visible: false,
            title: "Today".into(),
            rows: vec![],
        }));
        assert_eq!(dm.logical_size().h, base, "hidden section adds no height");
        // A visible section with rows grows the column.
        assert!(dm.set_events(EventsSectionModel {
            visible: true,
            title: "Today".into(),
            rows: vec![
                EventRow {
                    summary: "Standup".into(),
                    time: "09:00".into(),
                },
                EventRow {
                    summary: "Lunch".into(),
                    time: "12:00".into(),
                },
            ],
        }));
        assert!(
            dm.logical_size().h > base,
            "a visible events section grows the calendar column"
        );
    }

    fn visible_events() -> EventsSectionModel {
        EventsSectionModel {
            visible: true,
            title: "Today".into(),
            rows: vec![EventRow {
                summary: "Standup".into(),
                time: "09:00".into(),
            }],
        }
    }

    #[test]
    fn events_click_launches_calendar() {
        let mut dm = DateMenu::new(0, false, [0, 0, 0], vec![]);
        dm.set_events(visible_events());
        let rect = dm.events_rect().expect("visible section has a rect");
        let center = rect.loc + Point::from((rect.size.w / 2., rect.size.h / 2.));
        assert_eq!(
            dm.pointer_click(center),
            PopoverAction::Spawn(vec!["gtk-launch".into(), "org.gnome.Calendar".into()]),
            "clicking the card launches the calendar app"
        );
    }

    #[test]
    fn events_hover_lights_up_the_card() {
        let mut dm = DateMenu::new(0, false, [0, 0, 0], vec![]);
        dm.set_events(visible_events());
        let rect = dm.events_rect().expect("visible section has a rect");
        let center = rect.loc + Point::from((rect.size.w / 2., rect.size.h / 2.));
        // Moving onto the card sets the hover wash and reports a redraw.
        assert!(dm.pointer_hover(Some(center)), "entering the card redraws");
        assert!(
            dm.events_button.hovered(),
            "pointer over the card is hovered"
        );
        // Idempotent while still inside.
        assert!(!dm.pointer_hover(Some(center)), "staying inside is a no-op");
        // Leaving clears it.
        assert!(dm.pointer_hover(None), "leaving the card redraws");
        assert!(!dm.events_button.hovered(), "pointer gone is not hovered");
    }

    #[test]
    fn events_card_bakes_at_multiple_scales() {
        let mut vk = match VulkanRenderer::new() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping events_card_bakes_at_multiple_scales: no Vulkan device ({e})");
                return;
            }
        };
        let mut dm = DateMenu::new(0, false, [0, 0, 0], vec![]);
        dm.set_events(EventsSectionModel {
            visible: true,
            title: "Today".into(),
            rows: vec![EventRow {
                summary: "Standup".into(),
                time: "09:00 \u{2013} 09:30".into(),
            }],
        });
        for scale in [1.0, 1.5, 2.0] {
            let tex = dm
                .events_texture(&mut vk, scale)
                .expect("events texture bakes");
            let size = smithay::backend::renderer::Texture::size(&tex);
            assert!(size.w > 0 && size.h > 0, "non-empty at scale {scale}");
        }
        // Empty (placeholder) also bakes.
        dm.set_events(EventsSectionModel {
            visible: true,
            title: "Today".into(),
            rows: vec![],
        });
        assert!(dm.events_texture(&mut vk, 2.0).is_ok());
    }

    fn wc_model(visible: bool, rows: usize) -> crate::world_clocks::WorldClocksModel {
        use crate::world_clocks::{ClockRow, WorldClocksModel};
        WorldClocksModel {
            visible,
            header: if rows == 0 {
                "Add World Clocks…"
            } else {
                "World Clocks"
            }
            .into(),
            empty: rows == 0,
            rows: (0..rows)
                .map(|i| ClockRow {
                    city: format!("City {i}"),
                    time: "12:00".into(),
                    tz_offset: "+1".into(),
                })
                .collect(),
        }
    }

    /// The display cards match the heights a live GNOME 50.3 shell allocates.
    ///
    /// Measured from a mapped actor dump at the default font: `.events-button` 321x70 with the
    /// "No Events" placeholder, `.world-clocks-button` 321x145 with four city rows. Both are
    /// `1 + 12 + content + 12 + 1` — the outer 1s being the `%card` border, which is
    /// `transparent` in the dark theme and therefore reserves space without ever being drawn.
    /// That is exactly why these were 2px short and looked right; see [`display_card_node`].
    ///
    /// Driven through the real height entry points rather than re-adding up the constants, so
    /// this fails if the border stops being reserved, not merely if a literal is edited.
    #[test]
    #[cfg_attr(
        not(feature = "reference-env"),
        ignore = "measures shaped text, so it needs the reference font stack; \
run it with --features reference-env, as the fedora CI job does"
    )]
    fn display_cards_match_the_live_shell() {
        let mut dm = DateMenu::new(0, false, [0, 0, 0], vec![]);

        assert_eq!(
            dm.events_card_h(),
            70.,
            "the empty events card is 1 + 12 + 44 + 12 + 1"
        );

        dm.set_world_clocks(wc_model(true, 4));
        assert_eq!(
            dm.world_clocks_card_h(),
            145.,
            "four world-clock rows are 1 + 12 + 119 + 12 + 1"
        );
    }

    #[test]
    fn world_clocks_section_grows_the_calendar_column() {
        let mut dm = DateMenu::new(0, false, [0, 0, 0], vec![]);
        let base = dm.logical_size().h;
        // Hidden (Clocks not installed) adds no height.
        assert!(dm.set_world_clocks(wc_model(false, 2)));
        assert_eq!(dm.logical_size().h, base, "hidden section adds no height");
        // A visible section grows the column.
        assert!(dm.set_world_clocks(wc_model(true, 2)));
        assert!(
            dm.logical_size().h > base,
            "a visible world-clocks section grows the calendar column"
        );
    }

    #[test]
    fn world_clocks_click_launches_clocks() {
        let mut dm = DateMenu::new(0, false, [0, 0, 0], vec![]);
        dm.set_world_clocks(wc_model(true, 1));
        let rect = dm.world_clocks_rect().expect("visible section has a rect");
        let center = rect.loc + Point::from((rect.size.w / 2., rect.size.h / 2.));
        assert_eq!(
            dm.pointer_click(center),
            PopoverAction::Spawn(vec!["gtk-launch".into(), "org.gnome.clocks".into()]),
            "clicking the card launches GNOME Clocks"
        );
    }

    #[test]
    fn world_clocks_hover_lights_up_the_card() {
        let mut dm = DateMenu::new(0, false, [0, 0, 0], vec![]);
        dm.set_world_clocks(wc_model(true, 1));
        let rect = dm.world_clocks_rect().expect("visible section has a rect");
        let center = rect.loc + Point::from((rect.size.w / 2., rect.size.h / 2.));
        // Moving onto the card sets the hover wash and reports a redraw.
        assert!(dm.pointer_hover(Some(center)), "entering the card redraws");
        assert!(
            dm.world_clocks_button.hovered(),
            "pointer over the card is hovered"
        );
        // Idempotent while still inside.
        assert!(!dm.pointer_hover(Some(center)), "staying inside is a no-op");
        // Leaving clears it.
        assert!(dm.pointer_hover(None), "leaving the card redraws");
        assert!(
            !dm.world_clocks_button.hovered(),
            "pointer gone is not hovered"
        );
    }

    #[test]
    fn world_clocks_card_bakes_at_multiple_scales() {
        let mut vk = match VulkanRenderer::new() {
            Ok(r) => r,
            Err(e) => {
                eprintln!("skipping world_clocks_card_bakes_at_multiple_scales: no Vulkan ({e})");
                return;
            }
        };
        let mut dm = DateMenu::new(0, false, [0, 0, 0], vec![]);
        dm.set_world_clocks(wc_model(true, 2));
        for scale in [1.0, 1.5, 2.0] {
            let tex = dm
                .world_clocks_texture(&mut vk, scale)
                .expect("world clocks texture bakes");
            let size = smithay::backend::renderer::Texture::size(&tex);
            assert!(size.w > 0 && size.h > 0, "non-empty at scale {scale}");
        }
        // Empty (Add World Clocks…) also bakes.
        dm.set_world_clocks(wc_model(true, 0));
        assert!(dm.world_clocks_texture(&mut vk, 2.0).is_ok());
    }
}
