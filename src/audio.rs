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

/// A snapshot of microphone (input) activity, feeding both the panel privacy indicator
/// (gnome-shell's `InputIndicator`) and the quick-settings **microphone slider**
/// (`InputStreamSlider`, `js/ui/status/volume.js`). Fed by the PipeWire watcher; carries no
/// rendering or PipeWire dependency (stays [`Default`] — not recording — where the audio backend is
/// absent, e.g. headless). No `Eq` because it carries an f64 volume.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MicStatus {
    /// A non-skipped application is actively capturing (a running input stream).
    pub recording: bool,
    /// The default source is muted — a muted mic is no privacy concern, so the panel drops the
    /// tint. Defaults `false` (→ tinted) when the mute state is unknown (no source/metadata): an
    /// active capture whose mute we can't read is still a privacy event, so understating it white
    /// would be wrong. This diverges from gnome-shell, which shows un-tinted when there's no
    /// stream.
    pub muted: bool,
    /// The default source's perceptual (cubic) volume, `0.0..=MAX_VOLUME`, for the mic slider
    /// fill. `0.0` when unknown (no bound source).
    pub volume: f64,
    /// Whether a default source is actually bound (a stream to control). gnome-shell's mic slider
    /// visibility is `stream != null && recording` (`volume.js:429`), so the slider shows only
    /// when both hold — a recording with no controllable source would give a dead slider.
    pub source_present: bool,
}

/// The symbolic icon for the current microphone level, gnome-shell's `InputStreamSlider` level
/// icons (`microphone-sensitivity-{muted,low,medium,high}-symbolic`, `volume.js:384-388`). Same
/// bucketing shape as [`volume_icon`]: muted (or ≤0) → muted glyph; else low/medium/high at the ⅓/⅔
/// marks.
pub fn mic_volume_icon(status: &MicStatus) -> &'static str {
    const ICONS: [&str; 4] = [
        "microphone-sensitivity-muted-symbolic",
        "microphone-sensitivity-low-symbolic",
        "microphone-sensitivity-medium-symbolic",
        "microphone-sensitivity-high-symbolic",
    ];
    if status.muted || status.volume <= 0.0 {
        return ICONS[0];
    }
    let n = (3.0 * status.volume).ceil() as i64;
    ICONS[n.clamp(1, 3) as usize]
}

/// Apps the mic privacy indicator ignores — they open capture only to display input levels, so
/// they aren't a real recording. Matches gnome-shell's `_maybeShowInput` skip list
/// (`js/ui/status/volume.js`), compared against `application.id` only (never `application.name`).
pub const MIC_SKIP_APP_IDS: &[&str] = &["org.gnome.VolumeControl", "org.PulseAudio.pavucontrol"];

/// One audio output sink, for the quick-settings output-device picker (gnome-shell's
/// `OutputStreamSlider` device rows). `name` is the PipeWire `node.name` — the stable key we match
/// the default against and write back to set the default; `description` is the human label
/// (`node.description`, e.g. "Built-in Audio Analog Stereo"). Diverges from gnome-shell, whose list
/// is port-level (gvc UIDevices, e.g. "Speakers"/"Headphones" on one card) rather than sink-level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkInfo {
    pub name: String,
    pub description: String,
}

/// A sink's place in the card/route model: the `Audio/Device` global it belongs to (`device.id`)
/// and the SPA device index inside that card (`card.profile.device`), which is what a route's
/// `device` field matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SinkCard {
    pub card_id: u32,
    /// `None` when the node does not report `card.profile.device` — a card with a single active
    /// route per direction still resolves, since there is nothing to disambiguate.
    pub device: Option<u32>,
}

