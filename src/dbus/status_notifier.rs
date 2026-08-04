// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! `org.kde.StatusNotifierWatcher` — the bus half of app-indicator support.
//!
//! We are the watcher *and* the only host. Clients call `RegisterStatusNotifierItem` and we track
//! each item for as long as the connection serving it is alive. Reference: the
//! `gnome-shell-extension-appindicator` extension's `statusNotifierWatcher.js` (v64); GNOME Shell
//! itself has none of this. The model, and the reasoning behind the id and key choices, is in
//! [`crate::status_notifier`]; the plan is `docs/fork/status-notifier-port.md`.
//!
//! Two things a client can observe that are easy to get wrong, so they are stated here:
//!
//! - **Owning the name is the feature.** A large class of clients (Electron, Qt, Ayatana) checks
//!   whether `org.kde.StatusNotifierWatcher` has an owner and hides its tray affordance entirely
//!   when it does not. Failing to acquire the name must therefore fail the whole start, not leave a
//!   watcher that answers calls from a connection nobody looks up.
//! - **A vanished owner is not immediately a dead item.** Apps that restart re-register within
//!   milliseconds, and dropping the item on the first `NameOwnerChanged` makes the panel flicker;
//!   the extension waits and re-checks (`statusNotifierWatcher.js:104-116`).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use zbus::fdo::{RequestNameFlags, RequestNameReply};
use zbus::names::UniqueName;
use zbus::object_server::SignalEmitter;
use zbus::{fdo, interface, zvariant};

use super::Start;
use crate::status_notifier::{
    clean_text, item_id, normalize_icon_name, parse_service_argument, ItemCategory, ItemProps,
    ItemRegistry, ItemStatus, ParseError, RegisteredItem, Registration, ServiceRef,
    StatusNotifierToSynoik, DEFAULT_ITEM_OBJECT_PATH,
};

pub const BUS_NAME: &str = "org.kde.StatusNotifierWatcher";
pub const OBJECT_PATH: &str = "/StatusNotifierWatcher";

/// The interface each client exports for its item.
const ITEM_IFACE: &str = "org.kde.StatusNotifierItem";

/// The sentinel a client uses to say it has no menu (`appIndicator.js:577`).
const NO_DBUSMENU: &str = "/NO_DBUSMENU";

/// The item interface announces changes with these bare signals instead of `PropertiesChanged`;
/// each means "read me again" (`appIndicator.js:178-183`).
const NEW_SIGNALS: &[&str] = &[
    "NewIcon",
    "NewAttentionIcon",
    "NewOverlayIcon",
    "NewStatus",
    "NewTitle",
    "NewMenu",
    "NewIconThemePath",
];

/// How many times to re-read an item's properties while it has no `Id` yet, and how long to wait
/// between tries (`appIndicator.js:501-503`).
const READY_RETRIES: usize = 3;
const READY_RETRY_DELAY: Duration = Duration::from_secs(1);

/// How long a registered item survives its connection's disappearance before we believe it.
/// The extension uses 500 ms (`statusNotifierWatcher.js:107`).
const OWNER_GRACE: Duration = Duration::from_millis(500);

/// The protocol version the spec pins, and the one the extension reports
/// (`statusNotifierWatcher.js:281-283`).
const PROTOCOL_VERSION: i32 = 0;

pub struct StatusNotifierWatcher {
    /// Shared with the owner-watching task, which is the only other writer.
    registry: Arc<Mutex<ItemRegistry>>,
    to_niri: calloop::channel::Sender<StatusNotifierToSynoik>,
}

impl StatusNotifierWatcher {
    pub fn new(to_niri: calloop::channel::Sender<StatusNotifierToSynoik>) -> Self {
        Self {
            registry: Arc::new(Mutex::new(ItemRegistry::new())),
            to_niri,
        }
    }
}

