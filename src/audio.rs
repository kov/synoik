//! The default audio sink's live state (volume + mute) for the panel output
//! indicator and the quick-settings volume slider.
//!
//! GNOME's `js/ui/status/volume.js` shows the default sink's volume as a symbolic
//! icon in the top-right cluster and as a slider in the quick-settings menu, fed
//! from `gvc` (libgvc → PulseAudio). This is the fork-owned model those resolve
//! from: a plain data snapshot updated by the PipeWire watcher
//! (`src/pipewire_audio.rs`, feature `pipewire`) over a calloop channel — the same
//! model→channel shape as [`crate::system_status`]. The model itself carries no
//! rendering or PipeWire dependency (it compiles without the audio backend, where
//! it simply stays absent).

/// Perceptual volume ceiling for the slider/scroll. GNOME caps the default sink at
/// 100% unless `allow-volume-above-100-percent` is set (then 150%,
/// `get_vol_max_amplified`); we start at 100% and can lift this later.
pub const MAX_VOLUME: f64 = 1.0;

/// Scroll-wheel volume step, GNOME's `SLIDER_SCROLL_STEP` (`js/ui/slider.js`): 2%.
pub const SCROLL_STEP: f64 = 0.02;

/// A snapshot of the default sink, in GNOME's **perceptual (cubic)** volume space —
/// the space the panel slider and `pactl`/gvc percentages live in, *not* PipeWire's
/// linear `channelVolumes` (convert with [`pw_linear_to_volume`] /
/// [`volume_to_pw_linear`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioStatus {
    /// Perceptual volume, `0.0..=MAX_VOLUME` (may exceed 1.0 if amplified).
    pub volume: f64,
    pub muted: bool,
}

impl Default for AudioStatus {
    fn default() -> Self {
        Self {
            volume: 0.0,
            muted: false,
        }
    }
}

/// A snapshot of microphone (input) activity for the panel privacy indicator — the fork's model
/// behind gnome-shell's `InputIndicator` (`js/ui/status/volume.js`). Fed by the PipeWire watcher;
/// carries no rendering or PipeWire dependency (stays [`Default`] — not recording — where the audio
/// backend is absent, e.g. headless).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MicStatus {
    /// A non-skipped application is actively capturing (a running input stream).
    pub recording: bool,
    /// The default source is muted — a muted mic is no privacy concern, so the panel drops the
    /// tint. Defaults `false` (→ tinted) when the mute state is unknown (no source/metadata): an
    /// active capture whose mute we can't read is still a privacy event, so understating it white
    /// would be wrong. This diverges from gnome-shell, which shows un-tinted when there's no
    /// stream.
    pub muted: bool,
}

/// Apps the mic privacy indicator ignores — they open capture only to display input levels, so
/// they aren't a real recording. Matches gnome-shell's `_maybeShowInput` skip list
/// (`js/ui/status/volume.js`), compared against `application.id` only (never `application.name`).
pub const MIC_SKIP_APP_IDS: &[&str] = &["org.gnome.VolumeControl", "org.PulseAudio.pavucontrol"];

/// Whether any non-skipped application is actively recording, given `(application.id, running)` for
/// each input-capture stream. Pure, so the PipeWire recording signal can be unit-tested. A stream
/// counts only when its node is in the `Running` state (an idle/corked stream — e.g. a browser
/// holding an open-but-paused mic — must not pin the indicator).
pub fn is_recording<'a>(streams: impl IntoIterator<Item = (Option<&'a str>, bool)>) -> bool {
    streams.into_iter().any(|(app_id, running)| {
        running && !app_id.is_some_and(|id| MIC_SKIP_APP_IDS.contains(&id))
    })
}