/// What the **bound default sink** carries beyond its name — the inputs to GNOME's headphone
/// detection (`js/ui/status/volume.js:332-345`).
///
/// This hangs off the bound sink rather than off every [`SinkInfo`] for a concrete reason: a
/// PipeWire *registry global* carries only a subset of a node's props. `device.form_factor` and
/// `card.profile.device` are **not** in it — they arrive only in the `info` event of a bound proxy.
/// We bind exactly one sink (the default), so it is the only one that can honestly report these,
/// and it is also the only one GNOME asks about.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BoundSinkInfo {
    /// Which card the sink comes out of, for [`AudioCards::active_route`]. `None` for a virtual
    /// sink with no card behind it (`auto_null`).
    pub card: Option<SinkCard>,
    /// `device.form_factor` ("headset" / "headphone" / …) — the first branch of `_findHeadphones`.
    /// `None` on most cards; bluetooth headsets are where it actually shows up.
    pub form_factor: Option<String>,
}

/// The set of output sinks + which is the current default, fed by the PipeWire watcher for the
/// output-device picker. Sorted by PipeWire global id (stable across republishes). `default_name`
/// is the default sink's `node.name`, so the picker can mark the selected row. [`Default`] (empty,
/// no default) where the audio backend is absent (headless).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SinkList {
    pub sinks: Vec<SinkInfo>,
    pub default_name: Option<String>,
    /// The bound default sink's card membership and form factor — see [`BoundSinkInfo`] for why
    /// these live here and not on every [`SinkInfo`]. `None` until a sink is bound and its `info`
    /// event has arrived.
    pub bound: Option<BoundSinkInfo>,
}

/// One audio input source, for the quick-settings input-device picker (gnome-shell's
/// `InputStreamSlider` device rows). The input mirror of [`SinkInfo`]: `name` is the `node.name`
/// key, `description` the human label (`node.description`). gnome-shell's list is port-level; we
/// diverge to source-level, same as the output picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceInfo {
    pub name: String,
    pub description: String,
}

/// The set of input sources + the current default, fed by the PipeWire watcher for the input-device
/// picker. The input mirror of [`SinkList`]; `default_name` is the default source's `node.name`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceList {
    pub sources: Vec<SourceInfo>,
    pub default_name: Option<String>,
}

/// Which way a port carries audio. PipeWire's `SPA_PARAM_ROUTE_direction` is a `spa_direction`
/// (`SPA_DIRECTION_INPUT` = 0, `SPA_DIRECTION_OUTPUT` = 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortDirection {
    Input,
    Output,
}

/// Whether a port is plugged in / usable, PipeWire's `spa_param_availability`. gvc's device list is
/// keyed off this: `create_ui_device_from_port` treats `available != PA_PORT_AVAILABLE_NO` as
/// offerable (`gvc-mixer-control.c:1973,1995`), so **`Unknown` counts as available** — a card that
/// cannot detect jack presence reports `Unknown` for everything and must not vanish from the
/// picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PortAvailability {
    #[default]
    Unknown,
    No,
    Yes,
}

impl PortAvailability {
    /// gvc's test. Deliberately *not* `== Yes`: this machine's only output route reports `Unknown`.
    pub fn is_offerable(self) -> bool {
        self != PortAvailability::No
    }
}

/// One route on an audio card, parsed from a `SPA_TYPE_OBJECT_ParamRoute` pod. Both `EnumRoute`
/// (every route the card has) and `Route` (the active one per direction) use this object type; the
/// active form additionally carries [`device`](Self::device), the SPA device index it applies to.
///
/// This is gvc's card *port* — the thing GNOME builds one `GvcMixerUIDevice` per.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RouteInfo {
    pub index: u32,
    pub direction: Option<PortDirection>,
    /// Machine name, e.g. `analog-output` / `analog-output-headphones`. This is what GNOME's
    /// `_findHeadphones` substring-matches (`js/ui/status/volume.js:341-342`).
    pub name: String,
    /// Human label, e.g. "Analog Output" — the picker row's text (gvc's `port->human_port`).
    pub description: String,
    pub priority: u32,
    pub available: PortAvailability,
    /// SPA device indices this route applies to (`EnumRoute` form).
    pub devices: Vec<u32>,
    /// The SPA device index this route is currently applied to (`Route` form only). Joins to a
    /// node's `card.profile.device`.
    pub device: Option<u32>,
    /// Card profile indices this route is available under (`EnumRoute` form).
    pub profiles: Vec<u32>,
}

