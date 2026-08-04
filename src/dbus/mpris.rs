// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Session-bus watcher for MPRIS media players — gnome-shell's `MprisSource`
//! (`js/ui/mpris.js:189-254`).
//!
//! Every `org.mpris.MediaPlayer2.*` name on the session bus is a media player, so unlike our other
//! clients this one watches a *set* of names: `ListNames` seeds it and `NameOwnerChanged` keeps it
//! current. Each player gets its own task holding the two proxies GNOME holds
//! (`org.mpris.MediaPlayer2` and `…​.Player`, both at `/org/mpris/MediaPlayer2`), which re-reads
//! every property on any `PropertiesChanged` — gnome-shell's `_updateState` does the same off its
//! proxy cache. That task is the **sole writer** for its bus name: it sends the updates and, when
//! the name loses its owner, the removal. Nobody else may, or a read racing a removal would
//! resurrect a player that is gone.
//!
//! This is the untrusted side of the seam described in [`crate::mpris`]: all validation, capping
//! and the `file://`-only cover-art rule happen here, before anything crosses the channel.

use std::collections::HashSet;

use futures_util::StreamExt;
use zbus::names::InterfaceName;
use zbus::{fdo, zvariant};

use crate::mpris::{
    validate_metadata, MetaField, MprisToSynoik, PlaybackStatus, PlayerState, RawMetadata,
    SynoikToMpris, MPRIS_PATH, MPRIS_PLAYER_PREFIX,
};
use crate::notifications::{clamp_text, flatten_text};

const ROOT_IFACE: &str = "org.mpris.MediaPlayer2";
const PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";

/// Cap for `Identity`, matching the model's cap for track text.
const MAX_TEXT_BYTES: usize = 1024;
/// A desktop id longer than this is not one.
const MAX_DESKTOP_ENTRY_BYTES: usize = 255;

type Props = std::collections::HashMap<String, zvariant::OwnedValue>;

pub fn start(
    to_niri: calloop::channel::Sender<MprisToSynoik>,
    from_niri: async_channel::Receiver<SynoikToMpris>,
) -> anyhow::Result<zbus::blocking::Connection> {
    let conn = zbus::blocking::Connection::session()?;
    let async_conn = conn.inner().clone();

    let discover_conn = async_conn.clone();
    let discover = async move {
        let dbus = match fdo::DBusProxy::new(&discover_conn).await {
            Ok(proxy) => proxy,
            Err(err) => {
                warn!("error creating DBusProxy for MPRIS discovery: {err:?}");
                return;
            }
        };

        // Subscribe before listing, so a player that appears during the round trip is not missed.
        let mut owner_changed = match dbus.receive_name_owner_changed().await {
            Ok(stream) => stream,
            Err(err) => {
                warn!("error subscribing to NameOwnerChanged for MPRIS: {err:?}");
                return;
            }
        };

        let mut known: HashSet<String> = HashSet::new();
        match dbus.list_names().await {
            Ok(names) => {
                for name in names {
                    let name = name.as_str();
                    if name.starts_with(MPRIS_PLAYER_PREFIX) && known.insert(name.to_owned()) {
                        spawn_player(&discover_conn, name.to_owned(), to_niri.clone());
                    }
                }
            }
            Err(err) => warn!("error listing bus names for MPRIS: {err:?}"),
        }

        while let Some(signal) = owner_changed.next().await {
            let Ok(args) = signal.args() else {
                continue;
            };
            let name = args.name().as_str();
            if !name.starts_with(MPRIS_PLAYER_PREFIX) {
                continue;
            }

            // `_onNameOwnerChanged` (`mpris.js:238-253`): a lost owner drops the player, a gained
            // one adds it — a name that changed owner in one signal does both, in that order.
            if args.old_owner().is_some() {
                known.remove(name);
            }
            if args.new_owner().is_some() && known.insert(name.to_owned()) {
                spawn_player(&discover_conn, name.to_owned(), to_niri.clone());
            }
        }
    };
    conn.inner()
        .executor()
        .spawn(discover, "discover MPRIS players")
        .detach();

    let command_conn = async_conn.clone();
    let commands = async move {
        while let Ok(command) = from_niri.recv().await {
            let (bus_name, member) = match &command {
                SynoikToMpris::PlayPause(bus) => (bus, "PlayPause"),
                SynoikToMpris::Next(bus) => (bus, "Next"),
                SynoikToMpris::Previous(bus) => (bus, "Previous"),
                SynoikToMpris::Raise(bus) => (bus, "Raise"),
            };
            let iface = if member == "Raise" {
                ROOT_IFACE
            } else {
                PLAYER_IFACE
            };

            // Fire-and-forget, like every `…Async().catch(logError)` in `mpris.js:73-100`: a
            // wedged player must not stall the watcher, let alone the compositor.
            if let Err(err) = command_conn
                .call_method(
                    Some(bus_name.as_str()),
                    MPRIS_PATH,
                    Some(iface),
                    member,
                    &(),
                )
                .await
            {
                warn!("error calling {member} on {bus_name}: {err:?}");
            }
        }
    };
    conn.inner()
        .executor()
        .spawn(commands, "drive MPRIS players")
        .detach();

    Ok(conn)
}

