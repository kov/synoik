//! PipeWire watcher + controller for the default audio sink (feature `audio`).
//!
//! GNOME reads the default sink through `gvc` (libgvc → PulseAudio). We talk to
//! PipeWire directly: one persistent connection whose main loop is driven on the
//! compositor's calloop (the same fd-integration the screencast code uses,
//! `src/screencasting/pw_utils.rs`), so every callback runs on the main thread and
//! can mutate state / set params without a cross-thread hop.
//!
//! Flow: the registry surfaces `Audio/Sink` nodes and the `default` metadata; the
//! metadata's `default.audio.sink` key names the current default sink; we bind that
//! node, subscribe to its `Props` param, and translate `channelVolumes` (linear) +
//! `mute` into an [`AudioStatus`] (perceptual, via [`crate::audio`]). Control
//! (`set_volume`/`set_mute`) writes a `Props` pod back to the node.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Cursor;
use std::os::fd::{AsFd, BorrowedFd};
use std::rc::{Rc, Weak};
use std::time::Duration;

use anyhow::Context as _;
use calloop::generic::Generic;
use calloop::{Interest, LoopHandle, Mode, PostAction, RegistrationToken};
use pipewire::context::ContextRc;
use pipewire::core::CoreRc;
use pipewire::main_loop::MainLoopRc;
use pipewire::metadata::{Metadata, MetadataListener};
use pipewire::node::{Node, NodeInfoRef, NodeListener};
use pipewire::properties::PropertiesBox;
use pipewire::registry::{GlobalObject, Listener as RegistryListener, RegistryRc};
use pipewire::spa::param::ParamType;
use pipewire::spa::pod::deserialize::PodDeserializer;
use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::spa::pod::{Object, Pod, Property, PropertyFlags, Value, ValueArray};
use pipewire::spa::sys::{
    SPA_PARAM_Props, SPA_PROP_channelVolumes, SPA_PROP_mute, SPA_PROP_volume, SPA_TYPE_OBJECT_Props,
};
use pipewire::types::ObjectType;

use crate::audio::{
    pw_linear_to_volume, sink_default_json, volume_to_pw_linear, AudioStatus, MicStatus, SinkInfo,
    SinkList, MAX_VOLUME,
};
use crate::niri::State;

/// The live audio connection, owned by the compositor for the whole session.
pub struct PwAudio {
    // Kept alive; the loop is driven via the calloop source below.
    main_loop: MainLoopRc,
    _context: ContextRc,
    _core: CoreRc,
    _registry: RegistryRc,
    _registry_listener: RegistryListener,
    _loop_token: RegistrationToken,
    inner: Rc<RefCell<Inner>>,
}

/// A bound default-sink node and the listener keeping its param events flowing.
struct BoundSink {
    id: u32,
    node: Node,
    _listener: NodeListener,
    /// Channel count from the last `channelVolumes` read, so writes match.
    channels: usize,
}

/// A bound default-source node, kept alive so its `Props` (mute) events flow.
struct BoundSource {
    id: u32,
    _node: Node,
    _listener: NodeListener,
}

/// A tracked input-capture stream (`Stream/Input/Audio`): an application recording from a mic. We
/// bind the node only to watch its run-state via `.info()` — a `Running` non-skipped stream is what
/// makes the privacy indicator light up. The node + listener are kept alive here; dropping this
/// entry (only ever from `on_global_remove`) unbinds it.
struct Capture {
    /// `application.id`, for the skip list (matched against `id` only, per gnome-shell).
    app_id: Option<String>,
    /// Whether the node is currently in the `Running` state (actively capturing).
    running: bool,
    _node: Node,
    _listener: NodeListener,
}

#[derive(Default)]
struct Inner {
    /// Registry, so the metadata callback can bind a node when the default changes.
    registry: Option<RegistryRc>,
    /// `default` metadata proxy + listener (bound once).
    metadata: Option<(Metadata, MetadataListener)>,
    /// Known `Audio/Sink` nodes: id → (node.name, node.description, owned global for late
    /// binding). The description is captured once when the global appears (props are only live
    /// in that callback) and never updated — sink descriptions are effectively static.
    sinks: HashMap<u32, (String, String, GlobalObject<PropertiesBox>)>,
    /// The default sink's `node.name`, from `default.audio.sink`.
    default_name: Option<String>,
    /// The currently-bound default sink.
    bound: Option<BoundSink>,
    /// Latest status, and whether it changed since the compositor last drained it.
    status: AudioStatus,
    present: bool,
    dirty: Option<Option<AudioStatus>>,
    /// The last sink list handed to the compositor (for the output-device picker), and one it
    /// hasn't drained yet — published only on an actual change, mirroring the mic path.
    sink_list_last: Option<SinkList>,
    sink_list_dirty: Option<SinkList>,

