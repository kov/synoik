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

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::CStr;
use std::ptr::null_mut;

use ordered_float::NotNan;
use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::{Bind, Color32F, ContextId, Frame as _, Offscreen, Renderer};
use smithay::utils::{Buffer as BufferCoord, Logical, Physical, Point, Rectangle, Size, Transform};

use crate::render_helpers::renderer::OffscreenRenderer;
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::utils::to_physical_precise_round;

// Geometry, logical px (grounded in gnome-shell-sass `_calendar.scss` proportions).
const PAD: f64 = 8.;
const HEADER_H: f64 = 36.;
const WEEKDAY_H: f64 = 22.;
const CELL: f64 = 34.;
const WEEKCOL_W: f64 = 26.;
const GRID_ROWS: usize = 6;
const GRID_COLS: usize = 7;

const HEADER_PX: f64 = 14.;
const WEEKDAY_PX: f64 = 10.;
const DAY_PX: f64 = 12.;
const ARROW_PX: f64 = 18.;
/// The today/selected highlight disc, drawn as a filled-circle glyph behind the
/// day number (a rounded fill can't be drawn inside a hand-bound offscreen, but a
/// `●` glyph can — see the buffer/offscreen notes in the render helpers).
const DISC_PX: f64 = 30.;

const BOX_BG: [f32; 4] = [0.1, 0.1, 0.1, 1.];
const TEXT: [f32; 4] = [1., 1., 1., 1.];
/// Out-of-month day numbers, dimmed.
const DIM: [f32; 4] = [0.5, 0.5, 0.5, 1.];
/// Weekday header + week numbers, muted.
const MUTED: [f32; 4] = [0.6, 0.6, 0.6, 1.];

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
        let h = PAD + HEADER_H + WEEKDAY_H + GRID_ROWS as f64 * CELL + PAD;
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
        let h = PAD + HEADER_H + WEEKDAY_H + GRID_ROWS as f64 * CELL + PAD;
        Rectangle::new(Point::from((0., 0.)), Size::from((w, h)))
    }

    fn prev_arrow(&self) -> Rectangle<f64, Logical> {
        Rectangle::new(Point::from((PAD, PAD)), Size::from((HEADER_H, HEADER_H)))
    }

    fn next_arrow(&self) -> Rectangle<f64, Logical> {
        let x = self.bounds().size.w - PAD - HEADER_H;
        Rectangle::new(Point::from((x, PAD)), Size::from((HEADER_H, HEADER_H)))
    }

    fn cell(&self, row: usize, col: usize) -> Rectangle<f64, Logical> {
        let x = grid_left(self.week) + col as f64 * CELL;
        let y = PAD + HEADER_H + WEEKDAY_H + row as f64 * CELL;
        Rectangle::new(Point::from((x, y)), Size::from((CELL, CELL)))
    }
}

fn grid_left(show_week_numbers: bool) -> f64 {
    PAD + if show_week_numbers { WEEKCOL_W } else { 0. }
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
        let disc_px = (DISC_PX * scale) as f32;

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
        let disc_run = renderer.build_glyph_run("\u{25cf}", disc_px)?; // ●
        let ring_run = renderer.build_glyph_run("\u{25cb}", disc_px)?; // ○

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

        let mut target = renderer.create_buffer(
            Fourcc::Abgr8888,
            Size::<i32, BufferCoord>::from((box_w, box_h)),
        )?;
        {
            let mut fb = renderer.bind(&mut target)?;
            let mut frame = renderer.render(&mut fb, phys, Transform::Normal)?;
            let full = Rectangle::from_size(phys);
            frame.clear(Color32F::from(BOX_BG), &[full])?;

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
            let title_cy = px(PAD + HEADER_H / 2.);
            frame.render_glyphs(
                &title_run,
                place(title_run.ink_bounds(), title_cx, title_cy),
                TEXT,
                full,
                &[full],
            )?;

            // Weekday header row.
            let wd_cy = px(PAD + HEADER_H + WEEKDAY_H / 2.);
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
                let cy = px(PAD + HEADER_H + WEEKDAY_H + (r as f64 + 0.5) * CELL);
                frame.render_glyphs(run, place(run.ink_bounds(), cx, cy), MUTED, full, &[full])?;
            }

            // Day grid.
            for (i, date) in grid.iter().enumerate() {
                let (cx, cy) = center(layout.cell(i / GRID_COLS, i % GRID_COLS));
                let is_today = *date == self.today;
                let is_selected = *date == self.selected;
                if is_today {
                    frame.render_glyphs(
                        &disc_run,
                        place(disc_run.ink_bounds(), cx, cy),
                        self.accent,
                        full,
                        &[full],
                    )?;
                } else if is_selected {
                    frame.render_glyphs(
                        &ring_run,
                        place(ring_run.ink_bounds(), cx, cy),
                        MUTED,
                        full,
                        &[full],
                    )?;
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

        // Opaque dark background.
        let corner = {
            let (x, y) = (size.w - 3, size.h - 3);
            let i = ((y * size.w + x) * 4) as usize;
            [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
        };
        assert_eq!(
            corner[3], 255,
            "calendar box must be opaque, got {corner:?}"
        );
        assert!(corner[0] < 60 && corner[1] < 60 && corner[2] < 60);

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
}
