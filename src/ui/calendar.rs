//! The dateMenu calendar grid.
//!
//! A fork-owned port of gnome-shell's `js/ui/calendar.js` month view: a
//! `‹ Month Year ›` header with prev/next-month pagers, a weekday-abbreviation
//! row rotated by the locale/gsettings week-start, and a 6×7 day grid with the
//! current day highlighted and out-of-month days dimmed. An optional ISO
//! week-number column shows when `org.gnome.desktop.calendar show-weekdate` is
//! set. Date math is done with libc (no date crate); all text is drawn through
//! the owned Vulkan glyph atlas, like the rest of the panel.
//!
//! Deferred vs gnome-shell: the events list, world clocks, weather, and keyboard
//! grid navigation. Those hang off daemons/D-Bus (CalendarServer, GWeather) or
//! are follow-ups; this is the self-contained core the popover opens on.
//!
//! The popover content itself is [`DateMenu`]: gnome-shell's dateMenu is a
//! two-column hbox with the notification message list as the FIRST (left in
//! LTR) column and the calendar column second (`js/ui/dateMenu.js:917-940`).
//! The list ([`CalendarMessageList`], gnome-shell's `CalendarMessageList` +
//! `MessageView`, `js/ui/calendar.js:794-879`) renders the shared notification
//! cards flat (grouped stacks are a later slice), newest-first, with a
//! placeholder when empty and a Clear pill when not.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::CStr;
use std::ptr::null_mut;

use ordered_float::NotNan;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::{Bind, Color32F, ContextId, Frame as _, Offscreen, Renderer};
use smithay::utils::{Buffer as BufferCoord, Logical, Physical, Point, Rectangle, Size, Transform};

use crate::render_helpers::icon::IconCache;
use crate::render_helpers::renderer::OffscreenRenderer;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::ui::notification_card::{self, CardCache, CardContent, CardLayout};
use crate::ui::popover::PopoverAction;
use crate::utils::to_physical_precise_round;

// Geometry, logical px (grounded in gnome-shell-sass `_calendar.scss` proportions).
const PAD: f64 = 8.;
const HEADER_H: f64 = 36.;
const WEEKDAY_H: f64 = 22.;
const CELL: f64 = 34.;
const WEEKCOL_W: f64 = 26.;
const GRID_ROWS: usize = 6;
const GRID_COLS: usize = 7;

// Month label is GNOME's `.calendar-month-label` (%heading, 11pt); the weekday headings
// and day-number cells are 9pt (`_calendar.scss`). The month-nav chevron is a drawn
// glyph, not a GNOME point size, so it keeps its own logical-px size.
const HEADER_PX: f64 = crate::ui::pt_to_px(11.);
const WEEKDAY_PX: f64 = crate::ui::pt_to_px(9.);
const DAY_PX: f64 = crate::ui::pt_to_px(9.);
const ARROW_PX: f64 = 18.;
/// Diameter (logical px) of the today/selected highlight circle, drawn behind the
/// day number with `render_rounded_rect` (a half-diameter radius clamps to a full
/// circle in `sdf_rect.frag`). gnome-shell 50.1 makes both today and the selected
/// day circular filled buttons (`.calendar-day { border-radius:
/// $forced_circular_radius }`; today `%default_button`, selected `%flat_button`).
const DISC_DIAM: f64 = 30.;

const BOX_BG: [f32; 4] = [0.1, 0.1, 0.1, 1.];
/// Fully transparent — the buffer is cleared to this so the rounded outer corners stay see-through.
const TRANSPARENT: [f32; 4] = [0., 0., 0., 0.];
/// The popover's outer corner radius: gnome-shell's `.popup-menu-content` is
/// `$modal_radius * 1.25` (`$modal_radius` = 16px → 20px, `_popovers.scss:30`) — the standard
/// popup radius, smaller than the quick-settings menu's 2.25× modal radius.
pub const BOX_RADIUS: f64 = 20.;
const TEXT: [f32; 4] = [1., 1., 1., 1.];
/// The selected (non-today) day's subtle filled circle — gnome-shell's flat-button
/// selected state (a faint light fill), vs today's accent fill.
const SELECTED_BG: [f32; 4] = [0.28, 0.28, 0.28, 1.];
/// Out-of-month day numbers, dimmed.
const DIM: [f32; 4] = [0.5, 0.5, 0.5, 1.];
/// Weekday header + week numbers, muted.
const MUTED: [f32; 4] = [0.6, 0.6, 0.6, 1.];