/// One audio card: PipeWire's `Audio/Device`, gvc's `GvcMixerCard`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioCard {
    /// PipeWire global id, the stable key.
    pub id: u32,
    /// `device.description`, e.g. "Built-in Audio" — gvc's card name, which the picker shows as
    /// the row's *origin* suffix (`volume.js:130-133`).
    pub description: String,
    /// `device.icon-name`, e.g. `audio-card-analog`; the fallback for a device with no icon of its
    /// own (`gvc-mixer-ui-device.c:632-643`).
    pub icon_name: Option<String>,
    /// Every route the card offers, from `EnumRoute`, in PipeWire's order.
    pub ports: Vec<RouteInfo>,
    /// The active route per direction, from `Route` — each carries its `device`.
    pub active: Vec<RouteInfo>,
}

/// Every audio card the watcher knows about, sorted by PipeWire global id (registry-appearance
/// order) so republishes are stable — the same rule [`SinkList`] follows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioCards {
    pub cards: Vec<AudioCard>,
}

impl AudioCards {
    /// The active route on `card_id`'s SPA device `device` in `direction`, if any.
    ///
    /// `device` is a node's `card.profile.device` and `card_id` its `device.id` — the join
    /// confirmed on this machine's card, where the sink node reports `device.id=42` +
    /// `card.profile.device=1` and the active route reports `device=1`. A node that carries no
    /// `card.profile.device` passes `None`, which falls back to the card's single active route in
    /// that direction (there is nothing to disambiguate against).
    pub fn active_route(
        &self,
        card_id: u32,
        device: Option<u32>,
        direction: PortDirection,
    ) -> Option<&RouteInfo> {
        let card = self.cards.iter().find(|c| c.id == card_id)?;
        let mut matching = card
            .active
            .iter()
            .filter(|r| r.direction == Some(direction));
        match device {
            Some(device) => matching.find(|r| r.device == Some(device)),
            None => {
                let first = matching.next()?;
                // Ambiguous: more than one device active in this direction and no key to pick with.
                matching.next().is_none().then_some(first)
            }
        }
    }
}

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

/// The compositor's view of an audio backend: everything the input/UI paths ask audio to *do*.
///
/// The live implementation is [`crate::pipewire_audio::PwAudio`] (feature `pipewire`), but this
/// trait mentions no PipeWire type, so it compiles unconditionally and a test can plug in
/// [`StubAudio`] instead. That is the point: with a concrete `Option<PwAudio>` on `Niri`, a
/// headless fixture had no audio at all and *nothing* about the wiring was testable — deleting the
/// OSD call out of the panel-scroll path left the whole suite green.
///
/// All methods take `&self`: the live backend drives its PipeWire loop on the compositor's calloop
/// and mutates through interior mutability, and callers hold `&self.niri.audio_backend` while
/// needing `&mut self` for the redraw that follows.
///
/// The control methods return the **optimistically-updated** status for immediate UI feedback,
/// or `None` when there is nothing bound to control; the backend's echo confirms it a moment later.
/// The two `set_default_*` are fire-and-forget: a rejected write has no corrective echo, so the
/// caller must not move the picker's check on its own.
pub trait AudioBackend {
    /// The default sink's last-known state, or `None` if no sink is bound.
    fn status(&self) -> Option<AudioStatus>;
    /// The last-known microphone activity, or `None` before the first capture stream is seen.
    fn mic_status(&self) -> Option<MicStatus>;

    fn set_volume(&self, volume: f64) -> Option<AudioStatus>;
    fn set_muted(&self, muted: bool) -> Option<AudioStatus>;
    fn toggle_muted(&self) -> Option<AudioStatus>;

    fn set_input_volume(&self, volume: f64) -> Option<MicStatus>;
    fn set_input_muted(&self, muted: bool) -> Option<MicStatus>;
    fn toggle_input_muted(&self) -> Option<MicStatus>;

    fn set_default_sink(&self, node_name: &str);
    fn set_default_source(&self, node_name: &str);

