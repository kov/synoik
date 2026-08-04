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
use zbus::{fdo, interface};

use super::Start;
use crate::status_notifier::{
    item_id, parse_service_argument, ItemRegistry, ParseError, RegisteredItem, Registration,
    ServiceRef, StatusNotifierToSynoik, DEFAULT_ITEM_OBJECT_PATH,
};

pub const BUS_NAME: &str = "org.kde.StatusNotifierWatcher";
pub const OBJECT_PATH: &str = "/StatusNotifierWatcher";

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

        let _ = self
            .to_niri
            .send(StatusNotifierToSynoik::ItemRegistered(item.clone()));

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
