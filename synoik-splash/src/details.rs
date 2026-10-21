// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The details view (`docs/fork/boot-splash.md` §8): the boot's text over the splash, as two-step's
//! console viewer shows it (plymouth 24.004.60, `libply-splash-graphics/ply-console-viewer.c`).
//! The text goes through the subset of a terminal plymouth's emulator implements
//! (`libply-splash-core/ply-terminal-emulator.c`): what systemd's status lines and the kernel's
//! records, coloured as `ply-kmsg-reader.c` colours them, need.

use std::collections::VecDeque;

use crate::layout::{Point, Size};

/// The lines kept: more than any screen shows, so the view never runs short, and few enough that
/// the handover's state stays small.
pub const MAX_LINES: usize = 500;

/// `PLY_TERMINAL_EMULATOR_SPACES_PER_TAB`.
const TAB: usize = 8;

/// A terminal colour (SGR 30-37), and the viewer's own text colour for the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum Color {
    #[default]
    Default,
    Black,
    Red,
    Green,
    Brown,
    Blue,
    Magenta,
    Cyan,
    White,
}

impl Color {
    /// The colour drawn: the Linux VT palette the label plugins use
    /// (`look_up_rgb_color_from_terminal_color`), white for the default
    /// (`PLY_CONSOLE_VIEWER_LOG_TEXT_COLOR`).
    pub fn rgb(self) -> [f32; 3] {
        let (r, g, b): (u8, u8, u8) = match self {
            Self::Default => (0xff, 0xff, 0xff),
            Self::Black => (0, 0, 0),
            Self::Red => (0xaa, 0, 0),
            Self::Green => (0, 0xaa, 0),
            Self::Brown => (0xaa, 0x55, 0),
            Self::Blue => (0, 0, 0xaa),
            Self::Magenta => (0xaa, 0, 0xaa),
            Self::Cyan => (0, 0xaa, 0xaa),
            Self::White => (0xaa, 0xaa, 0xaa),
        };
        [r, g, b].map(|c| f32::from(c) / 255.)
    }

    fn from_sgr(n: u32) -> Self {
        match n {
            30 => Self::Black,
            31 => Self::Red,
            32 => Self::Green,
            33 => Self::Brown,
            34 => Self::Blue,
            35 => Self::Magenta,
            36 => Self::Cyan,
            37 => Self::White,
            _ => Self::Default,
        }
    }
}

/// How a character is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Style {
    pub color: Color,
    pub bold: bool,
}

/// One run of a line in one style.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Line {
    chars: Vec<(char, Style)>,
}

impl Line {
    fn spans(&self) -> Vec<Span> {
        let mut spans: Vec<Span> = Vec::new();
        for &(c, style) in &self.chars {
            match spans.last_mut() {
                Some(span) if span.style == style => span.text.push(c),
                _ => spans.push(Span {
                    text: c.to_string(),
                    style,
                }),
            }
        }
        spans
    }