    // --- Microphone privacy indicator (input side) ---
    /// Known `Audio/Source` nodes: id → (node.name, owned global for late binding).
    sources: HashMap<u32, (String, GlobalObject<PropertiesBox>)>,
    /// The default source's `node.name`, from `default.audio.source`.
    default_source_name: Option<String>,
    /// The currently-bound default source (for its mute).
    bound_source: Option<BoundSource>,
    /// The default source's mute, `false` when unknown (no source/metadata) — see [`MicStatus`].
    mic_muted: bool,
    /// Active input-capture streams: node id → [`Capture`]. Mutated ONLY from registry callbacks
    /// (`on_global`/`on_global_remove`); the per-node `.info()` callback only flips `running`.
    captures: HashMap<u32, Capture>,
    /// The last mic status handed to the compositor, so we publish only on an actual change
    /// (`.info()` fires for non-state changes too).
    mic_last: Option<MicStatus>,
    /// A mic status the compositor hasn't drained yet.
    mic_dirty: Option<MicStatus>,
}

impl Inner {
    /// Record a new status and flag it for the compositor to pick up.
    fn publish(&mut self, status: Option<AudioStatus>) {
        self.present = status.is_some();
        if let Some(s) = status {
            self.status = s;
        }
        self.dirty = Some(status);
    }

    /// Recompute the mic status from the current captures + source mute, and flag it for the
    /// compositor only if it actually changed.
    fn publish_mic(&mut self) {
        let recording = crate::audio::is_recording(
            self.captures
                .values()
                .map(|c| (c.app_id.as_deref(), c.running)),
        );
        let status = MicStatus {
            recording,
            muted: self.mic_muted,
        };
        if self.mic_last != Some(status) {
            self.mic_last = Some(status);
            self.mic_dirty = Some(status);
        }
    }

    /// Rebuild the sink list from the tracked sinks + current default, and flag it for the
    /// compositor only if it actually changed. Sorted by PipeWire global id (registry-appearance
    /// order) so the rows are stable across republishes — a `HashMap` iteration order would defeat
    /// the change-dedup and shuffle the picker rows under the pointer.
    fn publish_sinks(&mut self) {
        let mut ids: Vec<u32> = self.sinks.keys().copied().collect();
        ids.sort_unstable();
        let list = SinkList {
            sinks: ids
                .iter()
                .map(|id| {
                    let (name, description, _) = &self.sinks[id];
                    SinkInfo {
                        name: name.clone(),
                        description: description.clone(),
                    }
                })
                .collect(),
            default_name: self.default_name.clone(),
        };
        if self.sink_list_last.as_ref() != Some(&list) {
            self.sink_list_last = Some(list.clone());
            self.sink_list_dirty = Some(list);
        }
    }
}