// The "today" header card above the grid — gnome-shell's `TodayButton` (`js/ui/calendar.js`):
// a button showing the weekday name over the full date, tapping it snaps the selection back to
// today. GNOME stacks two labels (`.day-label` weekday over `.date-label` full date) inside a
// framed button. We draw one flat rounded card with the same two lines. Sizes are logical px.
const TODAY_PAD: f64 = 9.;
const DAY_ROW: f64 = 18.;
const DATE_ROW: f64 = 26.;
const TODAY_CARD_H: f64 = TODAY_PAD + DAY_ROW + DATE_ROW + TODAY_PAD; // 62
/// Gap between the today card and the month-nav header below it.
const TODAY_GAP: f64 = 6.;
const TODAY_RADIUS: f64 = 12.;
/// The card fill: BOX_BG lightened, matching GNOME's `#36363a`→`#47474c` button delta (~7%).
const TODAY_CARD_BG: [f32; 4] = [0.17, 0.17, 0.17, 1.];
/// `.day-label` (weekday name) and `.date-label` (full date) point sizes. GNOME's date label is
/// heavier (800) but the rasterizer tops out at bold (700); both draw bold here.
const DAY_LABEL_PX: f64 = crate::ui::pt_to_px(11.);
const DATE_LABEL_PX: f64 = crate::ui::pt_to_px(15.);

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
    /// Bumped on any content change to invalidate the rendered texture.
    revision: u64,
    cache: RefCell<TextureCache>,
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

    /// The calendar's logical size (depends only on whether the week column shows).
    pub fn logical_size(&self) -> Size<f64, Logical> {
        let w = grid_left(self.show_week_numbers) + GRID_COLS as f64 * CELL + PAD;
        let h = grid_top() + HEADER_H + WEEKDAY_H + GRID_ROWS as f64 * CELL + PAD;
        Size::from((w, h))
    }

    /// Step the displayed month by `delta` (keeping the selection where it is).
    fn shift_month(&mut self, delta: i32) {
        let (y, m) = add_months(self.year, self.month, delta);
        self.year = y;
        self.month = m;
        self.revision += 1;
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

    /// The 42 dates filling the 6×7 grid, row-major from the week-start column of
    /// the row containing the 1st.
    fn grid(&self) -> [Ymd; GRID_ROWS * GRID_COLS] {
        let first = Ymd {
            year: self.year,
            month: self.month,
            day: 1,
        };
        let col_of_first = (7 + weekday(first) as i32 - self.week_start as i32) % 7;
        let start = add_days(first, -(col_of_first as i64));
        let mut out = [first; GRID_ROWS * GRID_COLS];
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = add_days(start, i as i64);
        }
        out
    }
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

    fn bounds(&self) -> Rectangle<f64, Logical> {
        let w = grid_left(self.week) + GRID_COLS as f64 * CELL + PAD;
        let h = grid_top() + HEADER_H + WEEKDAY_H + GRID_ROWS as f64 * CELL + PAD;
        Rectangle::new(Point::from((0., 0.)), Size::from((w, h)))
    }

    /// The today-header card, spanning the full inner width above the month-nav header.
    fn today_button(&self) -> Rectangle<f64, Logical> {
        let w = self.bounds().size.w - 2. * PAD;
        Rectangle::new(Point::from((PAD, PAD)), Size::from((w, TODAY_CARD_H)))
    }

    fn prev_arrow(&self) -> Rectangle<f64, Logical> {
        Rectangle::new(
            Point::from((PAD, grid_top())),
            Size::from((HEADER_H, HEADER_H)),
        )
    }

    fn next_arrow(&self) -> Rectangle<f64, Logical> {
        let x = self.bounds().size.w - PAD - HEADER_H;
        Rectangle::new(
            Point::from((x, grid_top())),
            Size::from((HEADER_H, HEADER_H)),
        )
    }

    fn cell(&self, row: usize, col: usize) -> Rectangle<f64, Logical> {
        let x = grid_left(self.week) + col as f64 * CELL;
        let y = grid_top() + HEADER_H + WEEKDAY_H + row as f64 * CELL;
        Rectangle::new(Point::from((x, y)), Size::from((CELL, CELL)))
    }
}

fn grid_left(show_week_numbers: bool) -> f64 {
    PAD + if show_week_numbers { WEEKCOL_W } else { 0. }
}

