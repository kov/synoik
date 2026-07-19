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