/// Connect to PipeWire and start tracking the default sink. Returns `None`-worthy
/// errors as `Err` so the caller can log and carry on without audio.
pub fn start(loop_handle: &LoopHandle<'static, State>) -> anyhow::Result<PwAudio> {
    let main_loop = MainLoopRc::new(None).context("creating pipewire MainLoop")?;
    let context = ContextRc::new(&main_loop, None).context("creating pipewire Context")?;
    let core = context.connect_rc(None).context("connecting to pipewire")?;
    let registry = core
        .get_registry_rc()
        .context("getting pipewire registry")?;

    let inner = Rc::new(RefCell::new(Inner::default()));
    inner.borrow_mut().registry = Some(registry.clone());

    let registry_listener = {
        let weak = Rc::downgrade(&inner);
        let weak_rm = weak.clone();
        registry
            .add_listener_local()
            .global(move |obj| on_global(&weak, obj))
            .global_remove(move |id| on_global_remove(&weak_rm, id))
            .register()
    };

    // Drive the pipewire loop on our calloop, and drain any status change afterwards.
    let source = Generic::new(AsFdWrapper(main_loop.clone()), Interest::READ, Mode::Level);
    let loop_token = loop_handle
        .insert_source(source, {
            let inner = inner.clone();
            move |_, wrapper, state: &mut State| {
                wrapper.0.loop_().iterate(Duration::ZERO);
                // Drain all signals under one borrow, then release it before calling into State
                // (which redraws) so nothing can re-enter a borrowed Inner.
                let (dirty, mic_dirty, sink_list_dirty) = {
                    let mut inner = inner.borrow_mut();
                    (
                        inner.dirty.take(),
                        inner.mic_dirty.take(),
                        inner.sink_list_dirty.take(),
                    )
                };
                if let Some(status) = dirty {
                    state.on_audio_status(status);
                }
                if let Some(mic) = mic_dirty {
                    state.on_mic_status(mic);
                }
                if let Some(list) = sink_list_dirty {
                    state.on_sink_list(list);
                }
                Ok(PostAction::Continue)
            }
        })
        .map_err(|err| anyhow::anyhow!("inserting pipewire loop source: {err}"))?;

    Ok(PwAudio {
        main_loop,
        _context: context,
        _core: core,
        _registry: registry,
        _registry_listener: registry_listener,
        _loop_token: loop_token,
        inner,
    })
}

impl PwAudio {
    /// The last-known default-sink state, or `None` if no sink is bound yet.
    pub fn status(&self) -> Option<AudioStatus> {
        let inner = self.inner.borrow();
        inner.present.then_some(inner.status)
    }

    /// The last-known microphone activity (recording + mute). `None` until the first capture stream
    /// is seen; the compositor treats `None` as "not recording".
    pub fn mic_status(&self) -> Option<MicStatus> {
        self.inner.borrow().mic_last
    }

    /// Set the perceptual volume (clamped to `[0, MAX_VOLUME]`) on the default sink.
    /// Returns the optimistically-updated status for immediate UI feedback (the
    /// node's echo confirms it a moment later).
    pub fn set_volume(&self, volume: f64) -> Option<AudioStatus> {
        let volume = volume.clamp(0.0, MAX_VOLUME);
        let mut inner = self.inner.borrow_mut();
        let bound = inner.bound.as_ref()?;
        let linear = volume_to_pw_linear(volume) as f32;
        let vols = vec![linear; bound.channels.max(1)];
        set_props(&bound.node, Some(vols), None);
        let status = AudioStatus {
            volume,
            muted: inner.status.muted,
        };
        inner.publish(Some(status));
        Some(status)
    }

    /// Nudge the volume by `delta` (e.g. ±[`crate::audio::SCROLL_STEP`]).
    pub fn adjust_volume(&self, delta: f64) -> Option<AudioStatus> {
        let current = self.status()?.volume;
        self.set_volume(current + delta)
    }

    /// Set the mute flag on the default sink.
    pub fn set_muted(&self, muted: bool) -> Option<AudioStatus> {
        let mut inner = self.inner.borrow_mut();
        let bound = inner.bound.as_ref()?;
        set_props(&bound.node, None, Some(muted));
        let status = AudioStatus {
            volume: inner.status.volume,
            muted,
        };
        inner.publish(Some(status));
        Some(status)
    }

    /// Flip the mute flag.
    pub fn toggle_muted(&self) -> Option<AudioStatus> {
        let muted = self.status()?.muted;
        self.set_muted(!muted)
    }

    /// The last-known sink list (for the output-device picker), empty until the first sink is seen.
    pub fn sink_list(&self) -> SinkList {
        self.inner
            .borrow()
            .sink_list_last
            .clone()
            .unwrap_or_default()
    }