/// Top y (logical px) of the month-nav header, below the today card. Everything under the header
/// (arrows, weekday row, day grid) is offset by this instead of the bare leading `PAD`.
fn grid_top() -> f64 {
    PAD + TODAY_CARD_H + TODAY_GAP
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

        // Shape every run up front (immutable borrows of the renderer's font system).
        let header_px = (HEADER_PX * scale) as f32;
        let weekday_px = (WEEKDAY_PX * scale) as f32;
        let day_px = (DAY_PX * scale) as f32;
        let arrow_px = (ARROW_PX * scale) as f32;

        let title = strftime_ymd(
            Ymd {
                year: self.year,
                month: self.month,
                day: 1,
            },
            c"%B %Y",
        );
        let title_run = renderer.build_glyph_run(&title, header_px)?;
        let prev_run = renderer.build_glyph_run("\u{2039}", arrow_px)?; // ‹
        let next_run = renderer.build_glyph_run("\u{203a}", arrow_px)?; // ›

        let weekday_runs: Vec<_> = (0..GRID_COLS)
            .map(|c| {
                let w = (self.week_start as usize + c) % 7;
                renderer.build_glyph_run(&weekday_abbrev(w as u32), weekday_px)
            })
            .collect::<Result<_, _>>()?;

        let grid = self.grid();
        let day_runs: Vec<_> = grid
            .iter()
            .map(|d| renderer.build_glyph_run(&d.day.to_string(), day_px))
            .collect::<Result<_, _>>()?;

        let week_runs: Vec<_> = if self.show_week_numbers {
            (0..GRID_ROWS)
                .map(|r| {
                    let d = grid[r * GRID_COLS];
                    renderer.build_glyph_run(&iso_week(d).to_string(), weekday_px)
                })
                .collect::<Result<_, _>>()?
        } else {
            Vec::new()
        };

        // Today card labels: weekday name over the full date, both bold (GNOME's TodayButton).
        let day_label_px = (DAY_LABEL_PX * scale) as f32;
        let date_label_px = (DATE_LABEL_PX * scale) as f32;
        let day_label = strftime_ymd(self.today, c"%A");
        let date_label = strftime_ymd(self.today, c"%B %-d %Y");
        let day_label_run = renderer.build_glyph_run_weighted(&day_label, day_label_px, true)?;
        let date_label_run = renderer.build_glyph_run_weighted(&date_label, date_label_px, true)?;

        let mut target = renderer.create_buffer(
            Fourcc::Abgr8888,
            Size::<i32, BufferCoord>::from((box_w, box_h)),
        )?;
        {
            let mut fb = renderer.bind(&mut target)?;
            let mut frame = renderer.render(&mut fb, phys, Transform::Normal)?;
            let full = Rectangle::from_size(phys);
            // Rounded card: clear transparent, then fill the interior as a rounded rect so the four
            // outer corners stay transparent (the composited element reports opacity excluding
            // those corners — see `PanelPopover::render`). Matches the quick-settings
            // menu's approach.
            frame.clear(Color32F::from(TRANSPARENT), &[full])?;
            frame.render_rounded_rect(BOX_BG, (BOX_RADIUS * scale) as f32, full, &[full])?;

            let px = |v: f64| to_physical_precise_round::<i32>(scale, v);
            let center = |rect: Rectangle<f64, Logical>| {
                (
                    px(rect.loc.x + rect.size.w / 2.),
                    px(rect.loc.y + rect.size.h / 2.),
                )
            };
            // Center a shaped run's ink at a physical point.
            let place = |ink: (i32, i32, i32, i32), cx: i32, cy: i32| {
                let (ix, iy, iw, ih) = ink;
                Point::<i32, Physical>::from((cx - iw / 2 - ix, cy - ih / 2 - iy))
            };
            // Left-align a shaped run's ink at `x`, vertically centered on `cy` (mirrors `place`'s
            // vertical math — the ink's `min_y`/`iy` must be subtracted or the run sits `iy` low).
            let place_left = |ink: (i32, i32, i32, i32), x: i32, cy: i32| {
                let (ix, iy, _iw, ih) = ink;
                Point::<i32, Physical>::from((x - ix, cy - ih / 2 - iy))
            };

            // Today card: a flat rounded fill with the weekday name over the full date.
            let card = layout.today_button();
            let card_rect = Rectangle::new(
                Point::<i32, Physical>::from((px(card.loc.x), px(card.loc.y))),
                Size::<i32, Physical>::from((px(card.size.w), px(card.size.h))),
            );
            frame.render_rounded_rect(
                TODAY_CARD_BG,
                (TODAY_RADIUS * scale) as f32,
                card_rect,
                &[full],
            )?;
            let label_x = px(PAD + TODAY_PAD);
            let day_cy = px(PAD + TODAY_PAD + DAY_ROW / 2.);
            frame.render_glyphs(
                &day_label_run,
                place_left(day_label_run.ink_bounds(), label_x, day_cy),
                MUTED,
                full,
                &[full],
            )?;
            let date_cy = px(PAD + TODAY_PAD + DAY_ROW + DATE_ROW / 2.);
            frame.render_glyphs(
                &date_label_run,
                place_left(date_label_run.ink_bounds(), label_x, date_cy),
                TEXT,
                full,
                &[full],
            )?;

            // Header: ‹ arrows › and the centered "Month Year".
            let (px_, py_) = center(layout.prev_arrow());
            frame.render_glyphs(
                &prev_run,
                place(prev_run.ink_bounds(), px_, py_),
                MUTED,
                full,
                &[full],
            )?;
            let (nx, ny) = center(layout.next_arrow());
            frame.render_glyphs(
                &next_run,
                place(next_run.ink_bounds(), nx, ny),
                MUTED,
                full,
                &[full],
            )?;
            let title_cx = box_w / 2;
            let title_cy = px(grid_top() + HEADER_H / 2.);
            frame.render_glyphs(
                &title_run,
                place(title_run.ink_bounds(), title_cx, title_cy),
                TEXT,
                full,
                &[full],
            )?;

            // Weekday header row.
            let wd_cy = px(grid_top() + HEADER_H + WEEKDAY_H / 2.);
            for (c, run) in weekday_runs.iter().enumerate() {
                let cx = px(grid_left(self.show_week_numbers) + (c as f64 + 0.5) * CELL);
                frame.render_glyphs(
                    run,
                    place(run.ink_bounds(), cx, wd_cy),
                    MUTED,
                    full,
                    &[full],
                )?;
            }

            // Week-number column.
            for (r, run) in week_runs.iter().enumerate() {
                let cx = px(PAD + WEEKCOL_W / 2.);
                let cy = px(grid_top() + HEADER_H + WEEKDAY_H + (r as f64 + 0.5) * CELL);
                frame.render_glyphs(run, place(run.ink_bounds(), cx, cy), MUTED, full, &[full])?;
            }

            // Day grid.
            for (i, date) in grid.iter().enumerate() {
                let (cx, cy) = center(layout.cell(i / GRID_COLS, i % GRID_COLS));
                let is_today = *date == self.today;
                let is_selected = *date == self.selected;
                // Today: accent-filled circle; selected (not today): a subtle filled circle —
                // matching gnome-shell's circular calendar-day buttons. The day number draws on
                // top. A half-diameter radius clamps to a full circle in `sdf_rect.frag`.
                if is_today || is_selected {
                    let side = px(DISC_DIAM);
                    let disc = Rectangle::new(
                        Point::<i32, Physical>::from((cx - side / 2, cy - side / 2)),
                        Size::<i32, Physical>::from((side, side)),
                    );
                    let bg = if is_today { self.accent } else { SELECTED_BG };
                    frame.render_rounded_rect(bg, (side / 2) as f32, disc, &[full])?;
                }
                let in_month = date.month == self.month;
                let color = if is_today || in_month { TEXT } else { DIM };
                let run = &day_runs[i];
                frame.render_glyphs(run, place(run.ink_bounds(), cx, cy), color, full, &[full])?;
            }

            let _sync = frame.finish()?;
        }

        renderer.make_offscreen_sampleable(&target)?;
        Ok(target)
    }
}