fn spawn_player(
    conn: &zbus::Connection,
    bus_name: String,
    to_niri: calloop::channel::Sender<MprisToSynoik>,
) {
    let conn = conn.clone();
    let task_conn = conn.clone();
    conn.executor()
        .spawn(
            watch_player(task_conn, bus_name, to_niri),
            "watch an MPRIS player",
        )
        .detach();
}

/// One player's whole life: read, push, wait, repeat — and push exactly one removal when its name
/// loses its owner (`mpris.js:110-121` closes the proxies on the same edge). This task is the only
/// writer for its bus name.
async fn watch_player(
    conn: zbus::Connection,
    bus_name: String,
    to_niri: calloop::channel::Sender<MprisToSynoik>,
) {
    /// What woke the loop.
    enum Ev {
        Changed,
        OwnerGone,
    }

    let props = match fdo::PropertiesProxy::new(&conn, bus_name.clone(), MPRIS_PATH).await {
        Ok(proxy) => proxy,
        Err(err) => {
            warn!("error creating PropertiesProxy for {bus_name}: {err:?}");
            return;
        }
    };
    let dbus = match fdo::DBusProxy::new(&conn).await {
        Ok(proxy) => proxy,
        Err(err) => {
            warn!("error creating DBusProxy for {bus_name}: {err:?}");
            return;
        }
    };

    // Both streams before the first read, so nothing that happens during it is lost.
    let changed = match props.receive_properties_changed().await {
        Ok(stream) => stream.map(|_| Ev::Changed).boxed(),
        Err(err) => {
            warn!("error subscribing to PropertiesChanged for {bus_name}: {err:?}");
            return;
        }
    };
    let owner_gone = match dbus
        .receive_name_owner_changed_with_args(&[(0, bus_name.as_str())])
        .await
    {
        Ok(stream) => stream
            .filter_map(|signal| async move {
                let args = signal.args().ok()?;
                args.new_owner().is_none().then_some(Ev::OwnerGone)
            })
            .boxed(),
        Err(err) => {
            warn!("error subscribing to NameOwnerChanged for {bus_name}: {err:?}");
            return;
        }
    };
    let mut wake = futures_util::stream::select(changed, owner_gone);

    let root_iface = InterfaceName::try_from(ROOT_IFACE).unwrap();
    let player_iface = InterfaceName::try_from(PLAYER_IFACE).unwrap();

    let mut last: Option<PlayerState> = None;
    loop {
        // Both interfaces are mandatory for a player, so a name that cannot answer both is not
        // ready (or not a player at all): wait for the next edge rather than publish a blank card.
        let root = props.get_all(root_iface.clone()).await;
        let player = props.get_all(player_iface.clone()).await;
        if let (Ok(root), Ok(player)) = (root, player) {
            let state = read_player(&root, &player, &bus_name);
            if last.as_ref() != Some(&state) {
                last = Some(state.clone());
                let msg = MprisToSynoik::PlayerUpdated {
                    bus_name: bus_name.clone(),
                    state: Box::new(state),
                };
                if to_niri.send(msg).is_err() {
                    return;
                }
            }
        }

        match wake.next().await {
            Some(Ev::Changed) => (),
            // The name is gone, or the bus is: either way this player is over.
            Some(Ev::OwnerGone) | None => {
                let _ = to_niri.send(MprisToSynoik::PlayerRemoved { bus_name });
                return;
            }
        }
    }
}

