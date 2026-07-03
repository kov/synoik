//! The `org.gnome.Shell` interface — accelerator grabs.
//!
//! gsd-media-keys (volume/brightness/media keys), gnome-control-center's
//! shortcut capture, and the GNOME GlobalShortcuts portal register their key
//! combos here instead of listening for keys themselves; the compositor sends
//! back unicast `AcceleratorActivated`/`AcceleratorDeactivated` signals. The
//! consumers watch this bus name and (re-)grab whenever it appears, so
//! claiming it after they start is fine.
//!
//! Divergences from gnome-shell (for now): no sender allowlist (gnome-shell
//! only accepts org.gnome.Settings, org.gnome.SettingsDaemon.MediaKeys, and
//! the GNOME portal backend); `modeFlags` only gates lock-screen use rather
//! than the full ActionMode set; the `parameters` dict carries `timestamp`
//! and `action-mode` but not `activation-token`/`device-node`.

use std::thread;

use zbus::blocking::fdo::DBusProxy;
use zbus::fdo::{self, RequestNameFlags};
use zbus::message::Header;
use zbus::names::BusName;
use zbus::{interface, zvariant};

use super::Start;

pub struct GnomeShell {
    to_niri: calloop::channel::Sender<GnomeShellToNiri>,
}

pub enum GnomeShellToNiri {
    Grab {
        accelerator: String,
        mode_flags: u32,
        grab_flags: u32,
        sender: String,
        /// Replies with the grab's action id; 0 = refused (mutter's
        /// `META_KEYBINDING_ACTION_NONE`).
        reply: async_channel::Sender<u32>,
    },
    Ungrab {
        action: u32,
        sender: String,
        reply: async_channel::Sender<bool>,
    },
    SenderVanished(String),
}

fn sender(hdr: &Header<'_>) -> fdo::Result<String> {
    hdr.sender()
        .map(|name| name.to_string())
        .ok_or_else(|| fdo::Error::Failed("no sender".to_owned()))
}

impl GnomeShell {
    pub fn new(to_niri: calloop::channel::Sender<GnomeShellToNiri>) -> Self {
        Self { to_niri }
    }

    async fn grab_one(
        &self,
        accelerator: String,
        mode_flags: u32,
        grab_flags: u32,
        sender: String,
    ) -> fdo::Result<u32> {
        let (reply, rx) = async_channel::bounded(1);
        self.to_niri
            .send(GnomeShellToNiri::Grab {
                accelerator,
                mode_flags,
                grab_flags,
                sender,
                reply,
            })
            .map_err(|err| {
                warn!("error sending message to niri: {err:?}");
                fdo::Error::Failed("internal error".to_owned())
            })?;
        rx.recv().await.map_err(|err| {
            warn!("error receiving message from niri: {err:?}");
            fdo::Error::Failed("internal error".to_owned())
        })
    }

    async fn ungrab_one(&self, action: u32, sender: String) -> fdo::Result<bool> {
        let (reply, rx) = async_channel::bounded(1);
        self.to_niri
            .send(GnomeShellToNiri::Ungrab {
                action,
                sender,
                reply,
            })
            .map_err(|err| {
                warn!("error sending message to niri: {err:?}");
                fdo::Error::Failed("internal error".to_owned())
            })?;
        rx.recv().await.map_err(|err| {
            warn!("error receiving message from niri: {err:?}");
            fdo::Error::Failed("internal error".to_owned())
        })
    }
}

#[interface(name = "org.gnome.Shell")]
impl GnomeShell {
    async fn grab_accelerator(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        accelerator: String,
        mode_flags: u32,
        grab_flags: u32,
    ) -> fdo::Result<u32> {
        let sender = sender(&hdr)?;
        self.grab_one(accelerator, mode_flags, grab_flags, sender)
            .await
    }

    async fn grab_accelerators(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        accelerators: Vec<(String, u32, u32)>,
    ) -> fdo::Result<Vec<u32>> {
        // Like gnome-shell, the plural form is a convenience loop; each entry
        // grabs (or refuses with 0) independently.
        let sender = sender(&hdr)?;
        let mut actions = Vec::with_capacity(accelerators.len());
        for (accelerator, mode_flags, grab_flags) in accelerators {
            actions.push(
                self.grab_one(accelerator, mode_flags, grab_flags, sender.clone())
                    .await?,
            );
        }
        Ok(actions)
    }

    async fn ungrab_accelerator(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        action: u32,
    ) -> fdo::Result<bool> {
        let sender = sender(&hdr)?;
        self.ungrab_one(action, sender).await
    }

    async fn ungrab_accelerators(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        action: Vec<u32>,
    ) -> fdo::Result<bool> {
        let sender = sender(&hdr)?;
        let mut all = true;
        for action in action {
            all &= self.ungrab_one(action, sender.clone()).await?;
        }
        Ok(all)
    }

    // The activated/deactivated signals are emitted unicast to the grabbing
    // sender from the main loop (`Niri::emit_accelerator_signal`); these
    // declarations provide the introspection XML.
    #[zbus(signal)]
    pub async fn accelerator_activated(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        action: u32,
        parameters: std::collections::HashMap<String, zvariant::OwnedValue>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn accelerator_deactivated(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        action: u32,
        parameters: std::collections::HashMap<String, zvariant::OwnedValue>,
    ) -> zbus::Result<()>;
}

impl Start for GnomeShell {
    fn start(self) -> anyhow::Result<zbus::blocking::Connection> {
        let to_niri = self.to_niri.clone();
        let conn = zbus::blocking::Connection::session()?;
        let flags = RequestNameFlags::AllowReplacement
            | RequestNameFlags::ReplaceExisting
            | RequestNameFlags::DoNotQueue;

        conn.object_server().at("/org/gnome/Shell", self)?;
        conn.request_name_with_flags("org.gnome.Shell", flags)?;

        // Drop a sender's grabs when it leaves the bus, like gnome-shell's
        // per-sender bus-name watch.
        let watch_conn = conn.clone();
        thread::Builder::new()
            .name("org.gnome.Shell name watcher".to_owned())
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
                    // A unique name disappearing entirely.
                    if let (BusName::Unique(name), None) = (&args.name, args.new_owner.as_ref()) {
                        if to_niri
                            .send(GnomeShellToNiri::SenderVanished(name.to_string()))
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