// ---- The dateMenu popover: message list column + calendar ----

/// 1em at the 11pt base font.
const LIST_EM: f64 = crate::ui::pt_to_px(11.);
/// `.message-list` width (`_message-list.scss:3`).
const LIST_W: f64 = 29. * LIST_EM;
/// `.popup-menu-content` padding (`_popovers.scss:28`).
const LIST_PAD: f64 = 6.;
/// `.message-list:ltr` margin-right, separating the two columns
/// (`_message-list.scss:11`).
const LIST_MARGIN_R: f64 = 4.;
/// Space kept free right of the cards: the list's `padding-right:
/// $base_padding` plus `.message-view:ltr` `margin-right: $base_margin * 3`
/// (scrollbar room, `_message-list.scss:11,31`).
const LIST_SCROLL_R: f64 = 18.;
/// Card width in the list column.
const CARD_W: f64 = LIST_W - LIST_SCROLL_R;
/// `.message` `margin-bottom: $base_padding * 2` (`_message-list.scss:37`).
const CARD_GAP: f64 = 12.;
/// List-card radius: `$modal_radius + 2px` (`_message-list.scss:39`).
const CARD_RADIUS: f64 = 18.;
/// `.message-list-controls` padding: 12px sides/top, 9px bottom
/// (`_message-list.scss:44-47`).
const CONTROLS_PAD: f64 = 12.;
const CONTROLS_PAD_B: f64 = 9.;
/// The Clear pill (`.message-list-clear-button button`, forced-circular
/// radius): `%heading` 11pt/700 label.
const CLEAR_H: f64 = 28.;
const CLEAR_PAD_X: f64 = 14.;
const CLEAR_PX: f64 = crate::ui::pt_to_px(11.);
/// `.button` flat fill on the dark theme (matches the card action pills).
const CLEAR_BG: [f32; 4] = [1., 1., 1., 0.1];
/// `.message-list-placeholder` (`_message-list.scss:14-26`): 96px icon over a
/// `%title_3` (15pt/700) label, both at 45% fg.
const PLACEHOLDER_ICON: f64 = 96.;
const PLACEHOLDER_GAP: f64 = 12.;
const PLACEHOLDER_PX: f64 = crate::ui::pt_to_px(15.);
const PLACEHOLDER_FG: [f32; 4] = [1., 1., 1., 0.45];
/// The 1px column separator: `.message-list`'s `border-right` in
/// `$borders_color` = fg at 10% on the dark theme (`_message-list.scss:8,11`,
/// `_colors.scss:39`).
const SEPARATOR: [f32; 4] = [1., 1., 1., 0.1];

