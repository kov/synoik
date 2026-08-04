// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Encoder backends behind the [`EncoderBackend`] seam.
//!
//! Slice 1 ships [`FfmpegEncoder`]: captured RGBA frames are streamed to an `ffmpeg` subprocess
//! that encodes VP8 into WebM. Running the encoder out-of-process keeps its heap churn and any
//! codec bug out of the compositor (the isolation the design calls for), and sidesteps linking a
//! libvpx binding into the process. [`ThreadedRecorder`] runs any backend on a worker thread fed by
//! a bounded channel — the channel is the seam, so an in-process encoder can replace ffmpeg later
//! with no change to the capture side.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};

use anyhow::Context as _;

use super::{RecordConfig, RecordFrame};

/// Sink for captured frames. Implementors encode + mux; the compositor never calls these directly
/// (it goes through [`ThreadedRecorder`]), so `push`/`finish` may block.
pub trait EncoderBackend: Send {
    /// Encode and mux one frame. Errors are fatal to the recording.
    fn push(&mut self, frame: RecordFrame) -> anyhow::Result<()>;
    /// Flush the encoder, finalize the file, and clean up. Consumes the backend.
    fn finish(self: Box<Self>) -> anyhow::Result<()>;
}

/// Streams RGBA frames to an `ffmpeg` subprocess encoding VP8-in-WebM.
///
/// The input is fixed-rate (`-framerate fps`), so ffmpeg spaces frames at `1/fps` regardless of
/// when they were captured. Left there, a *dropped* frame (encoder behind → the capture side
/// discards it) would silently compress wall-clock time: the encoder counts only frames it
/// received. To keep the recording's duration truthful, [`FfmpegEncoder::push`] uses each frame's
/// monotonic `pts` to detect gaps and repeats the previous frame into the missed slots, so the
/// emitted frame count always matches elapsed real time. Output stays constant-rate (universally
/// playable) while honoring the capture timeline.
pub struct FfmpegEncoder {
    child: Child,
    stdin: Option<ChildStdin>,
    /// Drains ffmpeg's stderr so it can't deadlock on a full pipe; joined at finish for
    /// diagnostics.
    stderr: Option<JoinHandle<Vec<u8>>>,
    path: PathBuf,
    /// Expected bytes per frame (`width * height * 4`).
    frame_bytes: usize,
    /// Configured framerate, used to map a frame's `pts` to its constant-rate slot index.
    fps: f64,
    /// The most recently written frame, repeated to fill slots lost to drops.
    last: Option<Vec<u8>>,
    frames: u64,
}

/// Per-`push` cap on synthesized fill frames, bounding the burst written to ffmpeg's stdin in one
/// call (~10s at 30fps). It does *not* cap total fill: a deficit larger than this is worked off
/// over subsequent pushes, so wall-clock duration stays truthful — it just isn't dumped all at
/// once.
const MAX_GAP_FILL: u64 = 300;

