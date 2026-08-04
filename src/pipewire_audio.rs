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
use pipewire::device::{Device, DeviceInfoRef, DeviceListener};
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
    SPA_PARAM_AVAILABILITY_no, SPA_PARAM_AVAILABILITY_yes, SPA_PARAM_PROFILE_index,
    SPA_PARAM_Props, SPA_PARAM_ROUTE_available, SPA_PARAM_ROUTE_description,
    SPA_PARAM_ROUTE_device, SPA_PARAM_ROUTE_devices, SPA_PARAM_ROUTE_direction,
    SPA_PARAM_ROUTE_index, SPA_PARAM_ROUTE_name, SPA_PARAM_ROUTE_priority,
    SPA_PARAM_ROUTE_profiles, SPA_PARAM_ROUTE_save, SPA_PROP_channelVolumes, SPA_PROP_mute,
    SPA_PROP_volume, SPA_TYPE_OBJECT_ParamProfile, SPA_TYPE_OBJECT_ParamRoute,
    SPA_TYPE_OBJECT_Props, SPA_DIRECTION_INPUT, SPA_DIRECTION_OUTPUT,
};
use pipewire::types::ObjectType;

use crate::audio::{
    pw_linear_to_volume, sink_default_json, volume_to_pw_linear, AudioBackend, AudioCard,
    AudioCards, AudioStatus, MicStatus, PortAvailability, PortDirection, RouteInfo, SinkCard,
    SinkInfo, SinkList, SourceInfo, SourceList, MAX_VOLUME,
};
use crate::synoik::State;

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

/// A bound default-source node, kept alive so its `Props` (mute + volume) events flow.
struct BoundSource {
    id: u32,
    node: Node,
    _listener: NodeListener,
    /// Channel count from the last `channelVolumes` read, so writes match. Defaults 1 — most mics
    /// are mono, unlike the stereo sink default.
    channels: usize,
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

/// A tracked `Audio/Sink` or `Audio/Source` node. Everything here is captured once, when the global
/// appears — props are only live inside that registry callback — and never updated; node
/// descriptions and card membership are effectively static.
struct TrackedNode {
    name: String,
    description: String,
    /// The `Audio/Device` global this node belongs to (`device.id`), absent on virtual nodes with
    /// no card behind them (e.g. `auto_null`). This one IS in the registry global props.
    device_id: Option<u32>,
    /// The SPA device index within that card (`card.profile.device`), which is what a route's
    /// `device` field matches — the join that says which node a port row must make default. Comes
    /// from the node's `info` event; the registry global does NOT carry it.
    card_profile_device: Option<u32>,
    /// `device.form_factor`, also info-only. The first branch of `_findHeadphones`.
    form_factor: Option<String>,
    /// Owned global, for binding the node later (it isn't `Clone`).
    global: GlobalObject<PropertiesBox>,
    /// Every sink/source is bound with an **info-only** listener (no param subscription): the two
    /// fields above exist nowhere else. The default sink/source is additionally bound by
    /// [`bind_default`] for its `Props`; two proxies on one node is cheap and keeps the two
    /// concerns — "what is this node" vs "what is its volume" — from tangling.
    _node: Node,
    _listener: NodeListener,
}

/// A tracked `Audio/Device` — one sound card. Its routes are its ports, in gvc's sense.
struct TrackedCard {
    description: String,
    icon_name: Option<String>,
    /// Every route the card offers, accumulated from the `EnumRoute` param (PipeWire sends one pod
    /// per route, so this fills over several callbacks and resets when the enumeration restarts).
    ports: Vec<RouteInfo>,
    /// The active route per direction, from the `Route` param, same accumulate-and-reset rule.
    active: Vec<RouteInfo>,
    /// The active card profile, from the `Profile` param — routes outside it are not reachable
    /// without a profile switch, which we do not do yet.
    active_profile: Option<u32>,
    device: Device,
    _listener: DeviceListener,
}

#[derive(Default)]
struct Inner {
    /// Registry, so the metadata callback can bind a node when the default changes.
    registry: Option<RegistryRc>,
    /// `default` metadata proxy + listener (bound once).
    metadata: Option<(Metadata, MetadataListener)>,
    /// Known `Audio/Sink` nodes by PipeWire global id.
    sinks: HashMap<u32, TrackedNode>,
    /// Known `Audio/Device` cards by global id, with their routes.
    cards: HashMap<u32, TrackedCard>,
    /// The last card list handed to the compositor, and one it hasn't drained yet — published only
    /// on an actual change, like every other list here.
    cards_last: Option<AudioCards>,
    cards_dirty: Option<AudioCards>,
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

