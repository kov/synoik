// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! `com.canonical.dbusmenu` client — an app indicator's menu, fetched from the client that owns it.
//!
//! Reference: the `gnome-shell-extension-appindicator` extension's `dbusMenu.js` (v64). GNOME
//! Shell has none of this. The model, and what a layout *means*, is [`crate::dbusmenu`]; the plan
//! is `docs/fork/status-notifier-port.md`.
//!
//! The whole interface is one method with a recursive reply — `GetLayout(parent, depth, props)`
//! returns `(revision, (ia{sv}av))`, each child being another of the same — plus `Event` to tell
//! the client what the user did, `AboutToShow` to let it fill a submenu in first, and two signals
//! that mean "read it again".

use std::collections::HashMap;

use futures_util::{FutureExt as _, StreamExt};
use zbus::zvariant::{OwnedValue, Value};

use crate::dbusmenu::{MenuNode, NodeKind, MAX_CHILDREN, MAX_DEPTH};
use crate::status_notifier::{StatusNotifierToSynoik, SynoikToStatusNotifier};
use crate::ui::widget::Ornament;

pub const IFACE: &str = "com.canonical.dbusmenu";

/// `GetLayout`'s depth argument: -1 is "everything below this node".
///
/// The extension fetches the whole tree in one call too. A lazy per-level fetch would save a
/// round trip on a menu nobody opens, but every level then costs one *while* the menu is open,
/// which is where the user is watching.
const FULL_DEPTH: i32 = -1;

/// The properties we ask for. Asking for the ones we use rather than all of them keeps a client
/// from sending us its icon *data* for every row of a menu we may never show.
const WANTED_PROPS: &[&str] = &[
    "type",
    "label",
    "enabled",
    "visible",
    "icon-name",
    "toggle-type",
    "toggle-state",
    "children-display",
];

/// One node as it comes off the wire, before validation.
type RawNode = (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);

/// Fetch a client's whole menu.
pub async fn fetch_layout(
    conn: &zbus::Connection,
    dest: &str,
    path: &str,
) -> zbus::Result<(u32, MenuNode)> {
    let reply = conn
        .call_method(
            Some(dest),
            path,
            Some(IFACE),
            "GetLayout",
            &(0i32, FULL_DEPTH, WANTED_PROPS.to_vec()),
        )
        .await?;
    let (revision, root): (u32, RawNode) = reply.body().deserialize()?;
    Ok((revision, node_from_raw(&root, 0)))
}

/// Convert one wire node, recursively, refusing to follow a client past our depth and breadth
/// bounds — the reply is a tree a client wrote, and nothing else limits it.
fn node_from_raw(raw: &RawNode, depth: usize) -> MenuNode {
    let (id, props, children) = raw;
    let mut node = MenuNode::new(*id);

    let string =
        |key: &str| -> Option<&str> { props.get(key).and_then(|v| <&str>::try_from(v).ok()) };

    if let Some(label) = string("label") {
        node.label = label.to_owned();
    }
    if let Some(kind) = string("type") {
        if kind == "separator" {
            node.kind = NodeKind::Separator;
        }
    }
    // Both default to *true* when absent (`dbusMenu.js:86-91`), which is the opposite of what a
    // missing boolean usually means.
    node.enabled = props
        .get("enabled")
        .and_then(|v| bool::try_from(v).ok())
        .unwrap_or(true);
    node.visible = props
        .get("visible")
        .and_then(|v| bool::try_from(v).ok())
        .unwrap_or(true);
    node.icon_name = string("icon-name")
        .filter(|n| !n.is_empty())
        .and_then(crate::status_notifier::normalize_icon_name);
    node.has_submenu = string("children-display") == Some("submenu");

    // A mark is shown only when the state is non-zero, and only for a known toggle type
    // (`dbusMenu.js:745-750`).
    let state = props
        .get("toggle-state")
        .and_then(|v| i32::try_from(v).ok())
        .unwrap_or(0);
    node.ornament = match string("toggle-type") {
        Some("checkmark") => Ornament::Check(state > 0),
        Some("radio") => Ornament::Radio(state > 0),
        _ => Ornament::None,
    };

    if depth < MAX_DEPTH {
        node.children = children
            .iter()
            .take(MAX_CHILDREN)
            .filter_map(|child| {
                // Each child arrives boxed in a variant.
                let raw = RawNode::try_from(Value::from(child.clone())).ok()?;
                Some(node_from_raw(&raw, depth + 1))
            })
            .collect();
    }

    node
}