/// A visible card's `(id, card rect, close-button rect)`, popover-local
/// (introspection/test hook).
pub type CardRects = (u32, Rectangle<f64, Logical>, Rectangle<f64, Logical>);

/// What a click inside the message-list column hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListHit {
    /// A card's close button: close that notification, reason Dismissed.
    Close(u32),
    /// A card body: activate the notification (same semantics as a banner
    /// body click — the list card is a click-through to
    /// `notification.activate()`, `js/ui/messageList.js:730-732`).
    Body { id: u32, has_default: bool },
    /// The Clear pill: close everything.
    Clear,
}

/// The message-list column of the calendar popover: a plain-data snapshot of
/// the notification store, rendered as flat cards newest-first.
pub struct CalendarMessageList {
    cards: Vec<CardContent>,
    /// Bumped whenever the snapshot changes, to invalidate cached card
    /// textures (cache keys carry the revision in their high 32 bits).
    revision: u64,
    cache: RefCell<CardCache>,
}

impl CalendarMessageList {
    pub fn new(cards: Vec<CardContent>) -> Self {
        Self {
            cards,
            revision: 0,
            cache: RefCell::new(CardCache::new()),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.cards.is_empty()
    }

    pub fn len(&self) -> usize {
        self.cards.len()
    }

    /// Replace the snapshot (a store change pushed while the popover is
    /// open). Returns whether anything changed.
    pub fn set_cards(&mut self, cards: Vec<CardContent>) -> bool {
        if self.cards == cards {
            return false;
        }
        self.cards = cards;
        self.revision += 1;
        true
    }

    /// Height of the bottom controls row (the Clear pill), when shown.
    fn controls_h(&self) -> f64 {
        if self.is_empty() {
            0.
        } else {
            CONTROLS_PAD + CLEAR_H + CONTROLS_PAD_B
        }
    }

    /// The cards that fully fit above the controls row, with their popover-
    /// local origins. Overflowing cards are dropped — the list doesn't scroll
    /// yet (recorded divergence; gnome-shell scrolls).
    fn visible_cards(&self, height: f64) -> Vec<(usize, Point<f64, Logical>, CardLayout)> {
        let mut out = Vec::new();
        let bottom = height - self.controls_h();
        let mut y = LIST_PAD;
        for (i, content) in self.cards.iter().enumerate() {
            let layout = notification_card::layout(content, CARD_W, false);
            if y + layout.size.h > bottom {
                break;
            }
            let h = layout.size.h;
            out.push((i, Point::from((LIST_PAD, y)), layout));
            y += h + CARD_GAP;
        }
        out
    }

    /// The Clear pill's popover-local rect (only meaningful when non-empty).
    fn clear_rect(&self, height: f64) -> Rectangle<f64, Logical> {
        let label_w = niri_vk::text::measure_line_width_weighted("Clear", CLEAR_PX as f32, true);
        Rectangle::new(
            Point::from((LIST_PAD + CONTROLS_PAD, height - CONTROLS_PAD_B - CLEAR_H)),
            Size::from((label_w + 2. * CLEAR_PAD_X, CLEAR_H)),
        )
    }

    /// Hit-test a click at popover-local `pos` inside the list column.
    fn hit(&self, pos: Point<f64, Logical>, height: f64) -> Option<ListHit> {
        for (i, origin, layout) in self.visible_cards(height) {
            let rect = Rectangle::new(origin, layout.size);
            if !rect.contains(pos) {
                continue;
            }
            let local = pos - origin;
            let content = &self.cards[i];
            if layout.close.contains(local) {
                return Some(ListHit::Close(content.id));
            }
            return Some(ListHit::Body {
                id: content.id,
                has_default: content.has_default_action,
            });
        }
        if !self.is_empty() && self.clear_rect(height).contains(pos) {
            return Some(ListHit::Clear);
        }
        None
    }

    /// The card render elements (textures + icons), popover-relative to
    /// `origin`.
    fn render(
        &self,
        renderer: &mut VulkanRenderer,
        icons: &IconCache,
        scale: f64,
        origin: Point<f64, Logical>,
        height: f64,
    ) -> Vec<TextureRenderElement<VkTexture>> {
        let mut elements = Vec::new();
        let mut cache = self.cache.borrow_mut();
        let rev = self.revision & 0xffff_ffff;
        cache.retain(|key| key >> 32 == rev);
        for (i, card_origin, layout) in self.visible_cards(height) {
            let key = (rev << 32) | i as u64;
            elements.extend(notification_card::card_elements(
                renderer,
                icons,
                &mut cache,
                key,
                &self.cards[i],
                &layout,
                CARD_RADIUS,
                origin + card_origin,
                1.,
                scale,
            ));
        }
        elements
    }
}

/// The dateMenu popover content: the message-list column (left) beside the
/// calendar column (`js/ui/dateMenu.js:917-940`).
pub struct DateMenu {
    pub calendar: Calendar,
    list: CalendarMessageList,
    /// The popover background (rounded box + placeholder label / Clear pill),
    /// cached per scale; the stored revision is 0/1 for empty/non-empty.
    bg_cache: RefCell<TextureCache>,
}

/// The x where the calendar column starts (also the list column's width).
fn calendar_col_x() -> f64 {
    LIST_PAD + LIST_W + LIST_MARGIN_R
}

impl DateMenu {
    pub fn new(
        week_start: u8,
        show_week_numbers: bool,
        accent: [u8; 3],
        cards: Vec<CardContent>,
    ) -> Self {
        Self {
            calendar: Calendar::new(week_start, show_week_numbers, accent),
            list: CalendarMessageList::new(cards),
            bg_cache: RefCell::new(TextureCache {
                context: None,
                textures: HashMap::new(),
            }),
        }
    }