#[interface(name = "org.kde.StatusNotifierWatcher")]
impl StatusNotifierWatcher {
    /// Register an item. The argument is either an object path or a bus name — see
    /// [`parse_service_argument`] for why both must be understood.
    async fn register_status_notifier_item(
        &mut self,
        #[zbus(connection)] conn: &zbus::Connection,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        service: &str,
    ) -> fdo::Result<()> {
        let sender = header.sender().map(UniqueName::as_str);

        let (well_known, unique_name, object_path) = match parse_service_argument(service, sender)
            .map_err(|err| match err {
            ParseError::NotABusNameOrPath => fdo::Error::InvalidArgs(format!(
                "{service:?} is neither a bus name nor an object path"
            )),
            ParseError::NoSender => {
                fdo::Error::InvalidArgs("cannot register a path for an unknown sender".into())
            }
        })? {
            ServiceRef::Path {
                unique_name,
                object_path,
            } => (None, unique_name, object_path),
            ServiceRef::Name { service } => {
                // A well-known name must become a unique one before it can be tracked: the
                // item's life is the *connection's* life, and a well-known name outlives any
                // particular owner of it.
                let dbus = fdo::DBusProxy::new(conn).await?;
                let name = zbus::names::BusName::try_from(service.as_str())
                    .map_err(|err| fdo::Error::InvalidArgs(format!("{err}")))?;
                let owner = dbus.get_name_owner(name).await?;
                (
                    Some(service),
                    owner.as_str().to_owned(),
                    DEFAULT_ITEM_OBJECT_PATH.to_owned(),
                )
            }
        };

        let item = RegisteredItem {
            id: item_id(well_known.as_deref(), &unique_name, &object_path),
            unique_name,
            object_path,
        };

        let registration = self.registry.lock().unwrap().insert(item.clone());

        if registration == Registration::AlreadyRegistered {
            // Not an error: several clients re-register on their own restart. The item is
            // refreshed in place rather than doubled (`statusNotifierWatcher.js:134-146`).
            debug!("status-notifier: {} re-registered", item.id);
            return Ok(());
        }

        debug!("status-notifier: registered {}", item.id);
        watch_owner(conn, self.registry.clone(), self.to_niri.clone(), &item);
        watch_item(conn, self.registry.clone(), self.to_niri.clone(), &item);

        Self::status_notifier_item_registered(&emitter, &item.id).await?;
        self.registered_status_notifier_items_changed(&emitter)
            .await?;

        Ok(())
    }