impl FfmpegEncoder {
    /// Spawn ffmpeg writing VP8/WebM to `path`. Fails if ffmpeg is not installed or refuses the
    /// parameters.
    pub fn new(path: &Path, config: RecordConfig) -> anyhow::Result<Self> {
        anyhow::ensure!(
            config.width > 0 && config.height > 0,
            "recording dimensions must be positive, got {}x{}",
            config.width,
            config.height,
        );

        let mut child = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y"])
            // Raw RGBA frames on stdin at a fixed rate.
            .args(["-f", "rawvideo", "-pixel_format", "rgba"])
            .args([
                "-video_size",
                &format!("{}x{}", config.width, config.height),
            ])
            .args(["-framerate", &config.fps.max(1).to_string()])
            .args(["-i", "pipe:0"])
            // VP8/WebM, realtime tuning for live capture. yuv420p for broad player support.
            .args([
                "-c:v",
                "libvpx",
                "-b:v",
                &format!("{}k", config.bitrate_kbps),
            ])
            .args(["-deadline", "realtime", "-cpu-used", "4"])
            .args(["-pix_fmt", "yuv420p"])
            .arg(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawning ffmpeg (is it installed?)")?;

        let stdin = child.stdin.take().expect("piped stdin");
        let mut stderr_pipe = child.stderr.take().expect("piped stderr");
        let stderr = thread::Builder::new()
            .name("synoik-recorder-ffmpeg-err".to_owned())
            .spawn(move || {
                let mut buf = Vec::new();
                let _ = stderr_pipe.read_to_end(&mut buf);
                buf
            })
            .expect("spawning the ffmpeg stderr drain");

        Ok(Self {
            child,
            stdin: Some(stdin),
            stderr: Some(stderr),
            path: path.to_owned(),
            frame_bytes: (config.width * config.height * 4) as usize,
            fps: config.fps.max(1) as f64,
            last: None,
            frames: 0,
        })
    }

    /// Write one raw RGBA frame to ffmpeg's stdin, surfacing its stderr on a broken pipe.
    fn write_frame(&mut self, rgba: &[u8]) -> anyhow::Result<()> {
        let stdin = self.stdin.as_mut().context("ffmpeg stdin already closed")?;
        if let Err(err) = stdin.write_all(rgba) {
            // A broken pipe means ffmpeg died; surface its stderr.
            let details = self.take_stderr();
            return Err(anyhow::Error::new(err)).context(format!(
                "writing a frame to ffmpeg failed. ffmpeg said: {details}"
            ));
        }
        self.frames += 1;
        Ok(())
    }

    /// Collect ffmpeg's stderr (joining the drain thread) for error messages.
    fn take_stderr(&mut self) -> String {
        match self.stderr.take().map(|h| h.join()) {
            Some(Ok(bytes)) => String::from_utf8_lossy(&bytes).trim().to_owned(),
            _ => String::new(),
        }
    }
}

impl EncoderBackend for FfmpegEncoder {
    fn push(&mut self, frame: RecordFrame) -> anyhow::Result<()> {
        anyhow::ensure!(
            frame.rgba.len() == self.frame_bytes,
            "frame is {} bytes, expected {} for the configured size",
            frame.rgba.len(),
            self.frame_bytes,
        );

        // This frame belongs in the constant-rate slot nearest its capture time. If drops left
        // earlier slots empty, repeat the previous frame into them so the timeline stays honest.
        let slot = (frame.pts.as_secs_f64() * self.fps).round() as u64;
        if let Some(last) = self.last.take() {
            let deficit = slot.saturating_sub(self.frames);
            if deficit > self.fps as u64 {
                debug!(
                    "recorder filling a {deficit}-frame gap (dropped frames or a capture stall)"
                );
            }
            let fill = deficit.min(MAX_GAP_FILL);
            for _ in 0..fill {
                self.write_frame(&last)?;
            }
            self.last = Some(last);
        }

        self.write_frame(&frame.rgba)?;
        self.last = Some(frame.rgba);
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> anyhow::Result<()> {
        // Closing stdin lets ffmpeg flush and exit.
        self.stdin.take();
        let status = self.child.wait().context("waiting for ffmpeg")?;
        let details = self.take_stderr();
        anyhow::ensure!(
            status.success(),
            "ffmpeg exited with {status} after {} frames ({}). stderr: {details}",
            self.frames,
            self.path.display(),
        );
        Ok(())
    }
}

impl Drop for FfmpegEncoder {
    fn drop(&mut self) {
        // If dropped without finish() (e.g. a panic), don't leak the subprocess.
        self.stdin.take();
        let _ = self.child.wait();
        self.stderr.take();
    }
}

/// Runs an [`EncoderBackend`] on a worker thread, fed by a bounded channel. The compositor keeps
/// only the sender; when the encoder falls behind, [`ThreadedRecorder::try_push`] drops the frame
/// rather than stalling the compositor thread.
pub struct ThreadedRecorder {
    tx: Option<SyncSender<RecordFrame>>,
    handle: Option<JoinHandle<anyhow::Result<()>>>,
    /// Frames dropped because the channel was full (encoder behind).
    dropped: u64,
}

/// Outcome of a non-blocking [`ThreadedRecorder::try_push`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PushResult {
    Sent,
    /// Encoder is behind; frame discarded.
    Dropped,
    /// Worker has exited (finished or died).
    Disconnected,
}

impl ThreadedRecorder {
    /// Spawn the worker. `capacity` bounds the in-flight frame queue.
    pub fn spawn(mut backend: Box<dyn EncoderBackend>, capacity: usize) -> Self {
        let (tx, rx) = sync_channel::<RecordFrame>(capacity.max(1));
        let handle = thread::Builder::new()
            .name("synoik-recorder".to_owned())
            .spawn(move || {
                while let Ok(frame) = rx.recv() {
                    backend.push(frame)?;
                }
                backend.finish()
            })
            .expect("spawning the recorder worker thread");
        Self {
            tx: Some(tx),
            handle: Some(handle),
            dropped: 0,
        }
    }

