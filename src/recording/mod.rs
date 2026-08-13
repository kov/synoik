// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Built-in screen recorder.
//!
//! Frame *capture* lives in the compositor (it renders the same
//! `RenderTarget::Screencast` elements the screencast path uses and reads them back), so it is
//! backend-agnostic and testable headless. *Encoding* lives behind the
//! [`encoder::EncoderBackend`] seam, fed by a bounded channel via
//! [`encoder::ThreadedRecorder`] — the channel is the seam, so the encoder's location (subprocess
//! vs in-process) can change without touching the capture path.
//!
//! Slice 1 ships [`encoder::FfmpegEncoder`], which streams frames to an `ffmpeg` subprocess
//! encoding VP8/WebM. Out-of-process encoding keeps the codec's heap churn and any bug out of the
//! compositor. (An in-process libvpx backend is deferred: the available Rust bindings don't match
//! the host's libvpx 1.15 ABI.) See `docs/fork/panel-status-port.md` and the `recordings` ledger in
//! [`crate::screencasting`] that drives the R1 panel indicator.

pub mod encoder;

use std::ffi::CStr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context as _;

/// Default output path for the keybind/pill trigger, matching GNOME's default exactly:
/// `<Videos>/Screencasts/Screencast From <date> <time>.webm` (the same template gnome-shell's UI
/// sends over D-Bus; `screenshot.js` builds `Screencasts/Screencast From %d %t`).
/// Creates the directory.
///
/// `base` overrides where a relative template lands (see [`resolve_file_template`]); production
/// passes `None`.
pub fn default_recording_path(base: Option<&std::path::Path>) -> anyhow::Result<PathBuf> {
    resolve_file_template("Screencasts/Screencast From %d %t", "webm", base)
}

/// Resolve a `org.gnome.Shell.Screencast` file template into an absolute path, matching
/// gnome-shell's `screencastService.js` algorithm (50.1):
/// - a trailing `.webm` is stripped (a compat shim; the real extension is appended here);
/// - `%d` expands to the local date `YYYY-MM-DD`, `%t` to the local time `HH-MM-SS`, `%%` to `%`,
///   and any other `%x` escape is dropped (it is NOT strftime);
/// - a relative result lands under `base`, else the XDG **Videos** user directory (else `$HOME`) —
///   see [`recordings_base`]. `base` exists so a test can drive the real capture path without
///   writing into the developer's actual `~/Videos/Screencasts`, which it did, once per suite run;
/// - `extension` is appended and the parent directory is created.
///
/// The default template gnome-shell's UI sends is `Screencasts/Screencast From %d %t`.
pub fn resolve_file_template(
    template: &str,
    extension: &str,
    base: Option<&std::path::Path>,
) -> anyhow::Result<PathBuf> {
    let template = template.strip_suffix(".webm").unwrap_or(template);

    let (date, time) = local_date_time().context("reading the local time for the template")?;
    let stem = expand_template(template, &date, &time);

    let mut path = PathBuf::from(&stem);
    if path.is_relative() {
        let base = match base {
            Some(base) => base.to_owned(),
            None => {
                let dirs = directories::UserDirs::new()
                    .context("error retrieving the user directories")?;
                recordings_base(dirs.video_dir(), dirs.home_dir())
            }
        };
        path = base.join(path);
    }

    // Append the real extension (the template's stem never carries it).
    let mut os = path.into_os_string();
    os.push(".");
    os.push(extension);
    let path = PathBuf::from(os);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    Ok(path)
}

/// Where a relative screencast template lands: the XDG **Videos** user directory, else `$HOME` —
/// `_getAbsolutePath` (`screencastService.js:585-594`), which is
/// `g_get_user_special_dir(DIRECTORY_VIDEOS) || g_get_home_dir()`.
///
/// **Not the `XDG_VIDEOS_DIR` environment variable**, which is what this used to read. That
/// variable is not part of the running session's contract: the user directories are declared in
/// `~/.config/user-dirs.dirs`, which is what glib reads and what `directories::UserDirs` parses.
/// The variable is unset in an ordinary session, so every recording fell through to `$HOME` and
/// landed in `~/Screencasts` instead of `~/Videos/Screencasts`. Reading the file also gets the
/// localized directory names right — a `pt_BR` session's videos live in `~/Vídeos`, and no amount
/// of guessing English names finds them.
///
/// Pure, so the fallback is testable without touching the process environment (which in a parallel
/// test binary is a flake generator).
fn recordings_base(videos: Option<&std::path::Path>, home: &std::path::Path) -> PathBuf {
    videos.unwrap_or(home).to_owned()
}

