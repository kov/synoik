//! The `org.gnome.Mutter.IdleMonitor` interface — user-activity monitoring.
//!
//! gnome-settings-daemon's power plugin hard-codes this bus name: it adds idle watches ("fire when
//! the user has been idle for N ms") to drive screen dim, blank, and auto-suspend, and user-active
//! watches ("fire when the user comes back") to undo them. The compositor already detects activity;
//! [`crate::idle_monitor::IdleMonitor`] holds the watch bookkeeping and this exposes it on the bus.
//!
//! Only the `Core` monitor at `/org/gnome/Mutter/IdleMonitor/Core` is served — the object
//! gnome-desktop's `GnomeIdleMonitor` (used by gsd) talks to. mutter also serves per-input-device
//! monitors under an ObjectManager, which nothing in the gsd path consumes; that is a follow-up.
//!
//! Like `org.gnome.Shell`'s accelerator signals, `WatchFired` is emitted **unicast** to the client
//! that owns the watch, from the main loop (`Niri::emit_idle_watch_fired`); the request/reply for
//! the returning methods and the per-sender bus-name watch mirror `dbus::gnome_shell`.

use std::thread;

use zbus::blocking::fdo::DBusProxy;
use zbus::fdo::{self, RequestNameFlags};
use zbus::interface;
use zbus::message::Header;
use zbus::names::BusName;

use super::Start;

pub struct IdleMonitor {
    to_niri: calloop::channel::Sender<IdleMonitorToNiri>,
}

pub enum IdleMonitorToNiri {
    GetIdletime {
        reply: async_channel::Sender<u64>,
    },
    AddIdleWatch {
        interval: u64,
        owner: String,
        reply: async_channel::Sender<u32>,
    },
    AddUserActiveWatch {
        owner: String,
        reply: async_channel::Sender<u32>,
    },
    RemoveWatch {
        id: u32,
    },
    ResetIdletime,
    SenderVanished(String),
}

fn sender(hdr: &Header<'_>) -> fdo::Result<String> {
    hdr.sender()
        .map(|name| name.to_string())
        .ok_or_else(|| fdo::Error::Failed("no sender".to_owned()))
}

impl IdleMonitor {
    pub fn new(to_niri: calloop::channel::Sender<IdleMonitorToNiri>) -> Self {
        Self { to_niri }
    }

    async fn request<T>(
        &self,
        make: impl FnOnce(async_channel::Sender<T>) -> IdleMonitorToNiri,
    ) -> fdo::Result<T> {
        let (reply, rx) = async_channel::bounded(1);
        self.to_niri.send(make(reply)).map_err(|err| {
            warn!("error sending message to niri: {err:?}");
            fdo::Error::Failed("internal error".to_owned())
        })?;
        rx.recv().await.map_err(|err| {
            warn!("error receiving message from niri: {err:?}");
            fdo::Error::Failed("internal error".to_owned())
        })
    }

    fn notify(&self, msg: IdleMonitorToNiri) -> fdo::Result<()> {
        self.to_niri.send(msg).map_err(|err| {
            warn!("error sending message to niri: {err:?}");
            fdo::Error::Failed("internal error".to_owned())
        })
    }
}

#[interface(name = "org.gnome.Mutter.IdleMonitor")]
impl IdleMonitor {
    async fn get_idletime(&self) -> fdo::Result<u64> {
        self.request(|reply| IdleMonitorToNiri::GetIdletime { reply })
            .await
    }

    async fn add_idle_watch(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        interval: u64,
    ) -> fdo::Result<u32> {
        let owner = sender(&hdr)?;
        self.request(|reply| IdleMonitorToNiri::AddIdleWatch {
            interval,
            owner,
            reply,
        })
        .await
    }

    async fn add_user_active_watch(&self, #[zbus(header)] hdr: Header<'_>) -> fdo::Result<u32> {
        let owner = sender(&hdr)?;
        self.request(|reply| IdleMonitorToNiri::AddUserActiveWatch { owner, reply })
            .await
    }

    async fn remove_watch(&self, id: u32) -> fdo::Result<()> {
        self.notify(IdleMonitorToNiri::RemoveWatch { id })
    }

    async fn reset_idletime(&self) -> fdo::Result<()> {
        self.notify(IdleMonitorToNiri::ResetIdletime)
    }

    // Emitted unicast to the owning client from the main loop (`Niri::emit_idle_watch_fired`); this
    // declaration provides the introspection XML.
    #[zbus(signal)]
    pub async fn watch_fired(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        id: u32,
    ) -> zbus::Result<()>;
}

impl Start for IdleMonitor {
    fn start(self) -> anyhow::Result<zbus::blocking::Connection> {
        let to_niri = self.to_niri.clone();
        let conn = zbus::blocking::Connection::session()?;
        let flags = RequestNameFlags::AllowReplacement
            | RequestNameFlags::ReplaceExisting
            | RequestNameFlags::DoNotQueue;

        conn.object_server()
            .at("/org/gnome/Mutter/IdleMonitor/Core", self)?;
        conn.request_name_with_flags("org.gnome.Mutter.IdleMonitor", flags)?;

        // Drop a client's watches when it leaves the bus, like mutter's per-name watch.
        let watch_conn = conn.clone();
        thread::Builder::new()
            .name("org.gnome.Mutter.IdleMonitor name watcher".to_owned())
            .spawn(move || {
                let proxy = match DBusProxy::new(&watch_conn) {
                    Ok(proxy) => proxy,
                    Err(err) => {
                        warn!("error creating DBus proxy: {err:?}");
                        return;
                    }
                };
                let changed = match proxy.receive_name_owner_changed() {
                    Ok(changed) => changed,
                    Err(err) => {
                        warn!("error subscribing to NameOwnerChanged: {err:?}");
                        return;
                    }
                };
                for signal in changed {
                    let Ok(args) = signal.args() else { continue };
                    if let (BusName::Unique(name), None) = (&args.name, args.new_owner.as_ref()) {
                        if to_niri
                            .send(IdleMonitorToNiri::SenderVanished(name.to_string()))
                            .is_err()
                        {
                            return;
                        }
                    }
                }
            })?;

        Ok(conn)
    }
}