    /// Non-blocking hand-off. Drops the frame if the queue is full.
    pub fn try_push(&mut self, frame: RecordFrame) -> PushResult {
        let Some(tx) = &self.tx else {
            return PushResult::Disconnected;
        };
        match tx.try_send(frame) {
            Ok(()) => PushResult::Sent,
            Err(TrySendError::Full(_)) => {
                self.dropped += 1;
                PushResult::Dropped
            }
            Err(TrySendError::Disconnected(_)) => PushResult::Disconnected,
        }
    }

    pub fn dropped_frames(&self) -> u64 {
        self.dropped
    }

    /// Stop recording and finalize the file, joining the worker.
    pub fn finish(mut self) -> anyhow::Result<()> {
        // Closing the channel makes the worker drain and finalize.
        self.tx.take();
        match self.handle.take() {
            Some(handle) => handle
                .join()
                .map_err(|_| anyhow::anyhow!("recorder worker panicked"))?,
            None => Ok(()),
        }
    }

    /// Finalize without blocking the caller. Closing the channel lets the worker drain its queue
    /// and finalize the file on its own thread; the join (which waits for ffmpeg to flush and exit)
    /// is handed to a short detached thread that logs the outcome. Use this on the compositor
    /// thread, where a stop-click must not stall on encoder drain + WebM finalize.
    ///
    /// `label` names the recording in the log line. Returns the reaper's handle so tests can await
    /// finalization deterministically; production drops it (the thread runs to completion
    /// detached).
    pub fn finish_async(mut self, label: String) -> Option<JoinHandle<()>> {
        self.tx.take();
        let handle = self.handle.take()?;
        thread::Builder::new()
            .name("synoik-recorder-finalize".to_owned())
            .spawn(move || match handle.join() {
                Ok(Ok(())) => info!("saved screen recording to {label}"),
                Ok(Err(err)) => warn!("error finalizing recording {label}: {err:?}"),
                Err(_) => warn!("recorder worker panicked finalizing {label}"),
            })
            .ok()
    }
}

impl Drop for ThreadedRecorder {
    fn drop(&mut self) {
        // If dropped without finish(), still tear the worker down cleanly.
        self.tx.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::BufReader;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use matroska_demuxer::{MatroskaFile, TrackType};

    use super::*;

    fn ffmpeg_available() -> bool {
        Command::new("ffmpeg")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// A distinct temp path per call (no Date/random available in this environment).
    fn temp_path(tag: &str) -> std::path::PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "synoik-rec-test-{}-{tag}-{n}.webm",
            std::process::id()
        ))
    }

    /// Synthetic RGBA frame: a solid color that shifts per frame so encoding isn't a no-op.
    fn frame(w: u32, h: u32, i: u32) -> Vec<u8> {
        let (r, g, b) = (
            (i * 17 % 256) as u8,
            (i * 53 % 256) as u8,
            (i * 97 % 256) as u8,
        );
        let mut v = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..w * h {
            v.extend_from_slice(&[r, g, b, 255]);
        }
        v
    }

    fn config(w: u32, h: u32) -> RecordConfig {
        RecordConfig {
            width: w,
            height: h,
            fps: 30,
            bitrate_kbps: 2000,
        }
    }

