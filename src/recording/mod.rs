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

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context as _;

/// Default output path for a recording:
/// `$XDG_VIDEOS_DIR/Screencasts/niri-recording-<unix-secs>.webm`, falling back to `$HOME/Videos`.
/// Creates the directory. (GNOME saves screencasts under `$VIDEOS/Screencasts`;
/// `screencastService.js`.)
pub fn default_recording_path() -> anyhow::Result<PathBuf> {
    let base = std::env::var_os("XDG_VIDEOS_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Videos")))
        .context("neither XDG_VIDEOS_DIR nor HOME is set")?;
    let dir = base.join("Screencasts");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok(dir.join(format!("niri-recording-{secs}.webm")))
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