    /// Set the system default output to the sink with this `node.name`, by writing the persistent
    /// `default.configured.audio.sink` metadata key (what `wpctl set-default` / gvc's
    /// `change_output` write). The session manager (WirePlumber) echoes it back as
    /// `default.audio.sink`, which flows through [`on_metadata_property`] → [`bind_default`] → the
    /// volume path and the picker's selected marker — so, unlike volume, we do **not** flip the
    /// marker optimistically here (a rejected write has no corrective echo). No-op if no `default`
    /// metadata is bound. Safe from this thread: the pipewire loop runs on the compositor's
    /// calloop, and `set_property` is a pure marshal (no synchronous callback, no `Inner`
    /// re-entrancy).
    pub fn set_default_sink(&self, node_name: &str) {
        let inner = self.inner.borrow();
        let Some((metadata, _)) = inner.metadata.as_ref() else {
            return;
        };
        metadata.set_property(
            0,
            "default.configured.audio.sink",
            Some("Spa:String:JSON"),
            // `node.name` comes from a PipeWire C string, so it can't contain an interior NUL —
            // `set_property`'s internal `CString::new().expect()` therefore can't panic here.
            Some(&sink_default_json(node_name)),
        );
    }

    /// Manually pump the loop once (used at startup so the initial state lands
    /// without waiting for the first fd wakeup).
    pub fn pump(&self) {
        self.main_loop.loop_().iterate(Duration::ZERO);
    }
}

struct AsFdWrapper(MainLoopRc);
impl AsFd for AsFdWrapper {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.loop_().fd()
    }
}

/// A new global appeared: track `Audio/Sink` nodes and bind the `default` metadata.
fn on_global(
    weak: &Weak<RefCell<Inner>>,
    obj: &GlobalObject<&pipewire::spa::utils::dict::DictRef>,
) {
    let Some(inner_rc) = weak.upgrade() else {
        return;
    };
    match obj.type_ {
        ObjectType::Node => {
            let Some(props) = obj.props else { return };
            let Some(class) = props.get("media.class") else {
                return;
            };
            if class == "Audio/Sink" {
                let Some(name) = props.get("node.name") else {
                    return;
                };
                let name = name.to_string();
                // Human label for the picker, in gvc's preference order; last-resort the machine
                // name. Captured now — props are only live inside this callback.
                let description = props
                    .get("node.description")
                    .or_else(|| props.get("node.nick"))
                    .or_else(|| props.get("device.description"))
                    .unwrap_or(name.as_str())
                    .to_string();
                let mut inner = inner_rc.borrow_mut();
                inner
                    .sinks
                    .insert(obj.id, (name.clone(), description, obj.to_owned()));
                // If this is the default sink and nothing is bound yet, bind it now.
                if inner.default_name.as_deref() == Some(name.as_str())
                    && inner.bound.as_ref().map(|b| b.id) != Some(obj.id)
                {
                    bind_default(&mut inner, weak);
                }
                inner.publish_sinks();
            } else if class.starts_with("Audio/Source") {
                // Prefix, not exact: virtual/processed mics (echo-cancel, the default source on
                // many laptops) are `Audio/Source/Virtual`.
                let Some(name) = props.get("node.name") else {
                    return;
                };
                let name = name.to_string();
                let mut inner = inner_rc.borrow_mut();
                inner.sources.insert(obj.id, (name.clone(), obj.to_owned()));
                if inner.default_source_name.as_deref() == Some(name.as_str())
                    && inner.bound_source.as_ref().map(|b| b.id) != Some(obj.id)
                {
                    bind_default_source(&mut inner, weak);
                }
            } else if class == "Stream/Input/Audio" {
                // An application recording from a mic. Bind it to watch its run-state.
                track_capture(&inner_rc, weak, obj, props);
            }
        }
        ObjectType::Metadata => {
            let is_default = obj.props.and_then(|p| p.get("metadata.name")) == Some("default");
            if !is_default {
                return;
            }
            let mut inner = inner_rc.borrow_mut();
            if inner.metadata.is_some() {
                return;
            }
            let Some(registry) = inner.registry.clone() else {
                return;
            };
            let Ok(metadata) = registry.bind::<Metadata, _>(obj) else {
                warn!("failed to bind default metadata");
                return;
            };
            let listener = {
                let weak = weak.clone();
                metadata
                    .add_listener_local()
                    .property(move |subject, key, _type, value| {
                        on_metadata_property(&weak, subject, key, value);
                        0
                    })
                    .register()
            };
            inner.metadata = Some((metadata, listener));
        }
        _ => {}
    }
}