    // --- Microphone privacy indicator + input slider/picker (input side) ---
    /// Known `Audio/Source` nodes by PipeWire global id.
    sources: HashMap<u32, TrackedNode>,
    /// The default source's `node.name`, from `default.audio.source`.
    default_source_name: Option<String>,
    /// The currently-bound default source (for its mute + volume).
    bound_source: Option<BoundSource>,
    /// The default source's mute, `false` when unknown (no source/metadata) — see [`MicStatus`].
    mic_muted: bool,
    /// The default source's perceptual volume, `0.0` when unknown (no bound source).
    mic_volume: f64,
    /// The last source list handed to the compositor (for the input-device picker), and one it
    /// hasn't drained yet — published only on an actual change, mirroring the sink path.
    source_list_last: Option<SourceList>,
    source_list_dirty: Option<SourceList>,
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
            volume: self.mic_volume,
            source_present: self.bound_source.is_some(),
        };
        if self.mic_last != Some(status) {
            self.mic_last = Some(status);
            self.mic_dirty = Some(status);
        }
    }

    /// Rebuild the source list from the tracked sources + current default, and flag it for the
    /// compositor only on an actual change. Sorted by PipeWire global id, mirroring
    /// [`publish_sinks`].
    fn publish_sources(&mut self) {
        let mut ids: Vec<u32> = self.sources.keys().copied().collect();
        ids.sort_unstable();
        let list = SourceList {
            sources: ids
                .iter()
                .map(|id| {
                    let node = &self.sources[id];
                    SourceInfo {
                        name: node.name.clone(),
                        description: node.description.clone(),
                        card: node.device_id.map(|card_id| SinkCard {
                            card_id,
                            device: node.card_profile_device,
                        }),
                    }
                })
                .collect(),
            default_name: self.default_source_name.clone(),
        };
        if self.source_list_last.as_ref() != Some(&list) {
            self.source_list_last = Some(list.clone());
            self.source_list_dirty = Some(list);
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
                    let node = &self.sinks[id];
                    SinkInfo {
                        name: node.name.clone(),
                        description: node.description.clone(),
                        card: node.device_id.map(|card_id| SinkCard {
                            card_id,
                            device: node.card_profile_device,
                        }),
                        form_factor: node.form_factor.clone(),
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

    /// Rebuild the card/route model and flag it for the compositor only on an actual change. Sorted
    /// by PipeWire global id, like every other list here.
    fn publish_cards(&mut self) {
        let mut ids: Vec<u32> = self.cards.keys().copied().collect();
        ids.sort_unstable();
        let cards = AudioCards {
            cards: ids
                .iter()
                .map(|id| {
                    let card = &self.cards[id];
                    AudioCard {
                        id: *id,
                        description: card.description.clone(),
                        icon_name: card.icon_name.clone(),
                        ports: card.ports.clone(),
                        active: card.active.clone(),
                        active_profile: card.active_profile,
                    }
                })
                .collect(),
        };
        if self.cards_last.as_ref() != Some(&cards) {
            self.cards_last = Some(cards.clone());
            self.cards_dirty = Some(cards);
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
                let (dirty, mic_dirty, sink_list_dirty, source_list_dirty, cards_dirty) = {
                    let mut inner = inner.borrow_mut();
                    (
                        inner.dirty.take(),
                        inner.mic_dirty.take(),
                        inner.sink_list_dirty.take(),
                        inner.source_list_dirty.take(),
                        inner.cards_dirty.take(),
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
                if let Some(list) = source_list_dirty {
                    state.on_source_list(list);
                }
                if let Some(cards) = cards_dirty {
                    state.on_audio_cards(cards);
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
    /// Manually pump the loop once (used at startup so the initial state lands
    /// without waiting for the first fd wakeup).
    pub fn pump(&self) {
        self.main_loop.loop_().iterate(Duration::ZERO);
    }
}

/// The live backend behind [`crate::audio::AudioBackend`]. Everything the compositor calls goes
/// through the trait, so a headless fixture can swap in `StubAudio`.
impl AudioBackend for PwAudio {
    /// The last-known default-sink state, or `None` if no sink is bound yet.
    fn status(&self) -> Option<AudioStatus> {
        let inner = self.inner.borrow();
        inner.present.then_some(inner.status)
    }

    /// The last-known microphone activity (recording + mute). `None` until the first capture stream
    /// is seen; the compositor treats `None` as "not recording".
    fn mic_status(&self) -> Option<MicStatus> {
        self.inner.borrow().mic_last
    }

    /// Set the perceptual volume (clamped to `[0, MAX_VOLUME]`) on the default sink.
    /// Returns the optimistically-updated status for immediate UI feedback (the
    /// node's echo confirms it a moment later).
    fn set_volume(&self, volume: f64) -> Option<AudioStatus> {
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

    /// Set the mute flag on the default sink.
    fn set_muted(&self, muted: bool) -> Option<AudioStatus> {
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
    fn toggle_muted(&self) -> Option<AudioStatus> {
        let muted = self.status()?.muted;
        self.set_muted(!muted)
    }

    /// Set the perceptual volume on the default **source** (mic slider). Mirrors [`set_volume`]:
    /// writes `channelVolumes` to the bound source and optimistically publishes the mic status so
    /// the slider and the panel privacy tint react before the echo. No-op if no source is bound.
    fn set_input_volume(&self, volume: f64) -> Option<MicStatus> {
        let volume = volume.clamp(0.0, MAX_VOLUME);
        let mut inner = self.inner.borrow_mut();
        let bound = inner.bound_source.as_ref()?;
        let linear = volume_to_pw_linear(volume) as f32;
        let vols = vec![linear; bound.channels.max(1)];
        set_props(&bound.node, Some(vols), None);
        inner.mic_volume = volume;
        inner.publish_mic();
        inner.mic_last
    }

    /// Set the mute flag on the default **source**. Mirrors [`set_muted`].
    fn set_input_muted(&self, muted: bool) -> Option<MicStatus> {
        let mut inner = self.inner.borrow_mut();
        let bound = inner.bound_source.as_ref()?;
        set_props(&bound.node, None, Some(muted));
        inner.mic_muted = muted;
        inner.publish_mic();
        inner.mic_last
    }

    /// Flip the mute flag on the default source.
    fn toggle_input_muted(&self) -> Option<MicStatus> {
        let muted = self.inner.borrow().mic_muted;
        self.set_input_muted(!muted)
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
    fn set_default_sink(&self, node_name: &str) {
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

    /// Set the system default **input** to the source with this `node.name`, by writing the
    /// persistent `default.configured.audio.source` metadata key. The input mirror of
    /// [`set_default_sink`]; WirePlumber echoes it back as `default.audio.source`, moving the
    /// picker's check. No-op if no `default` metadata is bound.
    /// Write a `Route` param to the card selecting `route_index` on `device`, with `save: true` so
    /// the session manager persists the choice (what `wpctl set-route` and pavucontrol do). The
    /// card echoes a fresh `Route`, which is what moves the picker's check — nothing optimistic.
    fn set_route(&self, card_id: u32, device: u32, route_index: u32) {
        let inner = self.inner.borrow();
        let Some(card) = inner.cards.get(&card_id) else {
            return;
        };
        let object = Value::Object(Object {
            type_: SPA_TYPE_OBJECT_ParamRoute,
            id: pipewire::spa::sys::SPA_PARAM_Route,
            properties: vec![
                Property {
                    key: SPA_PARAM_ROUTE_index,
                    flags: PropertyFlags::empty(),
                    value: Value::Int(route_index as i32),
                },
                Property {
                    key: SPA_PARAM_ROUTE_device,
                    flags: PropertyFlags::empty(),
                    value: Value::Int(device as i32),
                },
                Property {
                    key: SPA_PARAM_ROUTE_save,
                    flags: PropertyFlags::empty(),
                    value: Value::Bool(true),
                },
            ],
        });
        let mut bytes = Vec::new();
        if PodSerializer::serialize(Cursor::new(&mut bytes), &object).is_err() {
            warn!("failed to serialize audio Route pod");
            return;
        }
        if let Some(pod) = Pod::from_bytes(&bytes) {
            card.device.set_param(ParamType::Route, 0, pod);
        }
    }

    fn set_default_source(&self, node_name: &str) {
        let inner = self.inner.borrow();
        let Some((metadata, _)) = inner.metadata.as_ref() else {
            return;
        };
        metadata.set_property(
            0,
            "default.configured.audio.source",
            Some("Spa:String:JSON"),
            Some(&sink_default_json(node_name)),
        );
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
                let mut inner = inner_rc.borrow_mut();
                let Some(registry) = inner.registry.clone() else {
                    return;
                };
                let Some(node) = track_node(&registry, weak, obj, props, true) else {
                    return;
                };
                let name = node.name.clone();
                inner.sinks.insert(obj.id, node);
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
                let mut inner = inner_rc.borrow_mut();
                let Some(registry) = inner.registry.clone() else {
                    return;
                };
                let Some(node) = track_node(&registry, weak, obj, props, false) else {
                    return;
                };
                let name = node.name.clone();
                inner.sources.insert(obj.id, node);
                if inner.default_source_name.as_deref() == Some(name.as_str())
                    && inner.bound_source.as_ref().map(|b| b.id) != Some(obj.id)
                {
                    bind_default_source(&mut inner, weak);
                }
                inner.publish_sources();
            } else if class == "Stream/Input/Audio" {
                // An application recording from a mic. Bind it to watch its run-state.
                track_capture(&inner_rc, weak, obj, props);
            }
        }
        // A sound card. Its routes are gvc's ports: what the device picker lists, and what the
        // headphone detection reads (`gvc-mixer-control.c:1973-2006`).
        ObjectType::Device => {
            let Some(props) = obj.props else { return };
            // Only audio cards; PipeWire also surfaces Video/Device (cameras) here.
            if !props
                .get("media.class")
                .is_some_and(|c| c.starts_with("Audio/"))
            {
                return;
            }
            track_card(&inner_rc, weak, obj, props);
        }
        ObjectType::Metadata => {
            let is_default = obj.props.and_then(|p| p.get("metadata.name")) == Some("default");
            if !is_default {
                return;
            }
            let mut inner = inner_rc.borrow_mut();
            // Bound once and never released: `on_global_remove` doesn't track the metadata global's
            // id, so if the session manager (WirePlumber) restarts, this proxy goes stale — the
            // default-sink read stops updating and `set_default_sink` writes into the dead proxy.
            // Pre-existing (the volume read path already relied on this single bind); fine in
            // practice (WirePlumber rarely restarts mid-session). TODO: track the id and rebind on
            // removal to harden both the read and the picker's write.
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

/// Capture everything we need from an `Audio/Sink` / `Audio/Source` node's props, which are only
/// live inside the registry callback. `None` when the node has no `node.name` — the key we match
/// the default against, so a node without one is unusable.
fn track_node(
    registry: &RegistryRc,
    weak: &Weak<RefCell<Inner>>,
    obj: &GlobalObject<&pipewire::spa::utils::dict::DictRef>,
    props: &pipewire::spa::utils::dict::DictRef,
    is_sink: bool,
) -> Option<TrackedNode> {
    let name = props.get("node.name")?.to_string();
    // Human label for the picker, in gvc's preference order; last-resort the machine name.
    let description = props
        .get("node.description")
        .or_else(|| props.get("node.nick"))
        .or_else(|| props.get("device.description"))
        .unwrap_or(name.as_str())
        .to_string();
    let id = obj.id;
    let node = match registry.bind::<Node, _>(obj) {
        Ok(node) => node,
        Err(_) => {
            warn!("failed to bind audio node {id} for its info");
            return None;
        }
    };
    let listener = {
        let weak = weak.clone();
        node.add_listener_local()
            .info(move |info| on_node_info(&weak, id, is_sink, info))
            .register()
    };
    Some(TrackedNode {
        name,
        description,
        device_id: props.get("device.id").and_then(|v| v.parse().ok()),
        card_profile_device: None,
        form_factor: None,
        global: obj.to_owned(),
        _node: node,
        _listener: listener,
    })
}

/// A sink/source node's `info` arrived: pick up the props a registry global does not carry.
fn on_node_info(weak: &Weak<RefCell<Inner>>, id: u32, is_sink: bool, info: &NodeInfoRef) {
    let Some(props) = info.props() else { return };
    let card_profile_device = props
        .get("card.profile.device")
        .and_then(|v| v.parse::<u32>().ok());
    let form_factor = props.get("device.form_factor").map(str::to_string);
    let device_id = props.get("device.id").and_then(|v| v.parse::<u32>().ok());
    let Some(inner_rc) = weak.upgrade() else {
        return;
    };
    let mut inner = inner_rc.borrow_mut();
    let map = if is_sink {
        &mut inner.sinks
    } else {
        &mut inner.sources
    };
    let Some(node) = map.get_mut(&id) else {
        return;
    };
    // `info` repeats with partial change masks; never clear a known value with nothing.
    if card_profile_device.is_some() {
        node.card_profile_device = card_profile_device;
    }
    if form_factor.is_some() {
        node.form_factor = form_factor;
    }
    if device_id.is_some() {
        node.device_id = device_id;
    }
    // Both publishers dedup, so a repeat with nothing new is a no-op.
    if is_sink {
        inner.publish_sinks();
    } else {
        inner.publish_sources();
    }
}

/// Bind a card and subscribe to its routes. `EnumRoute` gives every port the card has; `Route`
/// gives the active one per direction.
fn track_card(
    inner_rc: &Rc<RefCell<Inner>>,
    weak: &Weak<RefCell<Inner>>,
    obj: &GlobalObject<&pipewire::spa::utils::dict::DictRef>,
    props: &pipewire::spa::utils::dict::DictRef,
) {
    let id = obj.id;
    let mut inner = inner_rc.borrow_mut();
    if inner.cards.contains_key(&id) {
        return;
    }
    let Some(registry) = inner.registry.clone() else {
        return;
    };
    let device = match registry.bind::<Device, _>(obj) {
        Ok(device) => device,
        Err(_) => {
            warn!("failed to bind audio device {id}");
            return;
        }
    };
    let listener = {
        let weak = weak.clone();
        let weak_info = weak.clone();
        device
            .add_listener_local()
            .param(move |_seq, param_id, index, _next, pod| {
                if let Some(pod) = pod {
                    on_device_param(&weak, id, param_id, index, pod);
                }
            })
            // `device.icon-name` is info-only, like the node props above — the registry global for
            // this card carries description/nick/name but no icon.
            .info(move |info| on_device_info(&weak_info, id, info))
            .register()
    };
    device.subscribe_params(&[ParamType::EnumRoute, ParamType::Route, ParamType::Profile]);
    inner.cards.insert(
        id,
        TrackedCard {
            // gvc's card name, which becomes a device row's "origin"
            // (`gvc-mixer-control.c:1982`, `gvc_mixer_card_set_name` from `device.description`).
            description: props
                .get("device.description")
                .or_else(|| props.get("device.nick"))
                .or_else(|| props.get("device.name"))
                .unwrap_or_default()
                .to_string(),
            // Filled from the `info` event below; the registry global has no icon.
            icon_name: None,
            ports: Vec::new(),
            active: Vec::new(),
            active_profile: None,
            device,
            _listener: listener,
        },
    );
    inner.publish_cards();
}

/// A card's `info` arrived: pick up `device.icon-name` (the picker row's icon, and the fallback for
/// a device with no icon of its own, `gvc-mixer-ui-device.c:632-643`) and a better description if
/// the info props carry one. Neither is in the registry global.
fn on_device_info(weak: &Weak<RefCell<Inner>>, card_id: u32, info: &DeviceInfoRef) {
    let Some(props) = info.props() else { return };
    let icon_name = props.get("device.icon-name").map(str::to_string);
    let description = props.get("device.description").map(str::to_string);
    let Some(inner_rc) = weak.upgrade() else {
        return;
    };
    let mut inner = inner_rc.borrow_mut();
    let Some(card) = inner.cards.get_mut(&card_id) else {
        return;
    };
    // Never clear a known value: `info` repeats with partial change masks.
    if icon_name.is_some() {
        card.icon_name = icon_name;
    }
    if let Some(description) = description {
        card.description = description;
    }
    inner.publish_cards();
}

/// A card's route param arrived. PipeWire sends **one pod per route**, indexed from 0, so each
/// enumeration is accumulated and an `index == 0` starts a fresh one — otherwise a re-subscribe (or
/// a profile switch, which re-enumerates) would append duplicates forever.
fn on_device_param(
    weak: &Weak<RefCell<Inner>>,
    card_id: u32,
    param_id: ParamType,
    index: u32,
    pod: &Pod,
) {
    // The active profile decides which routes are reachable; handled here rather than in a second
    // callback because it arrives on the same listener.
    if param_id == ParamType::Profile {
        let Some(index) = parse_profile_index(pod) else {
            return;
        };
        let Some(inner_rc) = weak.upgrade() else {
            return;
        };
        let mut inner = inner_rc.borrow_mut();
        let Some(card) = inner.cards.get_mut(&card_id) else {
            return;
        };
        if card.active_profile == Some(index) {
            return;
        }
        card.active_profile = Some(index);
        inner.publish_cards();
        return;
    }
    let is_enum = match param_id {
        ParamType::EnumRoute => true,
        ParamType::Route => false,
        _ => return,
    };
    let Some(route) = parse_route(pod) else {
        return;
    };
    let Some(inner_rc) = weak.upgrade() else {
        return;
    };
    let mut inner = inner_rc.borrow_mut();
    let Some(card) = inner.cards.get_mut(&card_id) else {
        return;
    };
    let list = if is_enum {
        &mut card.ports
    } else {
        &mut card.active
    };
    if index == 0 {
        list.clear();
    }
    list.push(route);
    inner.publish_cards();
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
    if inner.sources.remove(&id).is_some() {
        inner.publish_sources();
    }
    if inner.cards.remove(&id).is_some() {
        inner.publish_cards();
    }
    if inner.bound.as_ref().map(|b| b.id) == Some(id) {
        inner.bound = None;
        inner.publish(None);
    }
    if inner.bound_source.as_ref().map(|b| b.id) == Some(id) {
        inner.bound_source = None;
        inner.mic_muted = false; // mute state is now unknown → treat as a privacy event
        inner.mic_volume = 0.0; // and the level is unknown
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
        // The picker marks the default row, so a default change must republish the list.
        inner.publish_sources();
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
        .find(|(_, node)| node.name == name)
        .map(|(id, _)| *id)
    else {
        return; // node not surfaced yet; on_global will bind it when it appears
    };
    if inner.bound.as_ref().map(|b| b.id) == Some(id) {
        return;
    }
    // Bind from a borrow of the stored global (GlobalObject isn't Clone); the borrow
    // ends before we write `inner.bound` below.
    let node = match registry.bind::<Node, _>(&inner.sinks[&id].global) {
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
        .find(|(_, node)| node.name == name)
        .map(|(id, _)| *id)
    else {
        return; // node not surfaced yet; on_global will bind it when it appears
    };
    if inner.bound_source.as_ref().map(|b| b.id) == Some(id) {
        return;
    }
    let node = match registry.bind::<Node, _>(&inner.sources[&id].global) {
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
        node,
        _listener: listener,
        channels: 1,
    });
}

/// A default-source `Props` param arrived: pull `mute` + `channelVolumes` and recompute the mic
/// status. Props events can be partial (mute-only or volume-only), so — like [`on_node_param`] —
/// each field falls back to the last-known value rather than dropping the whole pod.
// The SPA property constants keep their C mixed-case names; matched as constants.
#[allow(non_upper_case_globals)]
fn on_source_param(weak: &Weak<RefCell<Inner>>, pod: &Pod) {
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
        if let Some(bound) = inner.bound_source.as_mut() {
            bound.channels = ch.max(1);
        }
    }
    let volume = match linear.or(mono) {
        Some(l) => pw_linear_to_volume(l),
        None => inner.mic_volume,
    };
    let muted = muted.unwrap_or(inner.mic_muted);
    inner.mic_muted = muted;
    inner.mic_volume = volume;
    // `publish_mic` dedups against the last status, so an unchanged pod is a no-op.
    inner.publish_mic();
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

/// Parse a `SPA_TYPE_OBJECT_ParamRoute` pod into a [`RouteInfo`] — the one form both `EnumRoute`
/// (every route on the card) and `Route` (the active one) arrive in.
///
/// Returns `None` for a pod that is not a route object at all; unknown/missing fields fall back to
/// the [`Default`], since PipeWire is free to omit any of them and a partial route is still worth
/// listing. Pure, so it unit-tests against hand-built pods without a PipeWire connection — which is
/// the only way to cover multi-port cards and availability flips on a machine whose card has
/// exactly one route.
// The SPA property constants keep their C mixed-case names; matched as constants.
#[allow(non_upper_case_globals)]
fn parse_route(pod: &Pod) -> Option<RouteInfo> {
    let (_, Value::Object(obj)) =
        PodDeserializer::deserialize_from::<Value>(pod.as_bytes()).ok()?
    else {
        return None;
    };
    if obj.type_ != SPA_TYPE_OBJECT_ParamRoute {
        return None;
    }
    let mut route = RouteInfo::default();
    for prop in &obj.properties {
        match (prop.key, &prop.value) {
            (SPA_PARAM_ROUTE_index, Value::Int(v)) => route.index = *v as u32,
            (SPA_PARAM_ROUTE_direction, Value::Id(id)) => {
                route.direction = match id.0 {
                    SPA_DIRECTION_INPUT => Some(PortDirection::Input),
                    SPA_DIRECTION_OUTPUT => Some(PortDirection::Output),
                    _ => None,
                };
            }
            (SPA_PARAM_ROUTE_name, Value::String(s)) => route.name = s.clone(),
            (SPA_PARAM_ROUTE_description, Value::String(s)) => route.description = s.clone(),
            (SPA_PARAM_ROUTE_priority, Value::Int(v)) => route.priority = (*v).max(0) as u32,
            (SPA_PARAM_ROUTE_available, Value::Id(id)) => {
                route.available = match id.0 {
                    SPA_PARAM_AVAILABILITY_no => PortAvailability::No,
                    SPA_PARAM_AVAILABILITY_yes => PortAvailability::Yes,
                    // Includes SPA_PARAM_AVAILABILITY_unknown, and anything new.
                    _ => PortAvailability::Unknown,
                };
            }
            (SPA_PARAM_ROUTE_device, Value::Int(v)) => route.device = Some(*v as u32),
            (SPA_PARAM_ROUTE_devices, Value::ValueArray(ValueArray::Int(v))) => {
                route.devices = v.iter().map(|d| *d as u32).collect();
            }
            (SPA_PARAM_ROUTE_profiles, Value::ValueArray(ValueArray::Int(v))) => {
                route.profiles = v.iter().map(|p| *p as u32).collect();
            }
            _ => {}
        }
    }
    Some(route)
}

/// Parse a `SPA_TYPE_OBJECT_ParamProfile` pod for its `index` — the card's active profile, which
/// decides which routes are reachable right now (a route lists the profiles it belongs to).
#[allow(non_upper_case_globals)]
fn parse_profile_index(pod: &Pod) -> Option<u32> {
    let (_, Value::Object(obj)) =
        PodDeserializer::deserialize_from::<Value>(pod.as_bytes()).ok()?
    else {
        return None;
    };
    if obj.type_ != SPA_TYPE_OBJECT_ParamProfile {
        return None;
    }
    obj.properties
        .iter()
        .find_map(|prop| match (prop.key, &prop.value) {
            (SPA_PARAM_PROFILE_index, Value::Int(v)) => Some(*v as u32),
            _ => None,
        })
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
    use pipewire::spa::utils::Id;

    use super::*;
    use crate::audio::sink_default_json;

    /// Serialize a route object the way PipeWire sends one, so the parser is tested against a real
    /// pod rather than a mock. `device` present makes it the active-`Route` form; absent, the
    /// `EnumRoute` form.
    #[allow(clippy::too_many_arguments)]
    fn route_pod(
        index: i32,
        direction: u32,
        name: &str,
        description: &str,
        priority: i32,
        available: u32,
        devices: &[i32],
        device: Option<i32>,
    ) -> Vec<u8> {
        let mut properties = vec![
            Property {
                key: SPA_PARAM_ROUTE_index,
                flags: PropertyFlags::empty(),
                value: Value::Int(index),
            },
            Property {
                key: SPA_PARAM_ROUTE_direction,
                flags: PropertyFlags::empty(),
                value: Value::Id(Id(direction)),
            },
            Property {
                key: SPA_PARAM_ROUTE_name,
                flags: PropertyFlags::empty(),
                value: Value::String(name.to_owned()),
            },
            Property {
                key: SPA_PARAM_ROUTE_description,
                flags: PropertyFlags::empty(),
                value: Value::String(description.to_owned()),
            },
            Property {
                key: SPA_PARAM_ROUTE_priority,
                flags: PropertyFlags::empty(),
                value: Value::Int(priority),
            },
            Property {
                key: SPA_PARAM_ROUTE_available,
                flags: PropertyFlags::empty(),
                value: Value::Id(Id(available)),
            },
            Property {
                key: SPA_PARAM_ROUTE_devices,
                flags: PropertyFlags::empty(),
                value: Value::ValueArray(ValueArray::Int(devices.to_vec())),
            },
        ];
        if let Some(device) = device {
            properties.push(Property {
                key: SPA_PARAM_ROUTE_device,
                flags: PropertyFlags::empty(),
                value: Value::Int(device),
            });
        }
        let object = Value::Object(Object {
            type_: SPA_TYPE_OBJECT_ParamRoute,
            id: pipewire::spa::sys::SPA_PARAM_Route,
            properties,
        });
        let mut bytes = Vec::new();
        PodSerializer::serialize(Cursor::new(&mut bytes), &object).unwrap();
        bytes
    }

    /// The one route this machine's card actually has, byte for byte as `pw-dump` reports it:
    /// `index=0 direction=Output name="analog-output" description="Analog Output" priority=9900
    /// available=unknown device=1 devices=[1]`.
    ///
    /// The `available` value is the point. It is `unknown`, not `yes` — so a filter written as
    /// `== yes` would drop this card's only output from the picker. gvc's test is
    /// `!= PA_PORT_AVAILABLE_NO` (`gvc-mixer-control.c:1973,1995`), which is what
    /// [`PortAvailability::is_offerable`] implements.
    #[test]
    fn parses_the_real_cards_active_route() {
        let bytes = route_pod(
            0,
            SPA_DIRECTION_OUTPUT,
            "analog-output",
            "Analog Output",
            9900,
            pipewire::spa::sys::SPA_PARAM_AVAILABILITY_unknown,
            &[1],
            Some(1),
        );
        let route = parse_route(Pod::from_bytes(&bytes).unwrap()).unwrap();
        assert_eq!(
            route,
            RouteInfo {
                index: 0,
                direction: Some(PortDirection::Output),
                name: "analog-output".to_owned(),
                description: "Analog Output".to_owned(),
                priority: 9900,
                available: PortAvailability::Unknown,
                devices: vec![1],
                device: Some(1),
                profiles: vec![],
            }
        );
        assert!(
            route.available.is_offerable(),
            "unknown availability must still be offered, or this card loses its only output"
        );
    }

    /// A multi-port card, which no hardware here has: speakers + headphones on one card, headphones
    /// unplugged. Covers the shape slice 3's device list is built from, and the availability filter
    /// that decides which rows exist at all.
    #[test]
    fn parses_a_multi_port_card_and_its_availability() {
        let speakers = route_pod(
            0,
            SPA_DIRECTION_OUTPUT,
            "analog-output-speaker",
            "Speakers",
            9000,
            pipewire::spa::sys::SPA_PARAM_AVAILABILITY_yes,
            &[0],
            None,
        );
        let headphones = route_pod(
            1,
            SPA_DIRECTION_OUTPUT,
            "analog-output-headphones",
            "Headphones",
            9900,
            SPA_PARAM_AVAILABILITY_no,
            &[0],
            None,
        );
        let mic = route_pod(
            2,
            SPA_DIRECTION_INPUT,
            "analog-input-internal-mic",
            "Internal Microphone",
            8000,
            pipewire::spa::sys::SPA_PARAM_AVAILABILITY_unknown,
            &[0],
            None,
        );

        let speakers = parse_route(Pod::from_bytes(&speakers).unwrap()).unwrap();
        let headphones = parse_route(Pod::from_bytes(&headphones).unwrap()).unwrap();
        let mic = parse_route(Pod::from_bytes(&mic).unwrap()).unwrap();

        assert_eq!(speakers.description, "Speakers");
        assert!(speakers.available.is_offerable());
        // Unplugged headphones are the one case gvc drops.
        assert_eq!(headphones.available, PortAvailability::No);
        assert!(!headphones.available.is_offerable());
        // Direction is what splits the output picker from the input one.
        assert_eq!(mic.direction, Some(PortDirection::Input));
        assert_eq!(speakers.direction, Some(PortDirection::Output));
        // The EnumRoute form carries `devices` but no single `device` — only the active Route does.
        assert_eq!(speakers.device, None);
        assert_eq!(speakers.devices, vec![0]);
    }

    /// A pod that is not a route at all must be rejected rather than parsed into a default-filled
    /// route: the device listener sees every param it subscribed to, and a `Props` object would
    /// otherwise become a nameless phantom port.
    #[test]
    fn a_non_route_pod_is_rejected() {
        let object = Value::Object(Object {
            type_: SPA_TYPE_OBJECT_Props,
            id: SPA_PARAM_Props,
            properties: vec![Property {
                key: SPA_PROP_mute,
                flags: PropertyFlags::empty(),
                value: Value::Bool(true),
            }],
        });
        let mut bytes = Vec::new();
        PodSerializer::serialize(Cursor::new(&mut bytes), &object).unwrap();
        assert_eq!(parse_route(Pod::from_bytes(&bytes).unwrap()), None);
    }

    /// A route with fields missing still parses — PipeWire may omit any of them, and a partial
    /// route is worth listing. Only the object type is mandatory.
    #[test]
    fn a_sparse_route_falls_back_to_defaults() {
        let object = Value::Object(Object {
            type_: SPA_TYPE_OBJECT_ParamRoute,
            id: pipewire::spa::sys::SPA_PARAM_Route,
            properties: vec![Property {
                key: SPA_PARAM_ROUTE_index,
                flags: PropertyFlags::empty(),
                value: Value::Int(7),
            }],
        });
        let mut bytes = Vec::new();
        PodSerializer::serialize(Cursor::new(&mut bytes), &object).unwrap();
        let route = parse_route(Pod::from_bytes(&bytes).unwrap()).unwrap();
        assert_eq!(route.index, 7);
        assert_eq!(route.direction, None);
        assert_eq!(route.name, "");
        assert_eq!(
            route.available,
            PortAvailability::Unknown,
            "an absent availability is unknown, which is still offerable"
        );
    }

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