    /// Refused, as in the extension (`statusNotifierWatcher.js:262-267`): there is exactly one
    /// host on this session and it is the shell. A second host would render every icon twice.
    fn register_status_notifier_host(&self, _service: &str) -> fdo::Result<()> {
        Err(fdo::Error::NotSupported(
            "registering additional notification hosts is not supported".into(),
        ))
    }

    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        self.registry.lock().unwrap().ids()
    }

    /// Always true: we are the host, and we own this name for the session's lifetime. Clients
    /// read this to decide whether to show a tray icon at all.
    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn protocol_version(&self) -> i32 {
        PROTOCOL_VERSION
    }

    #[zbus(signal)]
    async fn status_notifier_item_registered(
        emitter: &SignalEmitter<'_>,
        service: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_item_unregistered(
        emitter: &SignalEmitter<'_>,
        service: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_host_registered(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn status_notifier_host_unregistered(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

/// Follow one item's connection and retire the item when it goes for good.
///
/// One task per registered item, and that task is the **sole remover** for it: a removal decided
/// anywhere else could race a re-registration and retire the live item instead of the dead one.
fn watch_owner(
    conn: &zbus::Connection,
    registry: Arc<Mutex<ItemRegistry>>,
    to_niri: calloop::channel::Sender<StatusNotifierToSynoik>,
    item: &RegisteredItem,
) {
    let task_conn = conn.clone();
    let unique_name = item.unique_name.clone();
    let object_path = item.object_path.clone();

    let task = async move {
        let conn = task_conn;
        let dbus = match fdo::DBusProxy::new(&conn).await {
            Ok(proxy) => proxy,
            Err(err) => {
                warn!("status-notifier: error creating DBusProxy for {unique_name}: {err:?}");
                return;
            }
        };
        let mut owner_changed = match dbus
            .receive_name_owner_changed_with_args(&[(0, unique_name.as_str())])
            .await
        {
            Ok(stream) => stream,
            Err(err) => {
                warn!("status-notifier: error watching owner of {unique_name}: {err:?}");
                return;
            }
        };

        loop {
            let Some(signal) = owner_changed.next().await else {
                return;
            };
            let Ok(args) = signal.args() else {
                continue;
            };
            if args.new_owner().is_some() {
                // A unique name is never reassigned, so this cannot be our peer coming back — but
                // the stream is filtered on the name, not on the direction, so ignore it rather
                // than treat it as a death.
                continue;
            }

            // The grace window: an app restarting re-registers almost immediately, and a panel
            // that drops the icon in between flickers.
            async_io::Timer::after(OWNER_GRACE).await;
            if let Ok(owner) = dbus
                .get_name_owner(zbus::names::BusName::from(
                    UniqueName::try_from(unique_name.as_str()).unwrap(),
                ))
                .await
            {
                if owner.as_str() == unique_name {
                    continue;
                }
            }

            let removed = {
                let mut registry = registry.lock().unwrap();
                registry.remove_item(&unique_name, &object_path)
            };
            let Some(id) = removed else {
                // Already retired by someone else — nothing left to announce.
                return;
            };

            debug!("status-notifier: {id} is gone");
            let _ = to_niri.send(StatusNotifierToSynoik::ItemUnregistered { id: id.clone() });

            if let Err(err) = announce_unregistered(&conn, &id).await {
                warn!("status-notifier: error announcing {id} unregistered: {err:?}");
            }
            return;
        }
    };

    conn.executor()
        .spawn(task, "watch a StatusNotifierItem's owner")
        .detach();
}

/// Read one item's properties and keep them current for as long as it lives.
///
/// Unlike almost everything else on the bus, `org.kde.StatusNotifierItem` does **not** announce
/// changes through `PropertiesChanged`: it emits bare `NewIcon`, `NewStatus`, `NewTitle`,
/// `NewAttentionIcon`, `NewOverlayIcon`, `NewMenu` signals carrying nothing, so every one of them
/// means "read me again" (`appIndicator.js:178-183`). Re-reading the whole set is what the
/// extension's proxy does too, and it keeps this task the item's single writer.
fn watch_item(
    conn: &zbus::Connection,
    registry: Arc<Mutex<ItemRegistry>>,
    to_niri: calloop::channel::Sender<StatusNotifierToSynoik>,
    item: &RegisteredItem,
) {
    let task_conn = conn.clone();
    let item = item.clone();

    let task = async move {
        let conn = task_conn;
        let props = match fdo::PropertiesProxy::new(
            &conn,
            item.unique_name.clone(),
            item.object_path.clone(),
        )
        .await
        {
            Ok(proxy) => proxy,
            Err(err) => {
                warn!(
                    "status-notifier: no properties proxy for {}: {err:?}",
                    item.id
                );
                return;
            }
        };

        // Subscribe before the first read, so a change during it is not lost.
        let mut changed = match receive_new_signals(&conn, &item).await {
            Ok(stream) => stream,
            Err(err) => {
                warn!("status-notifier: no signal stream for {}: {err:?}", item.id);
                return;
            }
        };

        // What the item can be *asked* to do is fixed for its lifetime, so it is probed once.
        let capabilities = introspect_capabilities(&conn, &item).await;

        // Properties, with the readiness retry: clients routinely register before they have
        // finished exporting themselves, and an item with no `Id` yet is not a broken item
        // (`appIndicator.js:498-529`).
        let mut current = None;
        for attempt in 0..READY_RETRIES {
            match read_props(&props, capabilities).await {
                Ok(p) if p.is_ready() => {
                    current = Some(p);
                    break;
                }
                Ok(p) => {
                    current = Some(p);
                    async_io::Timer::after(READY_RETRY_DELAY).await;
                }
                Err(err) => {
                    if is_gone(&err) {
                        retire(&conn, &registry, &to_niri, &item).await;
                        return;
                    }
                    if attempt + 1 == READY_RETRIES {
                        warn!(
                            "status-notifier: giving up reading {}'s properties: {err:?}",
                            item.id
                        );
                        return;
                    }
                    async_io::Timer::after(READY_RETRY_DELAY).await;
                }
            }
        }

        if let Some(props) = current {
            debug!(
                "status-notifier: {} ready={} status={:?} icon={:?} menu={:?} activate={}",
                item.id,
                props.is_ready(),
                props.status,
                props.effective_icon(),
                props.menu_path,
                props.supports_activation,
            );
            let _ = to_niri.send(StatusNotifierToSynoik::ItemUpdated {
                item: item.clone(),
                props: Box::new(props),
            });
        }

        while changed.next().await.is_some() {
            match read_props(&props, capabilities).await {
                Ok(props) => {
                    debug!(
                        "status-notifier: {} changed, icon={:?} status={:?}",
                        item.id,
                        props.effective_icon(),
                        props.status
                    );
                    let _ = to_niri.send(StatusNotifierToSynoik::ItemUpdated {
                        item: item.clone(),
                        props: Box::new(props),
                    });
                }
                Err(err) if is_gone(&err) => {
                    // The Electron case: the client dropped the object but kept its bus name, so
                    // the owner watcher will never fire and the icon would sit there forever
                    // (`appIndicator.js:623-666`).
                    retire(&conn, &registry, &to_niri, &item).await;
                    return;
                }
                Err(err) => {
                    warn!("status-notifier: error re-reading {}: {err:?}", item.id);
                }
            }
        }
    };

    conn.executor()
        .spawn(task, "watch a StatusNotifierItem")
        .detach();
}

/// A stream of every "read me again" signal the item interface defines, merged.
async fn receive_new_signals(
    conn: &zbus::Connection,
    item: &RegisteredItem,
) -> zbus::Result<futures_util::stream::SelectAll<zbus::MessageStream>> {
    let mut merged = futures_util::stream::SelectAll::new();
    for member in NEW_SIGNALS {
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender(item.unique_name.as_str())?
            .path(item.object_path.as_str())?
            .interface(ITEM_IFACE)?
            .member(*member)?
            .build();
        merged.push(zbus::MessageStream::for_match_rule(rule, conn, None).await?);
    }
    Ok(merged)
}

/// Ask the item's own introspection XML which optional methods it has.
///
/// Clients omit methods they do not implement, and calling a missing one is an error the user
/// would see as a click that did nothing. The answer is fixed for the item's life, so this is
/// asked once (`appIndicator.js:446-457`).
async fn introspect_capabilities(conn: &zbus::Connection, item: &RegisteredItem) -> Capabilities {
    let proxy = match zbus::fdo::IntrospectableProxy::builder(conn)
        .destination(item.unique_name.as_str())
        .and_then(|b| b.path(item.object_path.as_str()))
    {
        Ok(builder) => match builder.build().await {
            Ok(proxy) => proxy,
            Err(err) => {
                debug!("status-notifier: cannot introspect {}: {err:?}", item.id);
                return Capabilities::default();
            }
        },
        Err(err) => {
            debug!("status-notifier: cannot introspect {}: {err:?}", item.id);
            return Capabilities::default();
        }
    };

    let Ok(xml) = proxy.introspect().await else {
        // Introspection is optional in practice. Assume the spec's methods exist rather than
        // disabling activation for an item that may well support it.
        return Capabilities::default();
    };

    // A structural parse is more than this needs: the question is only whether a method name
    // appears inside this interface's element.
    let Some(iface) = xml.split(&format!("\"{ITEM_IFACE}\"")).nth(1) else {
        return Capabilities::default();
    };
    let iface = iface.split("</interface>").next().unwrap_or(iface);
    Capabilities {
        supports_activation: iface.contains("\"Activate\""),
        has_ayatana_secondary_activate: iface.contains("\"XAyatanaSecondaryActivate\""),
    }
}

/// Read every property we care about in one round trip and validate it into the model's shape.
async fn read_props(
    props: &fdo::PropertiesProxy<'_>,
    capabilities: Capabilities,
) -> zbus::Result<ItemProps> {
    let iface = zbus::names::InterfaceName::try_from(ITEM_IFACE)?;
    let all = props.get_all(iface).await?;

    let string = |key: &str| -> Option<String> {
        all.get(key)
            .and_then(|v| <&str>::try_from(v).ok())
            .map(str::to_owned)
    };

    // A client that exports the object but not this property is not ready yet, not broken.
    let menu_path = all
        .get("Menu")
        .and_then(|v| zvariant::ObjectPath::try_from(v.clone()).ok())
        .map(|p| p.as_str().to_owned())
        // `/NO_DBUSMENU` is how a client says "no menu" without omitting the property
        // (`appIndicator.js:576-580`).
        .filter(|p| p != NO_DBUSMENU);

    Ok(ItemProps {
        app_id: clean_text(&string("Id").unwrap_or_default()),
        title: clean_text(&string("Title").unwrap_or_default()),
        status: string("Status")
            .as_deref()
            .map(ItemStatus::parse)
            .unwrap_or_default(),
        category: string("Category")
            .as_deref()
            .map(ItemCategory::parse)
            .unwrap_or_default(),
        icon_name: string("IconName").as_deref().and_then(normalize_icon_name),
        attention_icon_name: string("AttentionIconName")
            .as_deref()
            .and_then(normalize_icon_name),
        icon_theme_path: string("IconThemePath").filter(|p| !p.is_empty()),
        menu_path,
        item_is_menu: all
            .get("ItemIsMenu")
            .and_then(|v| bool::try_from(v).ok())
            .unwrap_or(false),
        supports_activation: capabilities.supports_activation,
        has_ayatana_secondary_activate: capabilities.has_ayatana_secondary_activate,
    })
}

/// The optional methods an item declares.
#[derive(Debug, Clone, Copy)]
struct Capabilities {
    supports_activation: bool,
    has_ayatana_secondary_activate: bool,
}

impl Default for Capabilities {
    fn default() -> Self {
        // Assume the spec's methods are there. Deciding an item cannot be activated on the
        // strength of a failed introspection would silently downgrade every client behind a
        // confinement that blocks it.
        Self {
            supports_activation: true,
            has_ayatana_secondary_activate: false,
        }
    }
}

/// Whether an error means the item is no longer there, as opposed to merely unhappy.
///
/// The same six the extension treats as fatal (`appIndicator.js:651-658`). Anything else — a
/// timeout, a malformed reply — is a client having a bad time, not a client that has gone.
fn is_gone(err: &zbus::Error) -> bool {
    let zbus::Error::MethodError(name, _, _) = err else {
        return false;
    };
    matches!(
        name.as_str(),
        "org.freedesktop.DBus.Error.NameHasNoOwner"
            | "org.freedesktop.DBus.Error.ServiceUnknown"
            | "org.freedesktop.DBus.Error.UnknownObject"
            | "org.freedesktop.DBus.Error.UnknownInterface"
            | "org.freedesktop.DBus.Error.UnknownMethod"
            | "org.freedesktop.DBus.Error.UnknownProperty"
    )
}

/// Drop an item that answered as gone, announcing it exactly as an owner death would.
async fn retire(
    conn: &zbus::Connection,
    registry: &Arc<Mutex<ItemRegistry>>,
    to_niri: &calloop::channel::Sender<StatusNotifierToSynoik>,
    item: &RegisteredItem,
) {
    let removed = registry
        .lock()
        .unwrap()
        .remove_item(&item.unique_name, &item.object_path);
    let Some(id) = removed else {
        return;
    };

    debug!("status-notifier: {id} answered as gone");
    let _ = to_niri.send(StatusNotifierToSynoik::ItemUnregistered { id: id.clone() });
    if let Err(err) = announce_unregistered(conn, &id).await {
        warn!("status-notifier: error announcing {id} unregistered: {err:?}");
    }
}

/// Emit `StatusNotifierItemUnregistered` plus the property change, from outside the interface's
/// own method context (where zbus would hand us an emitter).
async fn announce_unregistered(conn: &zbus::Connection, id: &str) -> zbus::Result<()> {
    let emitter = SignalEmitter::new(conn, OBJECT_PATH)?;
    StatusNotifierWatcher::status_notifier_item_unregistered(&emitter, id).await?;

    let iface = conn
        .object_server()
        .interface::<_, StatusNotifierWatcher>(OBJECT_PATH)
        .await?;
    let iface_ref = iface.get().await;
    iface_ref
        .registered_status_notifier_items_changed(&emitter)
        .await
}

impl Start for StatusNotifierWatcher {
    fn start(self) -> anyhow::Result<zbus::blocking::Connection> {
        let conn = zbus::blocking::Connection::session()?;
        conn.object_server().at(OBJECT_PATH, self)?;

        // DoNotQueue without ReplaceExisting: if something else is already the watcher, queueing
        // behind it would leave us answering nothing while clients talk to it. Better to fail the
        // start loudly. (Unlike the notification daemon, we do not replace: a running watcher is
        // more likely to be a second shell than a stale daemon.)
        let flags = RequestNameFlags::DoNotQueue.into();
        match conn.request_name_with_flags(BUS_NAME, flags)? {
            RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner => (),
            reply => anyhow::bail!(
                "{BUS_NAME} is owned by another watcher (request replied {reply:?}); \
                 app indicators disabled"
            ),
        }

        // Announce the host *after* the name is ours, so a client that reacts to the signal by
        // registering finds a watcher to register with.
        let emit_conn = conn.inner().clone();
        conn.inner()
            .executor()
            .spawn(
                async move {
                    match SignalEmitter::new(&emit_conn, OBJECT_PATH) {
                        Ok(emitter) => {
                            if let Err(err) =
                                StatusNotifierWatcher::status_notifier_host_registered(&emitter)
                                    .await
                            {
                                warn!("status-notifier: error announcing the host: {err:?}");
                            }
                        }
                        Err(err) => warn!("status-notifier: no signal emitter: {err:?}"),
                    }
                },
                "announce the StatusNotifier host",
            )
            .detach();

        Ok(conn)
    }
}