/// A global went away: drop the sink and unbind if it was the default.
fn on_global_remove(weak: &Weak<RefCell<Inner>>, id: u32) {
    let Some(inner_rc) = weak.upgrade() else {
        return;
    };
    let mut inner = inner_rc.borrow_mut();
    if inner.sinks.remove(&id).is_some() {
        inner.publish_sinks();
    }
    inner.sources.remove(&id);
    if inner.bound.as_ref().map(|b| b.id) == Some(id) {
        inner.bound = None;
        inner.publish(None);
    }
    if inner.bound_source.as_ref().map(|b| b.id) == Some(id) {
        inner.bound_source = None;
        inner.mic_muted = false; // mute state is now unknown → treat as a privacy event
        inner.publish_mic();
    }
    // A recording stream going away recomputes the indicator.
    if inner.captures.remove(&id).is_some() {
        inner.publish_mic();
    }
}

/// The `default` metadata changed. When `default.audio.sink` moves, rebind.
fn on_metadata_property(
    weak: &Weak<RefCell<Inner>>,
    subject: u32,
    key: Option<&str>,
    value: Option<&str>,
) {
    if subject != 0 {
        return;
    }
    let is_source = match key {
        Some("default.audio.sink") => false,
        Some("default.audio.source") => true,
        _ => return,
    };
    let Some(name) = value.and_then(parse_metadata_name) else {
        return;
    };
    let Some(inner_rc) = weak.upgrade() else {
        return;
    };
    let mut inner = inner_rc.borrow_mut();
    if is_source {
        if inner.default_source_name.as_deref() == Some(name.as_str()) {
            return;
        }
        inner.default_source_name = Some(name);
        bind_default_source(&mut inner, weak);
    } else {
        if inner.default_name.as_deref() == Some(name.as_str()) {
            return;
        }
        inner.default_name = Some(name);
        bind_default(&mut inner, weak);
        // The picker marks the default row, so a default change must republish the list.
        inner.publish_sinks();
    }
}

/// Bind the node named by `default_name` (if it's a known sink) and subscribe to
/// its `Props`.
fn bind_default(inner: &mut Inner, weak: &Weak<RefCell<Inner>>) {
    let Some(name) = inner.default_name.clone() else {
        return;
    };
    let Some(registry) = inner.registry.clone() else {
        return;
    };
    let Some(id) = inner
        .sinks
        .iter()
        .find(|(_, (n, _, _))| *n == name)
        .map(|(id, _)| *id)
    else {
        return; // node not surfaced yet; on_global will bind it when it appears
    };
    if inner.bound.as_ref().map(|b| b.id) == Some(id) {
        return;
    }
    // Bind from a borrow of the stored global (GlobalObject isn't Clone); the borrow
    // ends before we write `inner.bound` below.
    let node = match registry.bind::<Node, _>(&inner.sinks[&id].2) {
        Ok(node) => node,
        Err(_) => {
            warn!("failed to bind default audio sink node {id}");
            return;
        }
    };
    let listener = {
        let weak = weak.clone();
        node.add_listener_local()
            .param(move |_seq, _id, _index, _next, pod| {
                if let Some(pod) = pod {
                    on_node_param(&weak, pod);
                }
            })
            .register()
    };
    node.subscribe_params(&[ParamType::Props]);
    inner.bound = Some(BoundSink {
        id,
        node,
        _listener: listener,
        channels: 2,
    });
}

/// Bind an input-capture stream and watch its run-state via `.info()`. A `Running` non-skipped
/// stream is what lights the mic privacy indicator. Called only from `on_global` (registry
/// callback), the sole mutator of `captures`.
fn track_capture(
    inner_rc: &Rc<RefCell<Inner>>,
    weak: &Weak<RefCell<Inner>>,
    obj: &GlobalObject<&pipewire::spa::utils::dict::DictRef>,
    props: &pipewire::spa::utils::dict::DictRef,
) {
    // Skip monitor captures — a stream recording a sink's monitor is capturing desktop audio, not
    // the microphone. gvc drops monitor sources entirely and gnome-shell's indicator never lights
    // for them, so tracking one here would be a false privacy alarm (e.g. a screen recorder
    // grabbing system sound). `PW_KEY_STREAM_MONITOR` marks these.
    if props.get("stream.monitor") == Some("true") {
        return;
    }
    let app_id = props.get("application.id").map(str::to_string);
    let id = obj.id;
    let mut inner = inner_rc.borrow_mut();
    let Some(registry) = inner.registry.clone() else {
        return;
    };
    let node = match registry.bind::<Node, _>(obj) {
        Ok(node) => node,
        Err(_) => {
            warn!("failed to bind input-capture node {id}");
            return;
        }
    };
    let listener = {
        let weak = weak.clone();
        node.add_listener_local()
            .info(move |info| on_capture_info(&weak, id, info))
            .register()
    };
    inner.captures.insert(
        id,
        Capture {
            app_id,
            running: false,
            _node: node,
            _listener: listener,
        },
    );
    inner.publish_mic();
}

