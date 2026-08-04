// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The StatusNotifierItem registry — the model behind app-indicator ("tray") icons.
//!
//! GNOME Shell has **no** StatusNotifier support of any kind, so unlike the rest of the port there
//! is no `js/ui/` file behind this. The reference is the
//! `gnome-shell-extension-appindicator` extension (v64), and the whole feature is a deliberate
//! divergence — see `docs/fork/status-notifier-port.md` for why we carry it and what the slices
//! are. Citations below name that extension's files.
//!
//! This module is the compositor-side half: which items exist, how they are keyed, and what the
//! observable `RegisteredStatusNotifierItems` list says. The bus half — owning
//! `org.kde.StatusNotifierWatcher`, resolving names, watching owners — is in
//! [`crate::dbus::status_notifier`].
//!
//! **Untrusted-content seam.** Every string here was chosen by a client: the registration
//! argument, and later the item's title, icon names and menu labels. Everything crossing into the
//! model is plain, validated, bounded data, so the bus side can be lifted into its own process
//! later.

/// The object path an item is assumed to live at when a client registers by bus name — the KDE
/// convention (`statusNotifierWatcher.js:41`).
pub const DEFAULT_ITEM_OBJECT_PATH: &str = "/StatusNotifierItem";

/// A bus name is at most this long (the D-Bus spec's limit). Anything longer is not one, and is
/// refused before it reaches a `GetNameOwner` round trip.
const MAX_BUS_NAME_LEN: usize = 255;

/// What a client's `RegisterStatusNotifierItem` argument turned out to mean.
///
/// The spec says "service", and the ecosystem reads that two ways: Ayatana-patched apps send an
/// **object path** and mean "my own bus name, this path", while KDE apps send a **bus name** and
/// mean the well-known `/StatusNotifierItem` (`statusNotifierWatcher.js:207-235`). A watcher that
/// understands only one form silently drops half the clients, so the dispatch is on the leading
/// `/` and nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceRef {
    /// The argument was an object path. The item is on the *sender's* connection, so no name
    /// resolution is needed — and no name resolution is wanted: trusting a path-sending client to
    /// also name its own connection would let it register on someone else's behalf.
    Path {
        unique_name: String,
        object_path: String,
    },
    /// The argument was a bus name, which still has to be resolved to a unique name before the
    /// item can be tracked. May itself already be unique (`:1.42`).
    Name { service: String },
}

/// Why a registration argument was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// Neither a plausible object path nor a plausible bus name.
    NotABusNameOrPath,
    /// An object path was sent, but the message had no sender to attribute it to. Nothing can be
    /// registered for a peer we cannot name.
    NoSender,
}

/// Classify a `RegisterStatusNotifierItem` argument.
///
/// `sender` is the message's unique name, which is what the argument means when it is a path.
pub fn parse_service_argument(arg: &str, sender: Option<&str>) -> Result<ServiceRef, ParseError> {
    if arg.starts_with('/') {
        if !is_valid_object_path(arg) {
            return Err(ParseError::NotABusNameOrPath);
        }
        let unique_name = sender.ok_or(ParseError::NoSender)?;
        return Ok(ServiceRef::Path {
            unique_name: unique_name.to_owned(),
            object_path: arg.to_owned(),
        });
    }

    if is_plausible_bus_name(arg) {
        return Ok(ServiceRef::Name {
            service: arg.to_owned(),
        });
    }

    Err(ParseError::NotABusNameOrPath)
}

/// A cheap structural check on an object path, so a malformed one is refused here rather than
/// deeper in, where it would be a bus error against a client that cannot see it.
fn is_valid_object_path(path: &str) -> bool {
    if path == "/" {
        return true;
    }
    if !path.starts_with('/') || path.ends_with('/') || path.contains("//") {
        return false;
    }
    path[1..]
        .split('/')
        .all(|el| !el.is_empty() && el.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
}

/// Whether a string could be a bus name — well-known (`org.kde.Foo`) or unique (`:1.42`).
///
/// The extension uses a loose regex here (`dbusUtils.js:22`) and so do we: the goal is to reject
/// obvious nonsense before spending a round trip, not to re-implement the bus's own validation.
/// The bus is the authority and will reject what it dislikes.
fn is_plausible_bus_name(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_BUS_NAME_LEN {
        return false;
    }

    // A unique name's elements may start with a digit; a well-known name's may not.
    let unique = name.starts_with(':');
    let body = name.strip_prefix(':').unwrap_or(name);

    // Both forms need at least one dot, so a bare word is not mistaken for a name.
    if !body.contains('.') {
        return false;
    }

    body.split('.').all(|el| {
        !el.is_empty()
            && el
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            && (unique || !el.starts_with(|c: char| c.is_ascii_digit()))
    })
}

/// One registered item, as the model holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredItem {
    /// The item's observable identity, published in `RegisteredStatusNotifierItems` and carried by
    /// the registration signals. See [`item_id`].
    pub id: String,
    /// The connection that owns the item. Identity for lifetime purposes: a well-known name can
    /// move between connections, and an item that moved is a different item.
    pub unique_name: String,
    /// Where the item's interface lives on that connection.
    pub object_path: String,
}