/// Expand the `%d`/`%t`/`%%` escapes of a screencast template. Pure, for testability; the caller
/// supplies the already-formatted `date`/`time`. Unknown escapes are dropped (matching
/// gnome-shell).
fn expand_template(template: &str, date: &str, time: &str) -> String {
    let mut out = String::with_capacity(template.len());
    let mut chars = template.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('%') => out.push('%'),
            Some('d') => out.push_str(date),
            Some('t') => out.push_str(time),
            Some(other) => warn!("ignoring unknown screencast template escape %{other}"),
            None => out.push('%'),
        }
    }
    out
}

/// Local date (`YYYY-MM-DD`) and time (`HH-MM-SS`) via libc, matching gnome-shell's `%d`/`%t`.
fn local_date_time() -> anyhow::Result<(String, String)> {
    // SAFETY: localtime returns a pointer into a static buffer; read it before any other libc time
    // call. strftime writes at most `buf.len()` bytes and returns the count (0 on overflow).
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        anyhow::ensure!(t != -1, "time() failed");
        let tm = libc::localtime(&t);
        anyhow::ensure!(!tm.is_null(), "localtime() failed");
        let date = strftime(tm, c"%Y-%m-%d")?;
        let time = strftime(tm, c"%H-%M-%S")?;
        Ok((date, time))
    }
}

unsafe fn strftime(tm: *const libc::tm, fmt: &CStr) -> anyhow::Result<String> {
    let mut buf = [0u8; 64];
    let n = libc::strftime(buf.as_mut_ptr().cast(), buf.len(), fmt.as_ptr(), tm);
    anyhow::ensure!(n != 0, "strftime failed");
    Ok(String::from_utf8_lossy(&buf[..n]).into_owned())
}

/// Fixed parameters of a recording, chosen when it starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordConfig {
    /// Frame width in pixels (must be even for 4:2:0).
    pub width: u32,
    /// Frame height in pixels (must be even for 4:2:0).
    pub height: u32,
    /// Target framerate cap; the capture side paces to this.
    pub fps: u32,
    /// Target video bitrate in kbit/s.
    pub bitrate_kbps: u32,
}

/// One captured frame handed to an [`encoder::EncoderBackend`].
///
/// The buffer is packed RGBA8, tightly packed (`width * height * 4` bytes, row stride
/// `width * 4`). The capture side reads back in RGBA order directly, so no swizzle is needed here.
pub struct RecordFrame {
    pub rgba: Vec<u8>,
    /// Presentation time measured from the start of the recording.
    pub pts: Duration,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_expands_gnome_escapes() {
        let d = "2026-07-19";
        let t = "11-21-29";
        assert_eq!(
            expand_template("Screencast From %d %t", d, t),
            "Screencast From 2026-07-19 11-21-29"
        );
        // %% is a literal percent; an unknown escape is dropped.
        assert_eq!(expand_template("a%%b%zc", d, t), "a%bc");
        // No escapes pass through untouched; a trailing % is kept.
        assert_eq!(expand_template("plain name", d, t), "plain name");
        assert_eq!(expand_template("trailing%", d, t), "trailing%");
    }

    #[test]
    fn resolve_appends_extension_and_keeps_absolute_paths() {
        let dir = std::env::temp_dir().join(format!("synoik-tmpl-{}", std::process::id()));
        let template = dir.join("rec %%").to_string_lossy().into_owned();
        let path = resolve_file_template(&template, "webm", None).unwrap();
        // Absolute template is kept; `%%` collapses to `%`; the real extension is appended.
        assert_eq!(path, dir.join("rec %.webm"));
        // The parent directory is created.
        assert!(dir.is_dir());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A relative template lands in the *Videos* directory, and only falls back to `$HOME` when
    /// there is no such directory at all.
    ///
    /// The fallback used to be the whole behaviour: the base came from `$XDG_VIDEOS_DIR`, which no
    /// session sets, so recordings piled up in `~/Screencasts` instead of `~/Videos/Screencasts`.
    #[test]
    fn a_relative_recording_lands_in_the_videos_directory() {
        let home = std::path::Path::new("/home/someone");
        let videos = std::path::Path::new("/home/someone/Vídeos");

        assert_eq!(recordings_base(Some(videos), home), videos);
        assert_eq!(
            recordings_base(None, home),
            home,
            "with no Videos directory declared, GNOME uses the home directory"
        );
    }

    #[test]
    fn resolve_strips_a_trailing_dot_webm_from_the_template() {
        let dir = std::env::temp_dir().join(format!("synoik-tmpl2-{}", std::process::id()));
        let template = format!("{}.webm", dir.join("clip").to_string_lossy());
        let path = resolve_file_template(&template, "webm", None).unwrap();
        assert_eq!(path, dir.join("clip.webm"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