/// A capture node's info arrived: flip its `running` flag on a `Running`↔other transition and
/// recompute. Only ever writes the `running` flag (never mutates the `captures` map), so it can't
/// drop the very entry it's called for.
fn on_capture_info(weak: &Weak<RefCell<Inner>>, id: u32, info: &NodeInfoRef) {
    // Read the raw state, NOT `NodeInfoRef::state()`: that accessor dereferences the node's error
    // string (UB on NULL) and `.unwrap()`s its UTF-8 for the `Error` variant, and panics on any
    // unknown state — a panic here aborts the compositor from a C callback. We only need "is it
    // Running", so compare the raw enum: any other state (incl. Error/unknown) is "not recording".
    let running = info.as_raw().state == pipewire::sys::pw_node_state_PW_NODE_STATE_RUNNING;
    let Some(inner_rc) = weak.upgrade() else {
        return;
    };
    let mut inner = inner_rc.borrow_mut();
    match inner.captures.get_mut(&id) {
        Some(cap) if cap.running != running => cap.running = running,
        _ => return, // unknown id, or no state change
    }
    inner.publish_mic();
}

/// Bind the node named by `default_source_name` (if it's a known source) and subscribe to its
/// `Props` for the mute flag. Mirrors [`bind_default`].
fn bind_default_source(inner: &mut Inner, weak: &Weak<RefCell<Inner>>) {
    let Some(name) = inner.default_source_name.clone() else {
        return;
    };
    let Some(registry) = inner.registry.clone() else {
        return;
    };
    let Some(id) = inner
        .sources
        .iter()
        .find(|(_, (n, _))| *n == name)
        .map(|(id, _)| *id)
    else {
        return; // node not surfaced yet; on_global will bind it when it appears
    };
    if inner.bound_source.as_ref().map(|b| b.id) == Some(id) {
        return;
    }
    let node = match registry.bind::<Node, _>(&inner.sources[&id].1) {
        Ok(node) => node,
        Err(_) => {
            warn!("failed to bind default audio source node {id}");
            return;
        }
    };
    let listener = {
        let weak = weak.clone();
        node.add_listener_local()
            .param(move |_seq, _id, _index, _next, pod| {
                if let Some(pod) = pod {
                    on_source_param(&weak, pod);
                }
            })
            .register()
    };
    node.subscribe_params(&[ParamType::Props]);
    inner.bound_source = Some(BoundSource {
        id,
        _node: node,
        _listener: listener,
    });
}

/// A default-source `Props` param arrived: pull `mute` and recompute the mic status.
// The SPA property constant keeps its C mixed-case name; matched as a constant.
#[allow(non_upper_case_globals)]
fn on_source_param(weak: &Weak<RefCell<Inner>>, pod: &Pod) {
    let Ok((_, Value::Object(obj))) = PodDeserializer::deserialize_from::<Value>(pod.as_bytes())
    else {
        return;
    };
    let mut muted: Option<bool> = None;
    for prop in &obj.properties {
        if let (SPA_PROP_mute, Value::Bool(b)) = (prop.key, &prop.value) {
            muted = Some(*b);
        }
    }
    let Some(muted) = muted else {
        return;
    };
    let Some(inner_rc) = weak.upgrade() else {
        return;
    };
    let mut inner = inner_rc.borrow_mut();
    if inner.mic_muted != muted {
        inner.mic_muted = muted;
        inner.publish_mic();
    }
}