/// The item's public id: the well-known name when the client registered by one, and
/// `<unique-name>@<path>` otherwise (`util.js:33-38`).
///
/// Other hosts and some clients read this list back, so the format is not ours to prettify.
pub fn item_id(service: Option<&str>, unique_name: &str, object_path: &str) -> String {
    match service {
        Some(service) if service != unique_name => service.to_owned(),
        _ => format!("{unique_name}@{object_path}"),
    }
}

/// What happened when an item was offered to the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Registration {
    /// A new item. The caller owes the registration signal and a property change.
    Added,
    /// This exact item is already registered. The client is re-registering — several do it on
    /// their own restart — and the answer is to refresh it in place, never to add a second icon
    /// (`statusNotifierWatcher.js:134-146`).
    AlreadyRegistered,
}

/// The set of items that have registered with our watcher.
///
/// Keyed by `(unique_name, object_path)` rather than by [`RegisteredItem::id`]: one connection may
/// legitimately export several items on different paths, and the well-known name a client
/// registered under says nothing about which connection is serving it now.
#[derive(Debug, Default)]
pub struct ItemRegistry {
    items: Vec<RegisteredItem>,
}

impl ItemRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add an item, or report that it is already there.
    pub fn insert(&mut self, item: RegisteredItem) -> Registration {
        if let Some(existing) = self
            .items
            .iter_mut()
            .find(|i| i.unique_name == item.unique_name && i.object_path == item.object_path)
        {
            // The id can legitimately change on a re-registration: a client that first registered
            // by path and then by well-known name is the same item under a better name.
            existing.id = item.id;
            return Registration::AlreadyRegistered;
        }

        self.items.push(item);
        Registration::Added
    }

    /// Drop every item served by `unique_name`, returning their ids so the caller can emit one
    /// unregistration signal each.
    pub fn remove_owner(&mut self, unique_name: &str) -> Vec<String> {
        let mut removed = Vec::new();
        self.items.retain(|item| {
            if item.unique_name == unique_name {
                removed.push(item.id.clone());
                false
            } else {
                true
            }
        });
        removed
    }

    /// Drop one item by connection and path — what the liveness probe uses when a client keeps its
    /// bus name but drops the object (see the Electron trap in the port doc).
    pub fn remove_item(&mut self, unique_name: &str, object_path: &str) -> Option<String> {
        let idx = self
            .items
            .iter()
            .position(|i| i.unique_name == unique_name && i.object_path == object_path)?;
        Some(self.items.remove(idx).id)
    }

    pub fn contains_owner(&self, unique_name: &str) -> bool {
        self.items.iter().any(|i| i.unique_name == unique_name)
    }

    pub fn items(&self) -> &[RegisteredItem] {
        &self.items
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The `RegisteredStatusNotifierItems` property, in registration order.
    pub fn ids(&self) -> Vec<String> {
        self.items.iter().map(|i| i.id.clone()).collect()
    }
}