    pub fn logical_size(&self) -> Size<f64, Logical> {
        let cal = self.calendar.logical_size();
        Size::from((calendar_col_x() + cal.w, cal.h))
    }

    pub fn list(&self) -> &CalendarMessageList {
        &self.list
    }

    /// Push a fresh store snapshot into the list. Returns whether it changed.
    pub fn set_notifications(&mut self, cards: Vec<CardContent>) -> bool {
        self.list.set_cards(cards)
    }

    /// Route a click at content-local `pos`: list hits map to notification
    /// actions; everything else goes to the calendar (all consumed — the
    /// popover stays open, like gnome-shell's grab).
    pub fn pointer_click(&mut self, pos: Point<f64, Logical>) -> PopoverAction {
        let size = self.logical_size();
        if pos.x < calendar_col_x() {
            return match self.list.hit(pos, size.h) {
                Some(ListHit::Close(id)) => PopoverAction::CloseNotification(id),
                Some(ListHit::Body { id, has_default }) => {
                    PopoverAction::ActivateNotification { id, has_default }
                }
                Some(ListHit::Clear) => PopoverAction::ClearNotifications,
                None => PopoverAction::Consumed,
            };
        }
        self.calendar
            .pointer_click(pos - Point::from((calendar_col_x(), 0.)));
        PopoverAction::Consumed
    }

    /// Test hooks: the visible cards' popover-local rects, and the Clear pill.
    pub fn card_rects(&self) -> Vec<CardRects> {
        let h = self.logical_size().h;
        self.list
            .visible_cards(h)
            .into_iter()
            .map(|(i, origin, layout)| {
                let close = Rectangle::new(origin + layout.close.loc, layout.close.size);
                (
                    self.list.cards[i].id,
                    Rectangle::new(origin, layout.size),
                    close,
                )
            })
            .collect()
    }

    pub fn clear_pill_rect(&self) -> Option<Rectangle<f64, Logical>> {
        (!self.list.is_empty()).then(|| self.list.clear_rect(self.logical_size().h))
    }