    /// Nudge the volume by `delta` (e.g. ±[`SCROLL_STEP`]). Clamping is [`set_volume`]'s job.
    fn adjust_volume(&self, delta: f64) -> Option<AudioStatus> {
        let current = self.status()?.volume;
        self.set_volume(current + delta)
    }
}

/// A test double for [`AudioBackend`]: holds a status, records every write, and controls whether
/// anything is "bound" at all (so a test can exercise the no-sink path the live backend takes when
/// PipeWire has nothing to offer).
///
/// Writes are recorded rather than applied blindly — `set_volume` on a stub that is bound updates
/// the status the way the live node echo would, so a test asserting on the *observable* state
/// (panel icon, OSD level) exercises the same code the compositor runs.
///
/// Cloning shares one state, so a test can hand a clone to the compositor
/// (`niri.audio_backend = Some(Box::new(stub.clone()))`) and keep its own handle to assert on.
#[cfg(test)]
#[derive(Debug, Default, Clone)]
pub struct StubAudio {
    inner: std::rc::Rc<std::cell::RefCell<StubAudioInner>>,
}

#[cfg(test)]
#[derive(Debug, Default)]
struct StubAudioInner {
    /// `None` = no sink bound, so every output control returns `None`.
    status: Option<AudioStatus>,
    /// `None` = no source bound.
    mic: Option<MicStatus>,
    /// Every call that reached the backend, in order, for assertions.
    writes: Vec<AudioWrite>,
}

/// One recorded control call on [`StubAudio`].
#[cfg(test)]
#[derive(Debug, Clone, PartialEq)]
pub enum AudioWrite {
    Volume(f64),
    Muted(bool),
    InputVolume(f64),
    InputMuted(bool),
    DefaultSink(String),
    DefaultSource(String),
}

#[cfg(test)]
impl StubAudio {
    /// A stub with a bound sink at this volume/mute, and no input source.
    pub fn with_status(status: AudioStatus) -> Self {
        let stub = Self::default();
        stub.inner.borrow_mut().status = Some(status);
        stub
    }

    /// Bind an input source with this status (the mic slider needs one to be controllable).
    pub fn with_mic(self, mic: MicStatus) -> Self {
        self.inner.borrow_mut().mic = Some(mic);
        self
    }

    /// Everything written to the backend so far, in call order.
    pub fn writes(&self) -> Vec<AudioWrite> {
        self.inner.borrow().writes.clone()
    }

    /// Drop the recorded writes, so a test can assert on one interaction at a time.
    pub fn clear_writes(&self) {
        self.inner.borrow_mut().writes.clear();
    }
}

#[cfg(test)]
impl AudioBackend for StubAudio {
    fn status(&self) -> Option<AudioStatus> {
        self.inner.borrow().status
    }

    fn mic_status(&self) -> Option<MicStatus> {
        self.inner.borrow().mic
    }

    fn set_volume(&self, volume: f64) -> Option<AudioStatus> {
        let mut inner = self.inner.borrow_mut();
        // Clamp like the live backend, so a test scrolling past the top sees the same value.
        let volume = volume.clamp(0.0, MAX_VOLUME);
        let status = inner.status.as_mut()?;
        status.volume = volume;
        let status = *status;
        inner.writes.push(AudioWrite::Volume(volume));
        Some(status)
    }

    fn set_muted(&self, muted: bool) -> Option<AudioStatus> {
        let mut inner = self.inner.borrow_mut();
        let status = inner.status.as_mut()?;
        status.muted = muted;
        let status = *status;
        inner.writes.push(AudioWrite::Muted(muted));
        Some(status)
    }

    fn toggle_muted(&self) -> Option<AudioStatus> {
        let muted = self.status()?.muted;
        self.set_muted(!muted)
    }

    fn set_input_volume(&self, volume: f64) -> Option<MicStatus> {
        let mut inner = self.inner.borrow_mut();
        let volume = volume.clamp(0.0, MAX_VOLUME);
        let mic = inner.mic.as_mut()?;
        mic.volume = volume;
        let mic = *mic;
        inner.writes.push(AudioWrite::InputVolume(volume));
        Some(mic)
    }