/// The symbolic icon for the current output volume, mirroring gnome-shell's
/// `StreamSlider.getIcon`: muted (or ≤0) shows the muted glyph; otherwise the level
/// buckets into low/medium/high at the ⅓ and ⅔ marks (`n = clamp(ceil(3·v), 1, 3)`).
pub fn volume_icon(status: &AudioStatus) -> &'static str {
    const ICONS: [&str; 4] = [
        "audio-volume-muted-symbolic",
        "audio-volume-low-symbolic",
        "audio-volume-medium-symbolic",
        "audio-volume-high-symbolic",
    ];
    if status.muted || status.volume <= 0.0 {
        return ICONS[0];
    }
    let n = (3.0 * status.volume).ceil() as i64;
    ICONS[n.clamp(1, 3) as usize]
}

/// PipeWire node `channelVolumes` are **linear** amplitude; GNOME/PulseAudio present
/// a **perceptual (cubic)** value — e.g. `pactl` "40%" is `0.4³ ≈ 0.064` linear
/// (−23.88 dB). Convert a linear channel volume to the perceptual value the slider
/// uses.
pub fn pw_linear_to_volume(linear: f64) -> f64 {
    linear.max(0.0).cbrt()
}

/// Inverse of [`pw_linear_to_volume`]: a perceptual slider value → the linear
/// `channelVolumes` amplitude PipeWire wants.
pub fn volume_to_pw_linear(volume: f64) -> f64 {
    volume.max(0.0).powi(3)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(volume: f64, muted: bool) -> AudioStatus {
        AudioStatus { volume, muted }
    }

    #[test]
    fn icon_buckets_match_gnome_thresholds() {
        assert_eq!(volume_icon(&at(0.5, true)), "audio-volume-muted-symbolic");
        assert_eq!(volume_icon(&at(0.0, false)), "audio-volume-muted-symbolic");
        // (0, 1/3] → low
        assert_eq!(volume_icon(&at(0.01, false)), "audio-volume-low-symbolic");
        assert_eq!(
            volume_icon(&at(1.0 / 3.0, false)),
            "audio-volume-low-symbolic"
        );
        // (1/3, 2/3] → medium
        assert_eq!(
            volume_icon(&at(0.34, false)),
            "audio-volume-medium-symbolic"
        );
        assert_eq!(
            volume_icon(&at(2.0 / 3.0, false)),
            "audio-volume-medium-symbolic"
        );
        // (2/3, 1] → high, and amplified still clamps to high
        assert_eq!(volume_icon(&at(0.67, false)), "audio-volume-high-symbolic");
        assert_eq!(volume_icon(&at(1.0, false)), "audio-volume-high-symbolic");
        assert_eq!(volume_icon(&at(1.5, false)), "audio-volume-high-symbolic");
    }

    #[test]
    fn recording_requires_a_running_non_skipped_stream() {
        // No streams, or only idle ones → not recording.
        assert!(!is_recording(std::iter::empty()));
        assert!(!is_recording([(Some("org.mozilla.firefox"), false)]));
        // A running non-skipped stream → recording.
        assert!(is_recording([(Some("org.mozilla.firefox"), true)]));
        assert!(is_recording([(None, true)])); // native clients often have no application.id
                                               // Skipped apps don't count even while running…
        assert!(!is_recording([(Some("org.PulseAudio.pavucontrol"), true)]));
        assert!(!is_recording([(Some("org.gnome.VolumeControl"), true)]));
        // …but a real recorder alongside a skipped monitor still counts.
        assert!(is_recording([
            (Some("org.PulseAudio.pavucontrol"), true),
            (Some("org.mozilla.firefox"), true),
        ]));
    }

    #[test]
    fn cubic_mapping_round_trips_and_matches_pactl() {
        // pactl shows 40% for a linear 0.064 channel volume (−23.88 dB).
        assert!((pw_linear_to_volume(0.064) - 0.4).abs() < 1e-3);
        assert!((volume_to_pw_linear(0.4) - 0.064).abs() < 1e-3);
        for v in [0.0, 0.2, 0.5, 0.8, 1.0] {
            let round = pw_linear_to_volume(volume_to_pw_linear(v));
            assert!((round - v).abs() < 1e-9, "round-trip failed for {v}");
        }
    }
}