    /// Draw the popover background: the full-size rounded box, plus the
    /// placeholder label (empty) or the Clear pill (non-empty). The calendar
    /// texture composites over the right column in the same bg color.
    fn bg_texture(&self, renderer: &mut VulkanRenderer, scale: f64) -> anyhow::Result<VkTexture> {
        let scale_key = NotNan::new(scale).map_err(|_| anyhow::anyhow!("bad scale"))?;
        let revision = u64::from(!self.list.is_empty());
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
        let full = Rectangle::from_size(phys);

        // Shape the text up front (immutable borrows of the font system).
        let placeholder_run = self
            .list
            .is_empty()
            .then(|| {
                renderer.build_glyph_run_weighted(
                    "No Notifications",
                    (PLACEHOLDER_PX * scale) as f32,
                    true,
                )
            })
            .transpose()?;
        let clear_run = (!self.list.is_empty())
            .then(|| renderer.build_glyph_run_weighted("Clear", (CLEAR_PX * scale) as f32, true))
            .transpose()?;

        let mut target = renderer.create_buffer(
            Fourcc::Abgr8888,
            Size::<i32, BufferCoord>::from((phys.w, phys.h)),
        )?;
        {
            let mut fb = renderer.bind(&mut target)?;
            let mut frame = renderer.render(&mut fb, phys, Transform::Normal)?;
            frame.clear(Color32F::from(TRANSPARENT), &[full])?;
            frame.render_rounded_rect(BOX_BG, (BOX_RADIUS * scale) as f32, full, &[full])?;

            // The faint 1px separator on the list column's right edge
            // (`.message-list` border-right, `_message-list.scss:8,11`).
            let sep_x = px(LIST_PAD + LIST_W);
            let sep = Rectangle::new(
                Point::<i32, Physical>::from((sep_x, 0)),
                Size::<i32, Physical>::from((((scale).round() as i32).max(1), phys.h)),
            );
            frame.render_rounded_rect(SEPARATOR, 0., sep, &[full])?;

            if let Some(run) = &placeholder_run {
                // Centered under the (separately composited) 96px icon.
                let (_, cy) = placeholder_centers(size.h);
                let cx = px(LIST_PAD + LIST_W / 2.);
                let (ix, iy, iw, ih) = run.ink_bounds();
                let origin = Point::<i32, Physical>::from((cx - iw / 2 - ix, px(cy) - ih / 2 - iy));
                frame.render_glyphs(run, origin, PLACEHOLDER_FG, full, &[full])?;
            }

            if let Some(run) = &clear_run {
                let pill = self.list.clear_rect(size.h);
                let rect = Rectangle::new(
                    Point::<i32, Physical>::from((px(pill.loc.x), px(pill.loc.y))),
                    Size::<i32, Physical>::from((px(pill.size.w), px(pill.size.h))),
                );
                frame.render_rounded_rect(
                    CLEAR_BG,
                    (CLEAR_H / 2. * scale) as f32,
                    rect,
                    &[full],
                )?;
                let (ix, iy, iw, ih) = run.ink_bounds();
                let origin = Point::<i32, Physical>::from((
                    rect.loc.x + (rect.size.w - iw) / 2 - ix,
                    rect.loc.y + (rect.size.h - ih) / 2 - iy,
                ));
                frame.render_glyphs(run, origin, TEXT, rect, &[full])?;
            }

            let _sync = frame.finish()?;
        }

        renderer.make_offscreen_sampleable(&target)?;
        Ok(target)
    }