/// Read both interfaces into a validated [`PlayerState`], logging the spec violations gnome-shell
/// logs (`mpris.js:140-142,150-153,160-163`) with the bus name attached.
pub(crate) fn read_player(root: &Props, player: &Props, bus_name: &str) -> PlayerState {
    let (title, artists, art, faults) = validate_metadata(&read_metadata(player));
    for fault in faults {
        warn!("faulty track metadata from {bus_name}: {fault}");
    }

    PlayerState {
        // `Identity` is app-chosen text like any other, so it is bounded here too.
        identity: clamp_text(flatten_text(&get_string(root, "Identity")), MAX_TEXT_BYTES),
        desktop_entry: desktop_entry(root),
        can_play: get_bool(player, "CanPlay"),
        can_raise: get_bool(root, "CanRaise"),
        can_go_next: get_bool(player, "CanGoNext"),
        can_go_previous: get_bool(player, "CanGoPrevious"),
        status: PlaybackStatus::parse(&get_string(player, "PlaybackStatus")),
        title,
        artists,
        art,
    }
}

/// `DesktopEntry` names a desktop file, so it is a *lookup key*, not display text: anything that
/// could escape the app id namespace is dropped rather than sanitized.
fn desktop_entry(root: &Props) -> Option<String> {
    let entry = get_string(root, "DesktopEntry");
    let sane = !entry.is_empty()
        && entry.len() <= MAX_DESKTOP_ENTRY_BYTES
        && entry
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    sane.then_some(entry)
}

/// The `a{sv}` `Metadata` property, reduced to the three keys the shell reads and to the plain
/// [`MetaField`] the model validates — the type check itself stays in [`crate::mpris`], where it
/// can be tested without a bus.
fn read_metadata(player: &Props) -> RawMetadata {
    let Some(value) = player.get("Metadata") else {
        return RawMetadata::default();
    };
    let value = unwrap_variant(value);
    let zvariant::Value::Dict(dict) = value else {
        // A `Metadata` that is not even a dict: one fault, not three.
        return RawMetadata {
            title: Some(MetaField::Malformed(value.value_signature().to_string())),
            artists: None,
            art_url: None,
        };
    };

    let mut meta = RawMetadata::default();
    for (key, value) in dict.iter() {
        let zvariant::Value::Str(key) = key else {
            continue;
        };
        match key.as_str() {
            "xesam:title" => meta.title = Some(field_of(value)),
            "xesam:artist" => meta.artists = Some(field_of(value)),
            "mpris:artUrl" => meta.art_url = Some(field_of(value)),
            _ => (),
        }
    }
    meta
}

/// A variant nested inside an `a{sv}`, unwrapped one level the way `deepUnpack` does
/// (`mpris.js:131-132`).
fn unwrap_variant<'v>(value: &'v zvariant::Value<'v>) -> &'v zvariant::Value<'v> {
    match value {
        zvariant::Value::Value(inner) => inner.as_ref(),
        other => other,
    }
}

/// One metadata value, as a variant is nested inside `a{sv}` — unwrapped one level, the way
/// `deepUnpack` does (`mpris.js:131-132`).
fn field_of(value: &zvariant::Value<'_>) -> MetaField {
    let value = unwrap_variant(value);

    match value {
        zvariant::Value::Str(s) => MetaField::Str(s.to_string()),
        zvariant::Value::Array(array) => {
            let mut strings = Vec::with_capacity(array.len());
            for item in array.iter() {
                let zvariant::Value::Str(s) = unwrap_variant(item) else {
                    return MetaField::Malformed(value.value_signature().to_string());
                };
                strings.push(s.to_string());
            }
            MetaField::Strings(strings)
        }
        other => MetaField::Malformed(other.value_signature().to_string()),
    }
}

fn get_bool(props: &Props, key: &str) -> bool {
    props
        .get(key)
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| bool::try_from(v).ok())
        .unwrap_or(false)
}