/// Tell the client the user did something to a node.
///
/// Fire-and-forget: a wedged client must not stall the compositor, and there is nothing useful to
/// do with a failure that the user has not already seen.
pub async fn send_event(conn: &zbus::Connection, dest: &str, path: &str, id: i32, event: &str) {
    let timestamp = 0u32;
    let data = Value::from(0i32);
    if let Err(err) = conn
        .call_method(
            Some(dest),
            path,
            Some(IFACE),
            "Event",
            &(id, event, &data, timestamp),
        )
        .await
    {
        warn!("dbusmenu: {event} on {dest}{path} node {id} failed: {err:?}");
    }
}

/// Ask the client to fill a node in before it is shown, and report whether the layout must be
/// re-fetched first.
///
/// **Two traps in one call.** Dropbox answers with `()` where the spec says `(b)`
/// (`dbusMenu.js:511-522`), so an empty reply is taken as "yes, re-read" rather than an error. And
/// a client that does not implement the method at all answers `UnknownMethod`, which is not a
/// failure — it just has nothing to prepare.
pub async fn about_to_show(conn: &zbus::Connection, dest: &str, path: &str, id: i32) -> bool {
    let reply = match conn
        .call_method(Some(dest), path, Some(IFACE), "AboutToShow", &(id,))
        .await
    {
        Ok(reply) => reply,
        Err(err) => {
            debug!("dbusmenu: AboutToShow({id}) on {dest}{path}: {err:?}");
            return false;
        }
    };

    // The `Err` arm is the untyped reply: the client answered, so treat it as "something may have
    // changed" rather than as a failure.
    reply.body().deserialize::<bool>().unwrap_or(true)
}

/// Follow one open menu: fetch it, keep it current, and carry the user's events back.
///
/// Lives for as long as the menu is open. The signals only matter while it is on screen — a client
/// repainting a menu nobody is looking at is not worth a round trip — so this task starts on open
/// and ends on close.
pub fn watch_menu(
    conn: &zbus::Connection,
    item_id: String,
    dest: String,
    path: String,
    to_niri: calloop::channel::Sender<StatusNotifierToSynoik>,
    requests: async_channel::Receiver<SynoikToStatusNotifier>,
) {
    let task_conn = conn.clone();

    let task = async move {
        let conn = task_conn;

        // Subscribe before the first fetch, so a change during it is not missed.
        let mut updates = match receive_updates(&conn, &dest, &path).await {
            Ok(stream) => stream,
            Err(err) => {
                warn!("dbusmenu: no signal stream for {dest}{path}: {err:?}");
                return;
            }
        };

        // The root's `AboutToShow` first: Dropbox needs it before the layout is valid
        // (`dbusMenu.js:893-894`), and for everyone else it is the client's chance to populate.
        about_to_show(&conn, &dest, &path, 0).await;
        send_event(&conn, &dest, &path, 0, "opened").await;

        let push = |node: MenuNode| {
            let _ = to_niri.send(StatusNotifierToSynoik::MenuLayout {
                item_id: item_id.clone(),
                root: Box::new(node),
            });
        };

        match fetch_layout(&conn, &dest, &path).await {
            Ok((_, root)) => push(root),
            Err(err) => {
                warn!("dbusmenu: GetLayout on {dest}{path} failed: {err:?}");
                return;
            }
        }

        loop {
            futures_util::select! {
                signal = updates.next() => {
                    if signal.is_none() {
                        break;
                    }
                    // Either signal means the same thing to us: read it again. Re-fetching the
                    // whole tree on a property change is more than the signal asks for, but a
                    // menu is small and the alternative is a second merge path to get wrong.
                    match fetch_layout(&conn, &dest, &path).await {
                        Ok((_, root)) => push(root),
                        Err(err) => {
                            warn!("dbusmenu: re-reading {dest}{path} failed: {err:?}");
                            break;
                        }
                    }
                }
                request = requests.recv().fuse() => {
                    let Ok(request) = request else {
                        break;
                    };
                    match request {
                        SynoikToStatusNotifier::MenuActivate(id) => {
                            send_event(&conn, &dest, &path, id, "clicked").await;
                        }
                        SynoikToStatusNotifier::MenuOpenSubmenu(id) => {
                            // The client may fill the submenu in only when asked.
                            if about_to_show(&conn, &dest, &path, id).await {
                                if let Ok((_, root)) = fetch_layout(&conn, &dest, &path).await {
                                    push(root);
                                }
                            }
                        }
                        // `OpenMenu` is handled by the dispatcher, which closes this task before
                        // starting the next menu's; either way this one is done.
                        SynoikToStatusNotifier::CloseMenu
                        | SynoikToStatusNotifier::OpenMenu { .. } => break,
                    }
                }
            }
        }

        send_event(&conn, &dest, &path, 0, "closed").await;
    };

    conn.executor().spawn(task, "watch a dbusmenu").detach();
}