/// What the watcher tells the compositor. S1 carries lifetime only; the item's contents
/// (icon, status, menu) arrive in later slices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusNotifierToSynoik {
    ItemRegistered(RegisteredItem),
    ItemUnregistered { id: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_argument_is_attributed_to_the_sender() {
        // Ayatana-patched apps send a path and mean "my connection, this path".
        assert_eq!(
            parse_service_argument("/org/ayatana/NotificationItem/foo", Some(":1.42")),
            Ok(ServiceRef::Path {
                unique_name: ":1.42".to_owned(),
                object_path: "/org/ayatana/NotificationItem/foo".to_owned(),
            })
        );
    }

    #[test]
    fn a_bus_name_argument_is_left_for_resolution() {
        // KDE apps send a bus name and mean the default path.
        assert_eq!(
            parse_service_argument("org.kde.StatusNotifierItem-1234-1", None),
            Ok(ServiceRef::Name {
                service: "org.kde.StatusNotifierItem-1234-1".to_owned(),
            })
        );
        // A unique name is a bus name too, and clients do send them.
        assert_eq!(
            parse_service_argument(":1.42", Some(":1.42")),
            Ok(ServiceRef::Name {
                service: ":1.42".to_owned(),
            })
        );
    }

    #[test]
    fn a_path_with_no_sender_is_refused() {
        // Nothing can be registered for a peer we cannot name.
        assert_eq!(
            parse_service_argument("/StatusNotifierItem", None),
            Err(ParseError::NoSender)
        );
    }

    #[test]
    fn nonsense_arguments_are_refused() {
        for arg in ["", "not a name", "no-dots", "//bad//path", "/trailing/"] {
            assert_eq!(
                parse_service_argument(arg, Some(":1.7")),
                Err(ParseError::NotABusNameOrPath),
                "{arg:?} should not parse"
            );
        }
    }

    #[test]
    fn the_id_prefers_the_well_known_name() {
        // A client that registered by well-known name is published under it...
        assert_eq!(
            item_id(
                Some("org.kde.StatusNotifierItem-9-1"),
                ":1.9",
                "/StatusNotifierItem"
            ),
            "org.kde.StatusNotifierItem-9-1"
        );
        // ...but one that registered by path, or by its own unique name, is not: `service ==
        // busName` there, and publishing a bare `:1.9` would collide with every other item on
        // that connection.
        assert_eq!(
            item_id(None, ":1.9", "/org/ayatana/NotificationItem/x"),
            ":1.9@/org/ayatana/NotificationItem/x"
        );
        assert_eq!(
            item_id(Some(":1.9"), ":1.9", "/StatusNotifierItem"),
            ":1.9@/StatusNotifierItem"
        );
    }

    fn item(id: &str, unique_name: &str, object_path: &str) -> RegisteredItem {
        RegisteredItem {
            id: id.to_owned(),
            unique_name: unique_name.to_owned(),
            object_path: object_path.to_owned(),
        }
    }

    #[test]
    fn re_registering_the_same_item_does_not_add_a_second_icon() {
        let mut registry = ItemRegistry::new();
        let it = item("org.kde.Foo", ":1.5", "/StatusNotifierItem");

        assert_eq!(registry.insert(it.clone()), Registration::Added);
        assert_eq!(registry.insert(it), Registration::AlreadyRegistered);
        assert_eq!(registry.ids(), vec!["org.kde.Foo".to_owned()]);
    }

    #[test]
    fn one_connection_may_serve_several_items() {
        let mut registry = ItemRegistry::new();
        registry.insert(item("a", ":1.5", "/one"));
        registry.insert(item("b", ":1.5", "/two"));

        assert_eq!(registry.ids(), vec!["a".to_owned(), "b".to_owned()]);

        // And losing the connection takes both, in one go.
        assert_eq!(
            registry.remove_owner(":1.5"),
            vec!["a".to_owned(), "b".to_owned()]
        );
        assert!(registry.is_empty());
    }

    #[test]
    fn the_same_name_on_a_new_connection_is_a_new_item() {
        // A well-known name that moved connections is a restarted app, not the old one: keying on
        // the id would leave the dead item in the list and drop the live one as a duplicate.
        let mut registry = ItemRegistry::new();
        registry.insert(item("org.kde.Foo", ":1.5", "/StatusNotifierItem"));
        assert_eq!(
            registry.insert(item("org.kde.Foo", ":1.9", "/StatusNotifierItem")),
            Registration::Added
        );

        assert_eq!(
            registry.remove_owner(":1.5"),
            vec!["org.kde.Foo".to_owned()]
        );
        assert!(registry.contains_owner(":1.9"));
    }

    #[test]
    fn removing_one_item_leaves_its_siblings() {
        let mut registry = ItemRegistry::new();
        registry.insert(item("a", ":1.5", "/one"));
        registry.insert(item("b", ":1.5", "/two"));

        assert_eq!(registry.remove_item(":1.5", "/one"), Some("a".to_owned()));
        assert_eq!(registry.ids(), vec!["b".to_owned()]);
        assert_eq!(registry.remove_item(":1.5", "/one"), None);
    }

    #[test]
    fn a_re_registration_may_improve_the_id() {
        // Registering by path first and by well-known name second is the same item under a better
        // name — the list should follow, without gaining an entry.
        let mut registry = ItemRegistry::new();
        registry.insert(item(
            ":1.5@/StatusNotifierItem",
            ":1.5",
            "/StatusNotifierItem",
        ));
        assert_eq!(
            registry.insert(item("org.kde.Foo", ":1.5", "/StatusNotifierItem")),
            Registration::AlreadyRegistered
        );
        assert_eq!(registry.ids(), vec!["org.kde.Foo".to_owned()]);
    }
}