    fn set_input_muted(&self, muted: bool) -> Option<MicStatus> {
        let mut inner = self.inner.borrow_mut();
        let mic = inner.mic.as_mut()?;
        mic.muted = muted;
        let mic = *mic;
        inner.writes.push(AudioWrite::InputMuted(muted));
        Some(mic)
    }

    fn toggle_input_muted(&self) -> Option<MicStatus> {
        let muted = self.mic_status()?.muted;
        self.set_input_muted(!muted)
    }

    fn set_default_sink(&self, node_name: &str) {
        self.inner
            .borrow_mut()
            .writes
            .push(AudioWrite::DefaultSink(node_name.to_owned()));
    }

    fn set_default_source(&self, node_name: &str) {
        self.inner
            .borrow_mut()
            .writes
            .push(AudioWrite::DefaultSource(node_name.to_owned()));
    }
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

/// The JSON value written to the `default.configured.audio.sink` metadata key to set the default
/// output — `{"name":"<node.name>"}`. A `node.name` can in principle carry a `"` or `\` (it's a
/// free-form PipeWire string), so both are escaped; this is the write-side inverse of the read-side
/// `parse_metadata_name` in `pipewire_audio` (kept honest by a round-trip test). Built by hand
/// rather than pulling in a JSON crate for this one one-field object.
pub fn sink_default_json(name: &str) -> String {
    let mut escaped = String::with_capacity(name.len());
    for c in name.chars() {
        if c == '\\' || c == '"' {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    format!("{{\"name\":\"{escaped}\"}}")
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
    fn mic_icon_buckets_match_gnome_thresholds() {
        let mic = |volume: f64, muted: bool| MicStatus {
            recording: true,
            muted,
            volume,
            source_present: true,
        };
        assert_eq!(
            mic_volume_icon(&mic(0.5, true)),
            "microphone-sensitivity-muted-symbolic"
        );
        assert_eq!(
            mic_volume_icon(&mic(0.0, false)),
            "microphone-sensitivity-muted-symbolic"
        );
        assert_eq!(
            mic_volume_icon(&mic(0.2, false)),
            "microphone-sensitivity-low-symbolic"
        );
        assert_eq!(
            mic_volume_icon(&mic(0.5, false)),
            "microphone-sensitivity-medium-symbolic"
        );
        assert_eq!(
            mic_volume_icon(&mic(1.0, false)),
            "microphone-sensitivity-high-symbolic"
        );
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

    /// The node→card→route join, on the numbers this machine's card actually reports: the sink node
    /// carries `device.id=42` and `card.profile.device=1`, and the card's active output route
    /// carries `device=1`.
    #[test]
    fn a_sink_resolves_to_its_cards_active_route() {
        let route = |index, direction, name: &str, device| RouteInfo {
            index,
            direction: Some(direction),
            name: name.to_owned(),
            device,
            ..RouteInfo::default()
        };
        let cards = AudioCards {
            cards: vec![AudioCard {
                id: 42,
                description: "Built-in Audio".to_owned(),
                icon_name: Some("audio-card-analog".to_owned()),
                ports: vec![],
                active: vec![
                    route(0, PortDirection::Output, "analog-output", Some(1)),
                    route(2, PortDirection::Input, "analog-input-mic", Some(0)),
                ],
            }],
        };

        let sink = SinkCard {
            card_id: 42,
            device: Some(1),
        };
        assert_eq!(
            cards
                .active_route(sink.card_id, sink.device, PortDirection::Output)
                .map(|r| r.name.as_str()),
            Some("analog-output")
        );
        // Direction picks the input route even though it is on a different SPA device.
        assert_eq!(
            cards
                .active_route(42, Some(0), PortDirection::Input)
                .map(|r| r.name.as_str()),
            Some("analog-input-mic")
        );
        // A device index nothing is active on resolves to nothing, rather than to the wrong route.
        assert_eq!(cards.active_route(42, Some(9), PortDirection::Output), None);
        // Unknown card.
        assert_eq!(cards.active_route(7, Some(1), PortDirection::Output), None);

        // No `card.profile.device` on the node: fall back to the card's single active route in that
        // direction, since there is nothing to disambiguate against.
        assert_eq!(
            cards
                .active_route(42, None, PortDirection::Output)
                .map(|r| r.name.as_str()),
            Some("analog-output")
        );

        // ...but with TWO active output routes and no key, guessing would be wrong — a card with
        // separate speaker and HDMI devices would silently pick whichever came first.
        let mut ambiguous = cards.clone();
        ambiguous.cards[0]
            .active
            .push(route(5, PortDirection::Output, "hdmi-output", Some(3)));
        assert_eq!(
            ambiguous.active_route(42, None, PortDirection::Output),
            None,
            "two active output routes and no device key must resolve to nothing, not to a guess"
        );
    }

    /// The shape a live run produced, kept as a regression: the bound sink's card join and the
    /// card's icon are populated, and they resolve to the card's active output route.
    ///
    /// Both of those came back `None` on the first live run, because they were being read from the
    /// PipeWire **registry global**, whose props are only a subset — `card.profile.device`,
    /// `device.form_factor` and `device.icon-name` exist solely in a bound proxy's `info` event.
    /// Nothing headless could have caught that: the model was well-formed, just permanently empty.
    #[test]
    fn the_live_cards_shape_resolves_end_to_end() {
        let cards = AudioCards {
            cards: vec![AudioCard {
                id: 42,
                description: "Built-in Audio".to_owned(),
                icon_name: Some("audio-card-analog".to_owned()),
                ports: vec![RouteInfo {
                    index: 0,
                    direction: Some(PortDirection::Output),
                    name: "analog-output".to_owned(),
                    description: "Analog Output".to_owned(),
                    priority: 9900,
                    available: PortAvailability::Unknown,
                    devices: vec![1],
                    device: None,
                    profiles: vec![1],
                }],
                active: vec![RouteInfo {
                    device: Some(1),
                    ..RouteInfo {
                        index: 0,
                        direction: Some(PortDirection::Output),
                        name: "analog-output".to_owned(),
                        description: "Analog Output".to_owned(),
                        priority: 9900,
                        available: PortAvailability::Unknown,
                        devices: vec![1],
                        device: None,
                        profiles: vec![1],
                    }
                }],
            }],
        };
        let sinks = SinkList {
            sinks: vec![SinkInfo {
                name: "alsa_output.platform-a016000.virtio_mmio.stereo-fallback".to_owned(),
                description: "Built-in Audio Stereo".to_owned(),
            }],
            default_name: Some(
                "alsa_output.platform-a016000.virtio_mmio.stereo-fallback".to_owned(),
            ),
            bound: Some(BoundSinkInfo {
                card: Some(SinkCard {
                    card_id: 42,
                    device: Some(1),
                }),
                form_factor: None,
            }),
        };

        let bound = sinks.bound.as_ref().unwrap();
        let card = bound.card.expect("the bound sink knows its card");
        let route = cards
            .active_route(card.card_id, card.device, PortDirection::Output)
            .expect("...and that card has an active output route");
        assert_eq!(route.name, "analog-output");
        assert!(route.available.is_offerable());
        // What slice 2 will read: no form factor and a port name with no "headphone" in it, so this
        // card is speakers by both of `_findHeadphones`' branches.
        assert_eq!(bound.form_factor, None);
        assert!(!route.name.to_lowercase().contains("headphone"));
    }

    #[test]
    fn sink_default_json_escapes_and_wraps() {
        assert_eq!(
            sink_default_json("alsa_output.pci-0000_00_1f.3.analog-stereo"),
            r#"{"name":"alsa_output.pci-0000_00_1f.3.analog-stereo"}"#
        );
        // A pathological name with a quote and a backslash stays valid JSON.
        assert_eq!(sink_default_json(r#"we"ir\d"#), r#"{"name":"we\"ir\\d"}"#);
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