/// A merged stream of the two signals that mean "the menu changed".
async fn receive_updates(
    conn: &zbus::Connection,
    dest: &str,
    path: &str,
) -> zbus::Result<futures_util::stream::SelectAll<zbus::MessageStream>> {
    let mut merged = futures_util::stream::SelectAll::new();
    for member in ["LayoutUpdated", "ItemsPropertiesUpdated"] {
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender(dest)?
            .path(path)?
            .interface(IFACE)?
            .member(member)?
            .build();
        merged.push(zbus::MessageStream::for_match_rule(rule, conn, None).await?);
    }
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn props(pairs: &[(&str, Value<'static>)]) -> HashMap<String, OwnedValue> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), OwnedValue::try_from(v.clone()).unwrap()))
            .collect()
    }

    fn raw(id: i32, pairs: &[(&str, Value<'static>)], children: Vec<RawNode>) -> RawNode {
        (
            id,
            props(pairs),
            children
                .into_iter()
                .map(|c| OwnedValue::try_from(Value::from(c)).unwrap())
                .collect(),
        )
    }

    #[test]
    fn an_absent_property_takes_the_specs_default_not_rusts() {
        // A client that sends only a label gets an enabled, visible, standard row — the opposite
        // of what `Default` would give (`dbusMenu.js:86-91`).
        let node = node_from_raw(&raw(7, &[("label", Value::from("Quit"))], vec![]), 0);
        assert_eq!(node.id, 7);
        assert_eq!(node.label, "Quit");
        assert!(node.enabled);
        assert!(node.visible);
        assert_eq!(node.kind, NodeKind::Standard);
        assert_eq!(node.ornament, Ornament::None);
    }

    #[test]
    fn a_toggle_shows_a_mark_only_when_it_is_on() {
        let checked = node_from_raw(
            &raw(
                1,
                &[
                    ("toggle-type", Value::from("checkmark")),
                    ("toggle-state", Value::from(1i32)),
                ],
                vec![],
            ),
            0,
        );
        assert_eq!(checked.ornament, Ornament::Check(true));

        // `toggle-state` is *tri*-state: -1 means "indeterminate", which is not a tick
        // (`dbusMenu.js:745-750`).
        let indeterminate = node_from_raw(
            &raw(
                1,
                &[
                    ("toggle-type", Value::from("radio")),
                    ("toggle-state", Value::from(-1i32)),
                ],
                vec![],
            ),
            0,
        );
        assert_eq!(indeterminate.ornament, Ornament::Radio(false));

        // A state with no toggle type is not a toggle at all.
        let neither = node_from_raw(&raw(1, &[("toggle-state", Value::from(1i32))], vec![]), 0);
        assert_eq!(neither.ornament, Ornament::None);
    }

    #[test]
    fn a_client_cannot_nest_us_past_the_depth_bound() {
        // Build a chain one deeper than we follow. The reply is a tree the client wrote, and
        // `GetLayout(-1)` puts no bound on it.
        let mut node = raw(0, &[], vec![]);
        for depth in 0..=MAX_DEPTH {
            node = raw(depth as i32 + 1, &[], vec![node]);
        }

        let mut converted = node_from_raw(&node, 0);
        let mut levels = 0;
        while let Some(child) = converted.children.into_iter().next() {
            levels += 1;
            converted = child;
        }
        assert_eq!(levels, MAX_DEPTH, "the walk stops at the bound");
    }

    #[test]
    fn an_icon_name_is_normalized_like_an_items_own() {
        // The same client habits show up on menu rows: a path is not a name.
        let themed = node_from_raw(
            &raw(1, &[("icon-name", Value::from("folder.png"))], vec![]),
            0,
        );
        assert_eq!(themed.icon_name.as_deref(), Some("folder"));

        let path = node_from_raw(
            &raw(1, &[("icon-name", Value::from("/tmp/evil.png"))], vec![]),
            0,
        );
        assert_eq!(path.icon_name, None);
    }
}