    /// Pad with blanks up to `col`, so a write there lands at its column.
    fn reach(&mut self, col: usize) {
        if self.chars.len() < col {
            self.chars.resize(col, (' ', Style::default()));
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum Parse {
    #[default]
    Ground,
    Escape,
    /// A control sequence's parameters so far.
    Csi(String),
}

/// The boot's text, as a terminal would hold it: lines, a cursor, the current style.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(from = "Saved", into = "Saved")]
pub struct Console {
    lines: VecDeque<Line>,
    row: usize,
    col: usize,
    style: Style,
    parse: Parse,
    /// The start of a UTF-8 character split across writes.
    pending: Vec<u8>,
}

/// What crosses a handover: the text and the cursor, not a half-parsed sequence.
#[derive(serde::Serialize, serde::Deserialize)]
struct Saved {
    lines: Vec<Vec<Span>>,
    row: usize,
    col: usize,
}

impl From<Console> for Saved {
    fn from(console: Console) -> Self {
        Self {
            lines: console.lines.iter().map(Line::spans).collect(),
            row: console.row,
            col: console.col,
        }
    }
}

impl From<Saved> for Console {
    fn from(saved: Saved) -> Self {
        let lines: VecDeque<Line> = saved
            .lines
            .into_iter()
            .map(|spans| Line {
                chars: spans
                    .iter()
                    .flat_map(|s| s.text.chars().map(move |c| (c, s.style)))
                    .collect(),
            })
            .collect();
        Self {
            row: saved.row.min(lines.len().saturating_sub(1)),
            col: saved.col,
            lines,
            ..Self::default()
        }
    }
}

impl Console {
    /// Every line, oldest first, as styled runs.
    pub fn lines(&self) -> impl Iterator<Item = Vec<Span>> + '_ {
        self.lines.iter().map(Line::spans)
    }

    pub fn is_empty(&self) -> bool {
        self.lines.iter().all(|l| l.chars.is_empty())
    }

    /// Take `bytes` of terminal output.
    pub fn write(&mut self, bytes: &[u8]) {
        let mut input = std::mem::take(&mut self.pending);
        input.extend_from_slice(bytes);
        let mut rest = &input[..];
        while !rest.is_empty() {
            match std::str::from_utf8(rest) {
                Ok(text) => {
                    self.write_str(text);
                    break;
                }
                Err(err) => {
                    let (good, bad) = rest.split_at(err.valid_up_to());
                    self.write_str(std::str::from_utf8(good).unwrap_or_default());
                    match err.error_len() {
                        // A character cut short: keep it for the next write.
                        None => {
                            self.pending = bad.to_vec();
                            break;
                        }
                        Some(len) => {
                            self.put('\u{fffd}');
                            rest = &bad[len..];
                        }
                    }
                }
            }
        }
    }

    pub fn write_str(&mut self, text: &str) {
        for c in text.chars() {
            self.feed(c);
        }
    }

    fn feed(&mut self, c: char) {
        match std::mem::take(&mut self.parse) {
            Parse::Ground => match c {
                '\x1b' => self.parse = Parse::Escape,
                '\n' | '\x0b' | '\x0c' => self.linefeed(),
                '\r' => self.col = 0,
                '\x08' => self.col = self.col.saturating_sub(1),
                '\t' => self.col = (self.col / TAB + 1) * TAB,
                c if c.is_control() => (),
                c => self.put(c),
            },
            Parse::Escape => match c {
                '[' => self.parse = Parse::Csi(String::new()),
                // ESC D, index; ESC E, next line; ESC M, reverse index.
                'D' | 'E' => self.linefeed(),
                'M' => self.row = self.row.saturating_sub(1),
                _ => (),
            },
            Parse::Csi(mut params) => {
                if ('\x40'..='\x7e').contains(&c) {
                    self.control(c, &params);
                } else if params.len() < 64 {
                    params.push(c);
                    self.parse = Parse::Csi(params);
                }
            }
        }
    }

    fn line(&mut self) -> &mut Line {
        if self.lines.is_empty() {
            self.lines.push_back(Line::default());
            self.row = 0;
        }
        &mut self.lines[self.row]
    }

    fn put(&mut self, c: char) {
        let (col, style) = (self.col, self.style);
        let line = self.line();
        line.reach(col);
        if col < line.chars.len() {
            line.chars[col] = (c, style);
        } else {
            line.chars.push((c, style));
        }
        self.col += 1;
    }

    /// Down a line, to its first column, making a new one at the bottom.
    fn linefeed(&mut self) {
        self.line();
        self.row += 1;
        self.col = 0;
        if self.row == self.lines.len() {
            self.lines.push_back(Line::default());
        }
        while self.lines.len() > MAX_LINES {
            self.lines.pop_front();
            self.row -= 1;
        }
    }

    fn control(&mut self, command: char, params: &str) {
        let args: Vec<u32> = params
            .trim_start_matches('?')
            .split(';')
            .map(|p| p.parse().unwrap_or(0))
            .collect();
        let n = |default: u32| match args.first() {
            Some(0) | None => default as usize,
            Some(&n) => n as usize,
        };
        match command {
            'A' => self.row = self.row.saturating_sub(n(1)),
            'B' => self.row = (self.row + n(1)).min(self.lines.len().saturating_sub(1)),
            'C' => self.col += n(1),
            'D' => self.col = self.col.saturating_sub(n(1)),
            'E' => {
                self.row = (self.row + n(1)).min(self.lines.len().saturating_sub(1));
                self.col = 0;
            }
            'F' => {
                self.row = self.row.saturating_sub(n(1));
                self.col = 0;
            }
            'G' => self.col = n(1) - 1,
            'K' => {
                let col = self.col;
                let line = self.line();
                match args.first().copied().unwrap_or(0) {
                    0 => line.chars.truncate(col),
                    1 => {
                        line.reach(col + 1);
                        line.chars[..=col].fill((' ', Style::default()));
                    }
                    _ => line.chars.clear(),
                }
            }
            '@' => {
                let (col, count) = (self.col, n(1));
                let line = self.line();
                if col < line.chars.len() {
                    line.chars.splice(
                        col..col,
                        std::iter::repeat_n((' ', Style::default()), count),
                    );
                }
            }
            'P' => {
                let (col, count) = (self.col, n(1));
                let line = self.line();
                if col < line.chars.len() {
                    let end = (col + count).min(line.chars.len());
                    line.chars.drain(col..end);
                }
            }
            'X' => {
                let (col, count) = (self.col, n(1));
                let line = self.line();
                let end = (col + count).min(line.chars.len());
                if col < end {
                    line.chars[col..end].fill((' ', Style::default()));
                }
            }
            'm' => {
                for &arg in &args {
                    match arg {
                        0 => self.style = Style::default(),
                        1 => self.style.bold = true,
                        22 => self.style.bold = false,
                        30..=37 | 39 => self.style.color = Color::from_sgr(arg),
                        _ => (),
                    }
                }
            }
            _ => (),
        }
    }
}

/// The console's level and the default level, from `/proc/sys/kernel/printk`'s first two fields
/// (`ply_get_kmsg_log_levels`).
pub fn printk_levels(printk: &str) -> Option<(u8, u8)> {
    let mut fields = printk.split_whitespace().map(|f| f.parse().ok());
    Some((fields.next()??, fields.next()??))
}

/// One `/dev/kmsg` record as the console would have shown it, coloured by priority
/// (`handle_kmsg_message`): `None` above the console's level (`console_level`, the first field of
/// `/proc/sys/kernel/printk`), as the console prints only what is more urgent than it.
pub fn kmsg_text(record: &str, console_level: u8, default_level: u8) -> Option<String> {
    let (fields, message) = record.split_once(';')?;
    let prefix: u32 = fields.split(',').next()?.parse().ok()?;
    let priority = if prefix > 0 {
        (prefix & 7) as u8
    } else {
        default_level
    };
    if priority > console_level {
        return None;
    }
    // Continuation lines (" KEY=value") follow the message; its own newlines are escaped.
    let message = message.split('\n').next().unwrap_or("");
    Some(record_text(priority, &unescape_kmsg(message)))
}

/// A log record of syslog `priority` as the details view shows it, a line each, coloured as
/// `ply-kmsg-reader.c` colours the kernel's: LOG_EMERG bold, the urgent ones red, warnings brown,
/// notices green.
pub fn record_text(priority: u8, message: &str) -> String {
    let bold = u8::from(priority < 1);
    let color = match priority {
        0..=3 => 31,
        4 => 33,
        5 => 32,
        _ => 39,
    };
    let mut text = String::new();
    for line in message.split('\n') {
        text.push_str(&format!("\x1b[0;{bold};{color}m{line}\x1b[0m\n"));
    }
    text
}

/// `/dev/kmsg`'s `\xNN` escapes back to bytes (`unhexmangle_to_buffer`).
fn unescape_kmsg(message: &str) -> String {
    let bytes = message.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && bytes.get(i + 1) == Some(&b'x') {
            if let Some(byte) = message
                .get(i + 2..i + 4)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
            {
                out.push(byte);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The viewer's monospace font: bgrt's `MonospaceFont` (`monospace 10`, pango points at 96 dpi), in
/// the shell's monospace rather than fontconfig's, as the rest of the splash is set in the shell's
/// sans (§4.4, §8.2).
pub const MONO_PX: f64 = 10. * 96. / 72.;

/// A styled run of a line on screen, its pen at `at.x` and its line box's top at `at.y`.
#[derive(Debug, Clone, PartialEq)]
pub struct Placed {
    pub text: String,
    pub style: Style,
    pub at: Point,
}

/// What the view shows on a `screen`, in character `cell`s (a space's advance and the font's line
/// height): the newest lines that fit, each wrapped at `width / cell − 1` characters, from the top,
/// half a cell in (`ply_console_viewer_new`, `ply_console_viewer_show`). `prompt` is a password
/// being asked for, as the viewer prints it: the prompt, a star per character, a cursor.
pub fn lay(
    console: &Console,
    prompt: Option<(&str, usize)>,
    screen: Size,
    cell: Size,
) -> Vec<Placed> {
    if cell.w <= 0 || cell.h <= 0 {
        return Vec::new();
    }
    let per_line = usize::try_from(screen.w / cell.w - 1).unwrap_or(0).max(1);
    let rows = usize::try_from(screen.h / cell.h).unwrap_or(0).max(1);

    let mut lines: Vec<Vec<Span>> = console.lines().collect();
    // The line under the cursor ends the text: trailing blank lines are a pending newline.
    while lines.last().is_some_and(Vec::is_empty) {
        lines.pop();
    }
    if let Some((prompt, bullets)) = prompt {
        lines.push(vec![Span {
            text: format!("{prompt}: {}_", "*".repeat(bullets)),
            style: Style::default(),
        }]);
    }

    // Wrap into screen rows, then keep the newest that fit.
    let mut wrapped: Vec<Vec<Span>> = Vec::new();
    for line in lines {
        let mut row: Vec<Span> = Vec::new();
        let mut used = 0;
        for span in line {
            let mut chars = span.text.chars().peekable();
            while chars.peek().is_some() {
                if used == per_line {
                    wrapped.push(std::mem::take(&mut row));
                    used = 0;
                }
                let text: String = chars.by_ref().take(per_line - used).collect();
                used += text.chars().count();
                row.push(Span {
                    text,
                    style: span.style,
                });
            }
        }
        wrapped.push(row);
    }
    let first = wrapped.len().saturating_sub(rows);

    let mut placed = Vec::new();
    for (i, row) in wrapped[first..].iter().enumerate() {
        let y = cell.h * i as i32;
        let mut col = 0;
        for span in row {
            let len = span.text.chars().count() as i32;
            // Blanks need no drawing; the rest is set where its column is.
            if !span.text.trim().is_empty() {
                placed.push(Placed {
                    text: span.text.clone(),
                    style: span.style,
                    at: Point::from((cell.w / 2 + col * cell.w, y)),
                });
            }
            col += len;
        }
    }
    placed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(console: &Console) -> Vec<String> {
        console
            .lines()
            .map(|spans| spans.iter().map(|s| s.text.as_str()).collect())
            .collect()
    }

    #[test]
    fn lines_break_and_return() {
        let mut c = Console::default();
        c.write(b"one\ntwo\r\nthree");
        assert_eq!(text(&c), ["one", "two", "three"]);
    }

    #[test]
    fn a_status_line_is_rewritten_in_place() {
        // systemd's ephemeral "A start job is running" line, then the result over it.
        let mut c = Console::default();
        c.write(b"[ ***  ] A start job is running for x (3s / no limit)");
        c.write(b"\r\x1b[K[  \x1b[0;32mOK\x1b[0m  ] Started x.\n");
        assert_eq!(text(&c), ["[  OK  ] Started x.", ""]);
        let spans: Vec<Span> = c.lines().next().unwrap();
        assert_eq!(spans[1].text, "OK");
        assert_eq!(spans[1].style.color, Color::Green);
        assert_eq!(spans[2].style, Style::default());
    }

    #[test]
    fn erase_and_move() {
        let mut c = Console::default();
        c.write(b"abcdef\x1b[3G\x1b[1K");
        assert_eq!(text(&c), ["   def"]);
        // The cursor stays at column 3 through the erase.
        c.write(b"\x1b[2KX");
        assert_eq!(text(&c), ["  X"]);
        c.write(b"\x1b[1G\x1b[2PZ");
        assert_eq!(text(&c), ["Z"]);
        c.write(b"\ta");
        assert_eq!(text(&c), ["Z       a"]);
    }

    #[test]
    fn utf8_split_across_writes() {
        let mut c = Console::default();
        let s = "ação".as_bytes();
        c.write(&s[..2]);
        c.write(&s[2..]);
        assert_eq!(text(&c), ["ação"]);
    }

    #[test]
    fn old_lines_go() {
        let mut c = Console::default();
        for i in 0..MAX_LINES + 10 {
            c.write(format!("{i}\n").as_bytes());
        }
        let lines = text(&c);
        assert_eq!(lines.len(), MAX_LINES);
        assert_eq!(lines[MAX_LINES - 2], (MAX_LINES + 9).to_string());
    }

    #[test]
    fn crosses_a_handover() {
        let mut c = Console::default();
        c.write(b"\x1b[31mred\x1b[0m plain\nnext");
        let json = serde_json::to_string(&c).unwrap();
        let mut back: Console = serde_json::from_str(&json).unwrap();
        assert_eq!(text(&back), text(&c));
        assert_eq!(back.lines().next(), c.lines().next());
        // The cursor too: the line goes on where it was.
        back.write(b" line");
        assert_eq!(text(&back), ["red plain", "next line"]);
    }

    #[test]
    fn kmsg_records_by_the_console_level() {
        // Under `quiet`, console level 4: errors are shown, warnings are not.
        let err = "3,812,1234567,-;virtio_gpu: oops\n SUBSYSTEM=virtio\n";
        assert_eq!(
            kmsg_text(err, 4, 4).as_deref(),
            Some("\x1b[0;0;31mvirtio_gpu: oops\x1b[0m\n")
        );
        assert_eq!(kmsg_text("4,813,1234568,-;careful", 3, 4), None);
        // A prefix of 0 counts as the default level, as plymouth reads it.
        assert!(kmsg_text("0,1,2,-;x", 4, 4).is_some());
        assert_eq!(kmsg_text("0,1,2,-;x", 3, 4), None);
        assert_eq!(
            kmsg_text("6,1,2,-;a\\x0ab", 7, 4).as_deref(),
            Some("\x1b[0;0;39ma\x1b[0m\n\x1b[0;0;39mb\x1b[0m\n")
        );
        assert_eq!(printk_levels("3\t4\t1\t7\n"), Some((3, 4)));
    }

    #[test]
    fn the_view_shows_the_newest_lines_that_fit() {
        let mut c = Console::default();
        for i in 0..10 {
            c.write(format!("line {i}\n").as_bytes());
        }
        // 4 rows of 16 px, 8 px cells: 9 characters a row.
        let placed = lay(&c, None, Size::from((80, 64)), Size::from((8, 16)));
        let rows: Vec<(&str, Point)> = placed.iter().map(|p| (p.text.as_str(), p.at)).collect();
        assert_eq!(
            rows,
            [
                ("line 6", Point::from((4, 0))),
                ("line 7", Point::from((4, 16))),
                ("line 8", Point::from((4, 32))),
                ("line 9", Point::from((4, 48))),
            ]
        );
    }

    #[test]
    fn long_lines_wrap_and_styles_keep_their_columns() {
        let mut c = Console::default();
        c.write(b"[  \x1b[32mOK\x1b[0m  ] Started a long unit name.");
        let placed = lay(&c, None, Size::from((80, 64)), Size::from((8, 16)));
        let rows: Vec<(&str, Point, Color)> = placed
            .iter()
            .map(|p| (p.text.as_str(), p.at, p.style.color))
            .collect();
        assert_eq!(
            rows,
            [
                ("[  ", Point::from((4, 0)), Color::Default),
                ("OK", Point::from((4 + 3 * 8, 0)), Color::Green),
                ("  ] ", Point::from((4 + 5 * 8, 0)), Color::Default),
                ("Started a", Point::from((4, 16)), Color::Default),
                (" long uni", Point::from((4, 32)), Color::Default),
                ("t name.", Point::from((4, 48)), Color::Default),
            ]
        );
    }

    #[test]
    fn a_prompt_is_the_last_line() {
        let mut c = Console::default();
        c.write(b"booting\n");
        let placed = lay(
            &c,
            Some(("Passphrase", 3)),
            Size::from((400, 64)),
            Size::from((8, 16)),
        );
        let last = placed.last().unwrap();
        assert_eq!(last.text, "Passphrase: ***_");
        assert_eq!(last.at, Point::from((4, 16)));
    }
}