fn get_string(props: &Props, key: &str) -> String {
    props
        .get(key)
        .and_then(|v| <&str>::try_from(&**v).ok())
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn owned(value: zvariant::Value<'_>) -> zvariant::OwnedValue {
        value.try_to_owned().unwrap()
    }

    fn props(pairs: Vec<(&str, zvariant::Value<'_>)>) -> Props {
        pairs
            .into_iter()
            .map(|(k, v)| (k.to_owned(), owned(v)))
            .collect()
    }

    fn metadata(pairs: Vec<(&str, zvariant::Value<'static>)>) -> zvariant::Value<'static> {
        let dict: HashMap<String, zvariant::Value<'static>> =
            pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect();
        zvariant::Value::from(dict)
    }

    /// The wire side of `_updateState` (`mpris.js:129-186`): both interfaces read into one state,
    /// with `Metadata`'s variants unwrapped the way `deepUnpack` unwraps them.
    #[test]
    fn read_player_maps_both_interfaces() {
        let root = props(vec![
            ("Identity", zvariant::Value::from("Rhythmbox")),
            (
                "DesktopEntry",
                zvariant::Value::from("org.gnome.Rhythmbox3"),
            ),
            ("CanRaise", zvariant::Value::from(true)),
        ]);
        let player = props(vec![
            ("PlaybackStatus", zvariant::Value::from("Playing")),
            ("CanPlay", zvariant::Value::from(true)),
            ("CanGoNext", zvariant::Value::from(true)),
            ("CanGoPrevious", zvariant::Value::from(false)),
            (
                "Metadata",
                metadata(vec![
                    ("xesam:title", zvariant::Value::from("So What")),
                    (
                        "xesam:artist",
                        zvariant::Value::from(vec!["Miles Davis".to_owned()]),
                    ),
                    ("mpris:artUrl", zvariant::Value::from("file:///tmp/a.png")),
                    // Keys the shell does not read are ignored, not faults.
                    ("mpris:length", zvariant::Value::from(545_000_000u64)),
                ]),
            ),
        ]);

        let state = read_player(&root, &player, "org.mpris.MediaPlayer2.rhythmbox");
        assert_eq!(state.identity, "Rhythmbox");
        assert_eq!(state.desktop_entry.as_deref(), Some("org.gnome.Rhythmbox3"));
        assert!(state.can_raise && state.can_play && state.can_go_next);
        assert!(!state.can_go_previous);
        assert_eq!(state.status, PlaybackStatus::Playing);
        assert_eq!(state.title, "So What");
        assert_eq!(state.artists, ["Miles Davis"]);
        assert_eq!(
            state.art,
            Some(crate::image_source::ImageSource::File(
                std::path::PathBuf::from("/tmp/a.png")
            ))
        );

        // A player that answers with nothing at all is still readable -- every property is
        // optional on the wire, and `CanPlay` defaulting to false is what keeps it off screen.
        let empty = read_player(&Props::default(), &Props::default(), "bus");
        assert_eq!(empty, PlayerState::default());
        assert!(!empty.can_play);
    }

    /// `DesktopEntry` is a lookup key, not text: it is dropped rather than sanitized, so nothing
    /// downstream can be talked into resolving a path.
    #[test]
    fn a_desktop_entry_that_is_not_an_app_id_is_dropped() {
        let entry = |value: &str| {
            desktop_entry(&props(vec![("DesktopEntry", zvariant::Value::from(value))]))
        };
        assert_eq!(
            entry("org.gnome.Rhythmbox3").as_deref(),
            Some("org.gnome.Rhythmbox3")
        );
        assert_eq!(entry("../../etc/passwd"), None);
        assert_eq!(entry("has space"), None);
        assert_eq!(entry(""), None);
        assert_eq!(entry(&"a".repeat(300)), None);
        assert_eq!(desktop_entry(&Props::default()), None);
    }

    /// A `Metadata` of the wrong shape is one fault, not a panic and not three.
    #[test]
    fn metadata_that_is_not_a_dict_is_one_fault() {
        let player = props(vec![("Metadata", zvariant::Value::from("not a dict"))]);
        let raw = read_metadata(&player);
        assert!(matches!(raw.title, Some(MetaField::Malformed(_))));
        assert!(raw.artists.is_none() && raw.art_url.is_none());

        // An artist array with a non-string in it is malformed as a whole, as GNOME treats it.
        let mixed = metadata(vec![(
            "xesam:artist",
            zvariant::Value::from(vec![
                zvariant::Value::from("a"),
                zvariant::Value::from(1u32),
            ]),
        )]);
        let raw = read_metadata(&props(vec![("Metadata", mixed)]));
        assert!(matches!(raw.artists, Some(MetaField::Malformed(_))));
    }
}