/// A `Props` param arrived: pull `channelVolumes` + `mute` and publish.
// The SPA property constants keep their C mixed-case names; matched as constants.
#[allow(non_upper_case_globals)]
fn on_node_param(weak: &Weak<RefCell<Inner>>, pod: &Pod) {
    let Ok((_, Value::Object(obj))) = PodDeserializer::deserialize_from::<Value>(pod.as_bytes())
    else {
        return;
    };
    let mut linear: Option<f64> = None;
    let mut mono: Option<f64> = None;
    let mut muted: Option<bool> = None;
    let mut channels: Option<usize> = None;
    for prop in &obj.properties {
        match (prop.key, &prop.value) {
            (SPA_PROP_channelVolumes, Value::ValueArray(ValueArray::Float(vols))) => {
                channels = Some(vols.len());
                // gvc exposes a single volume: the loudest channel.
                linear = vols
                    .iter()
                    .map(|v| *v as f64)
                    .fold(None, |acc, v| Some(acc.map_or(v, |a: f64| a.max(v))));
            }
            (SPA_PROP_volume, Value::Float(v)) => mono = Some(*v as f64),
            (SPA_PROP_mute, Value::Bool(b)) => muted = Some(*b),
            _ => {}
        }
    }
    let Some(inner_rc) = weak.upgrade() else {
        return;
    };
    let mut inner = inner_rc.borrow_mut();
    if let Some(ch) = channels {
        if let Some(bound) = inner.bound.as_mut() {
            bound.channels = ch.max(1);
        }
    }
    // Props events can be partial; fall back to the last-known values.
    let linear = linear
        .or(mono)
        .unwrap_or_else(|| volume_to_pw_linear(inner.status.volume));
    let muted = muted.unwrap_or(inner.status.muted);
    inner.publish(Some(AudioStatus {
        volume: pw_linear_to_volume(linear),
        muted,
    }));
}

/// Write a `Props` pod to `node` setting `channelVolumes` and/or `mute`.
fn set_props(node: &Node, channel_volumes: Option<Vec<f32>>, mute: Option<bool>) {
    let mut properties = Vec::new();
    if let Some(vols) = channel_volumes {
        properties.push(Property {
            key: SPA_PROP_channelVolumes,
            flags: PropertyFlags::empty(),
            value: Value::ValueArray(ValueArray::Float(vols)),
        });
    }
    if let Some(mute) = mute {
        properties.push(Property {
            key: SPA_PROP_mute,
            flags: PropertyFlags::empty(),
            value: Value::Bool(mute),
        });
    }
    if properties.is_empty() {
        return;
    }
    let object = Value::Object(Object {
        type_: SPA_TYPE_OBJECT_Props,
        id: SPA_PARAM_Props,
        properties,
    });
    let mut bytes = Vec::new();
    if PodSerializer::serialize(Cursor::new(&mut bytes), &object).is_err() {
        warn!("failed to serialize audio Props pod");
        return;
    }
    if let Some(pod) = Pod::from_bytes(&bytes) {
        node.set_param(ParamType::Props, 0, pod);
    }
}

/// Extract the sink name from a `default.audio.sink` metadata value, JSON like
/// `{"name":"alsa_output..."}`. Minimal parse to avoid a JSON dependency.
fn parse_metadata_name(value: &str) -> Option<String> {
    let after_key = value.split("\"name\"").nth(1)?;
    let after_colon = after_key.split(':').nth(1)?;
    let start = after_colon.find('"')? + 1;
    let rest = &after_colon[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::parse_metadata_name;
    use crate::audio::sink_default_json;

    #[test]
    fn parses_default_sink_name() {
        assert_eq!(
            parse_metadata_name(r#"{"name":"alsa_output.pci-0000_00.analog-stereo"}"#).as_deref(),
            Some("alsa_output.pci-0000_00.analog-stereo")
        );
        assert_eq!(
            parse_metadata_name(r#"{ "name" : "foo" , "x": 1 }"#).as_deref(),
            Some("foo")
        );
        assert_eq!(parse_metadata_name("null"), None);
    }

    /// The write-side serializer and the read-side parser must agree for a real `node.name` (which
    /// never carries a quote/backslash) — so a default we set round-trips through the metadata
    /// echo.
    #[test]
    fn sink_default_json_round_trips_through_the_parser() {
        for name in [
            "alsa_output.pci-0000_00_1f.3.analog-stereo",
            "bluez_output.AA_BB_CC_DD_EE_FF.1",
            "my-null-sink",
        ] {
            assert_eq!(
                parse_metadata_name(&sink_default_json(name)).as_deref(),
                Some(name)
            );
        }
    }
}
