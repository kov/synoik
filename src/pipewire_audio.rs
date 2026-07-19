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
use pipewire::node::{Node, NodeListener};
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

use crate::audio::{pw_linear_to_volume, volume_to_pw_linear, AudioStatus, MAX_VOLUME};
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

#[derive(Default)]
struct Inner {
    /// Registry, so the metadata callback can bind a node when the default changes.
    registry: Option<RegistryRc>,
    /// `default` metadata proxy + listener (bound once).
    metadata: Option<(Metadata, MetadataListener)>,
    /// Known `Audio/Sink` nodes: id → (node.name, owned global for late binding).
    sinks: HashMap<u32, (String, GlobalObject<PropertiesBox>)>,
    /// The default sink's `node.name`, from `default.audio.sink`.
    default_name: Option<String>,
    /// The currently-bound default sink.
    bound: Option<BoundSink>,
    /// Latest status, and whether it changed since the compositor last drained it.
    status: AudioStatus,
    present: bool,
    dirty: Option<Option<AudioStatus>>,
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
}

/// Connect to PipeWire and start tracking the default sink. Returns `None`-worthy
/// errors as `Err` so the caller can log and carry on without audio.
pub fn start(loop_handle: &LoopHandle<'static, State>) -> anyhow::Result<PwAudio> {
    let main_loop = MainLoopRc::new(None).context("creating pipewire MainLoop")?;
    let context = ContextRc::new(&main_loop, None).context("creating pipewire Context")?;
    let core = context
        .connect_rc(None)
        .context("connecting to pipewire")?;
    let registry = core.get_registry_rc().context("getting pipewire registry")?;

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
                let dirty = inner.borrow_mut().dirty.take();
                if let Some(status) = dirty {
                    state.on_audio_status(status);
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
fn on_global(weak: &Weak<RefCell<Inner>>, obj: &GlobalObject<&pipewire::spa::utils::dict::DictRef>) {
    let Some(inner_rc) = weak.upgrade() else {
        return;
    };
    match obj.type_ {
        ObjectType::Node => {
            let Some(props) = obj.props else { return };
            if props.get("media.class") != Some("Audio/Sink") {
                return;
            }
            let Some(name) = props.get("node.name") else {
                return;
            };
            let name = name.to_string();
            let mut inner = inner_rc.borrow_mut();
            inner.sinks.insert(obj.id, (name.clone(), obj.to_owned()));
            // If this is the default sink and nothing is bound yet, bind it now.
            if inner.default_name.as_deref() == Some(name.as_str())
                && inner.bound.as_ref().map(|b| b.id) != Some(obj.id)
            {
                bind_default(&mut inner, weak);
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
    inner.sinks.remove(&id);
    if inner.bound.as_ref().map(|b| b.id) == Some(id) {
        inner.bound = None;
        inner.publish(None);
    }
}

/// The `default` metadata changed. When `default.audio.sink` moves, rebind.
fn on_metadata_property(
    weak: &Weak<RefCell<Inner>>,
    subject: u32,
    key: Option<&str>,
    value: Option<&str>,
) {
    if subject != 0 || key != Some("default.audio.sink") {
        return;
    }
    let Some(name) = value.and_then(parse_metadata_name) else {
        return;
    };
    let Some(inner_rc) = weak.upgrade() else {
        return;
    };
    let mut inner = inner_rc.borrow_mut();
    if inner.default_name.as_deref() == Some(name.as_str()) {
        return;
    }
    inner.default_name = Some(name);
    bind_default(&mut inner, weak);
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
        .find(|(_, (n, _))| *n == name)
        .map(|(id, _)| *id)
    else {
        return; // node not surfaced yet; on_global will bind it when it appears
    };
    if inner.bound.as_ref().map(|b| b.id) == Some(id) {
        return;
    }
    // Bind from a borrow of the stored global (GlobalObject isn't Clone); the borrow
    // ends before we write `inner.bound` below.
    let node = match registry.bind::<Node, _>(&inner.sinks[&id].1) {
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
                linear = vols.iter().map(|v| *v as f64).fold(None, |acc, v| {
                    Some(acc.map_or(v, |a: f64| a.max(v)))
                });
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
    let linear = linear.or(mono).unwrap_or_else(|| volume_to_pw_linear(inner.status.volume));
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
}