    fn assert_valid_webm(path: &std::path::Path, w: u32, h: u32, min_frames: u64) {
        let file = std::fs::File::open(path).unwrap();
        let mut mkv = MatroskaFile::open(BufReader::new(file)).expect("parse as matroska");
        let track = mkv
            .tracks()
            .iter()
            .find(|t| t.track_type() == TrackType::Video)
            .expect("a video track");
        assert_eq!(track.codec_id(), "V_VP8", "codec");
        let video = track.video().expect("video settings");
        assert_eq!(video.pixel_width().get(), w as u64, "width");
        assert_eq!(video.pixel_height().get(), h as u64, "height");

        let mut frame = matroska_demuxer::Frame::default();
        let (mut count, mut last_ts) = (0u64, 0u64);
        while mkv.next_frame(&mut frame).expect("read frame") {
            assert!(frame.timestamp >= last_ts, "timestamps must be monotonic");
            last_ts = frame.timestamp;
            count += 1;
        }
        assert!(
            count >= min_frames,
            "got {count} frames, want >= {min_frames}"
        );
    }

    #[test]
    fn vp8_webm_roundtrips_through_an_independent_demuxer() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not installed");
            return;
        }
        let (w, h, n) = (64, 48, 15);
        let path = temp_path("sync");
        let mut enc = Box::new(FfmpegEncoder::new(&path, config(w, h)).unwrap());
        for i in 0..n {
            enc.push(RecordFrame {
                rgba: frame(w, h, i),
                pts: Duration::from_millis((i * 1000 / 30) as u64),
            })
            .unwrap();
        }
        enc.finish().unwrap();

        assert_valid_webm(&path, w, h, 10);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn threaded_recorder_finalizes_a_valid_file() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not installed");
            return;
        }
        let (w, h, n) = (32, 24, 20);
        let path = temp_path("threaded");
        let backend = Box::new(FfmpegEncoder::new(&path, config(w, h)).unwrap());
        let mut rec = ThreadedRecorder::spawn(backend, 8);
        for i in 0..n {
            // A tiny queue + no consumer wait means some frames may drop; that's fine.
            let _ = rec.try_push(RecordFrame {
                rgba: frame(w, h, i),
                pts: Duration::from_millis((i * 1000 / 30) as u64),
            });
        }
        rec.finish().unwrap();

        assert_valid_webm(&path, w, h, 1);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_pts_gap_is_filled_so_duration_is_preserved() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not installed");
            return;
        }
        // Push only two frames but stamp the second a full second (30 slots) later, as if 29
        // frames had been dropped. The gap-fill should synthesize the missing slots so the file
        // holds ~30 frames, not 2 — i.e. one second of video, not two frames' worth.
        let (w, h) = (32, 24);
        let path = temp_path("gap");
        let mut enc = Box::new(FfmpegEncoder::new(&path, config(w, h)).unwrap());
        enc.push(RecordFrame {
            rgba: frame(w, h, 0),
            pts: Duration::ZERO,
        })
        .unwrap();
        enc.push(RecordFrame {
            rgba: frame(w, h, 1),
            pts: Duration::from_secs(1),
        })
        .unwrap();
        enc.finish().unwrap();

        // 30 filled slots + the final frame ≈ 31; demand clearly more than the 2 pushed.
        assert_valid_webm(&path, w, h, 25);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn finish_async_finalizes_a_valid_file() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not installed");
            return;
        }
        let (w, h, n) = (32, 24, 20);
        let path = temp_path("async");
        let backend = Box::new(FfmpegEncoder::new(&path, config(w, h)).unwrap());
        let mut rec = ThreadedRecorder::spawn(backend, 8);
        for i in 0..n {
            let _ = rec.try_push(RecordFrame {
                rgba: frame(w, h, i),
                pts: Duration::from_millis((i * 1000 / 30) as u64),
            });
        }
        // Detached finalize: join the reaper so the assertion is deterministic. Production drops
        // it.
        rec.finish_async(path.display().to_string())
            .expect("reaper thread spawned")
            .join()
            .expect("reaper joined");

        assert_valid_webm(&path, w, h, 1);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn rejects_wrong_sized_frames() {
        if !ffmpeg_available() {
            eprintln!("skipping: ffmpeg not installed");
            return;
        }
        let path = temp_path("badsize");
        let mut enc = Box::new(FfmpegEncoder::new(&path, config(32, 32)).unwrap());
        let err = enc
            .push(RecordFrame {
                rgba: vec![0; 10],
                pts: Duration::ZERO,
            })
            .unwrap_err();
        assert!(err.to_string().contains("expected"), "got: {err}");
        // Clean up the (empty) file ffmpeg may have created.
        drop(enc);
        std::fs::remove_file(&path).ok();
    }
}