    /// All the popover's render elements at `origin`, in output stacking
    /// order (FIRST = topmost): message-list cards / placeholder icon, then
    /// the calendar column, then the background box (carrying the
    /// rounded-corner-aware opaque region) at the bottom.
    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        icons: &IconCache,
        scale: f64,
        origin: Point<f64, Logical>,
    ) -> Vec<TextureRenderElement<VkTexture>> {
        use smithay::backend::renderer::Texture as _;

        let mut elements = Vec::new();
        let size = self.logical_size();

        // The message-list cards, or the placeholder icon when empty.
        if self.list.is_empty() {
            let (icon_cy, _) = placeholder_centers(size.h);
            let center = Point::from((LIST_PAD + LIST_W / 2., icon_cy));
            if let Some(buffer) = icons.buffer(
                "no-notifications-symbolic",
                PLACEHOLDER_ICON,
                scale,
                PLACEHOLDER_FG,
            ) {
                if let Ok(tb) = TextureBuffer::from_memory_buffer(renderer, &buffer) {
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
            }
        } else {
            elements.extend(self.list.render(renderer, icons, scale, origin, size.h));
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

        // The background box, at the bottom of the stack, reporting opacity
        // as the two bands that exclude the rounded corners — never claiming
        // a cut-away corner pixel is opaque (which would let occlusion drop
        // what shows through).
        match self.bg_texture(renderer, scale) {
            Ok(texture) => {
                let tex_size = texture.size();
                let r = (BOX_RADIUS * scale).round() as i32;
                let opaque = if r > 0 && tex_size.w > 2 * r && tex_size.h > 2 * r {
                    vec![
                        Rectangle::new(
                            Point::<i32, BufferCoord>::from((0, r)),
                            Size::from((tex_size.w, tex_size.h - 2 * r)),
                        ),
                        Rectangle::new(
                            Point::<i32, BufferCoord>::from((r, 0)),
                            Size::from((tex_size.w - 2 * r, tex_size.h)),
                        ),
                    ]
                } else {
                    vec![Rectangle::from_size(tex_size)]
                };
                let buffer = TextureBuffer::from_texture(
                    renderer,
                    texture,
                    scale,
                    Transform::Normal,
                    opaque,
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
    let label_h = PLACEHOLDER_PX * 1.3;
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
    strftime_ymd(date, c"%a")
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn today_button_returns_to_today_only_when_off_today() {
        // The today card adds exactly its height + gap above the grid.
        assert_eq!(grid_top() - PAD, TODAY_CARD_H + TODAY_GAP);
        let cal = Calendar::new(0, false, [0, 0, 0]);
        assert_eq!(
            cal.logical_size().h,
            grid_top() + HEADER_H + WEEKDAY_H + GRID_ROWS as f64 * CELL + PAD
        );

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
        use smithay::backend::renderer::{ExportMem, Texture as _};

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
        // The interior background is opaque dark (sample the left-edge padding, past the corner
        // radius so it's inside the rounded rect).
        let interior = px_at(3, size.h / 2);
        assert_eq!(
            interior[3], 255,
            "calendar interior must be opaque, got {interior:?}"
        );
        assert!(interior[0] < 60 && interior[1] < 60 && interior[2] < 60);
        // The outer corners are rounded away — a pixel in the extreme corner is transparent.
        let corner = px_at(size.w - 2, size.h - 2);
        assert_eq!(
            corner[3], 0,
            "calendar outer corner must be transparent (rounded), got {corner:?}"
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

    #[test]
    fn date_menu_is_two_columns_list_first() {
        // The message-list column comes FIRST (left in LTR), the calendar
        // second (`js/ui/dateMenu.js:917-940`); the popover keeps the
        // calendar's height.
        let dm = DateMenu::new(0, false, [0, 0, 0], vec![sample_card(1)]);
        let cal = dm.calendar.logical_size();
        let size = dm.logical_size();
        assert_eq!(size.w, calendar_col_x() + cal.w);
        assert_eq!(size.h, cal.h);
        assert!(
            calendar_col_x() > 29. * LIST_EM,
            "the list column is 29em wide plus its margins"
        );
    }

    #[test]
    fn date_menu_routes_clicks_to_list_and_calendar() {
        let mut dm = DateMenu::new(0, false, [0, 0, 0], vec![sample_card(1), sample_card(2)]);

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
        assert!(dm.set_notifications(vec![with_default]));
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
        assert!(dm.set_notifications(vec![sample_card(1)]));
        assert_eq!(dm.list().len(), 1);
        assert!(dm.clear_pill_rect().is_some());
        assert!(
            !dm.set_notifications(vec![sample_card(1)]),
            "an identical snapshot must not invalidate the cards"
        );
    }

    #[test]
    fn message_list_overflow_drops_cards_beyond_the_controls() {
        // More cards than fit: only whole cards above the controls row render
        // (no scrolling yet — recorded divergence; gnome-shell scrolls).
        let cards: Vec<_> = (1..=10).map(sample_card).collect();
        let dm = DateMenu::new(0, false, [0, 0, 0], cards);
        let h = dm.logical_size().h;
        let visible = dm.card_rects();
        assert!(!visible.is_empty());
        assert!(visible.len() < 10, "10 cards cannot fit the popover height");
        let controls_top = h - (CONTROLS_PAD + CLEAR_H + CONTROLS_PAD_B);
        for (_, rect, _) in &visible {
            assert!(
                rect.loc.y + rect.size.h <= controls_top,
                "every visible card fits fully above the Clear row"
            );
        }
    }
}
