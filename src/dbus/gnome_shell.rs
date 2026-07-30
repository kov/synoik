//! The `org.gnome.Shell` interface — accelerator grabs.
//!
//! gsd-media-keys (volume/brightness/media keys), gnome-control-center's
//! shortcut capture, and the GNOME GlobalShortcuts portal register their key
//! combos here instead of listening for keys themselves; the compositor sends
//! back unicast `AcceleratorActivated`/`AcceleratorDeactivated` signals. The
//! consumers watch this bus name and (re-)grab whenever it appears, so
//! claiming it after they start is fine.
//!
//! It also carries `ShowOSD` — the *inbound* half of the OSD subsystem
//! (`js/ui/shellDBus.js:121-153`). gsd-media-keys handles the volume/mute/
//! mic/keyboard-backlight keys itself and then asks us to draw the feedback,
//! which is why the compositor must not handle those keys: see
//! `docs/fork/osd-media-port.md`.
//!
//! Divergences from gnome-shell (for now): `modeFlags` only gates lock-screen
//! use rather than the full ActionMode set; the `parameters` dict carries
//! `timestamp` and `action-mode` but not `activation-token`/`device-node`; and
//! the accelerator methods still have **no sender allowlist** (gnome-shell
//! checks all of them). `ShowOSD` does check, because it is the first method
//! here that lets a caller draw arbitrary text and icons across every monitor.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread;

use zbus::blocking::fdo::DBusProxy;
use zbus::fdo::{self, RequestNameFlags};
use zbus::message::Header;
use zbus::names::BusName;
use zbus::{interface, zvariant};

use super::Start;

/// The well-known names gnome-shell accepts privileged calls from
/// (`js/ui/shellDBus.js:27-31`).
const ALLOWED_SENDERS: &[&str] = &[
    "org.gnome.Settings",
    "org.gnome.SettingsDaemon.MediaKeys",
    "org.freedesktop.impl.portal.desktop.gnome",
];

/// The current unique-name owner of each entry in [`ALLOWED_SENDERS`] —
/// gnome-shell's `DBusSenderChecker` (`js/misc/util.js:344-419`), which watches
/// the well-known names and compares an invocation's sender against the owners.
/// Checking the *unique* name is the point: a well-known name in the message
/// header would be spoofable, and the sender field never carries one anyway.
type Allowlist = Arc<Mutex<HashMap<String, String>>>;

pub struct GnomeShell {
    to_niri: calloop::channel::Sender<GnomeShellToNiri>,
    allowlist: Allowlist,
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
    /// `ShowOSD` (`js/ui/shellDBus.js:121-153`). Every field is optional in the
    /// `a{sv}`; `connector` absent means every monitor.
    ShowOsd {
        connector: Option<String>,
        label: Option<String>,
        level: Option<f64>,
        max_level: Option<f64>,
        /// A serialized `GIcon` (`Gio.Icon.new_for_string`), not a bare name.
        icon: Option<String>,
    },
}

/// One level of variant unwrapping, since an `a{sv}` entry may itself hold a
/// variant depending on how the caller built the dict.
fn peel(value: &zvariant::Value<'_>) -> zvariant::Value<'static> {
    match value {
        zvariant::Value::Value(inner) => inner
            .as_ref()
            .try_to_owned()
            .map_or_else(|_| zvariant::Value::U8(0), zvariant::Value::from),
        other => other
            .try_to_owned()
            .map_or(zvariant::Value::U8(0), Into::into),
    }
}

fn as_string(value: &zvariant::OwnedValue) -> Option<String> {
    match peel(value) {
        zvariant::Value::Str(s) => Some(s.to_string()),
        _ => None,
    }
}

/// gnome-shell just `deepUnpack`s and hands the number on, and JS does not
/// distinguish integer from double — so accept whatever numeric type the caller
/// chose rather than only `d`.
fn as_f64(value: &zvariant::OwnedValue) -> Option<f64> {
    use zvariant::Value::*;
    Some(match peel(value) {
        F64(v) => v,
        I16(v) => f64::from(v),
        I32(v) => f64::from(v),
        I64(v) => v as f64,
        U8(v) => f64::from(v),
        U16(v) => f64::from(v),
        U32(v) => f64::from(v),
        U64(v) => v as f64,
        _ => return None,
    })
}

fn sender(hdr: &Header<'_>) -> fdo::Result<String> {
    hdr.sender()
        .map(|name| name.to_string())
        .ok_or_else(|| fdo::Error::Failed("no sender".to_owned()))
}

impl GnomeShell {
    pub fn new(to_niri: calloop::channel::Sender<GnomeShellToNiri>) -> Self {
        Self {
            to_niri,
            allowlist: Allowlist::default(),
        }
    }

    /// gnome-shell's `DBusSenderChecker.checkInvocation` (`js/misc/util.js:399-409`):
    /// the sender's unique name must currently own one of [`ALLOWED_SENDERS`].
    fn check_sender(&self, hdr: &Header<'_>) -> fdo::Result<()> {
        let sender = sender(hdr)?;
        let allowed = self
            .allowlist
            .lock()
            .unwrap()
            .values()
            .any(|owner| *owner == sender);
        if allowed {
            Ok(())
        } else {
            // gnome-shell returns ACCESS_DENIED with exactly this text.
            Err(fdo::Error::AccessDenied(
                "ShowOSD is not allowed".to_owned(),
            ))
        }
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

    /// Draw an on-screen display. Params (all optional): `connector` (absent =
    /// every monitor), `label`, `level`, `max_level`, `icon` (a serialized
    /// `GIcon`). gnome-shell passes them straight to the OSD manager after a
    /// `deepUnpack` (`js/ui/shellDBus.js:132-152`).
    #[zbus(name = "ShowOSD")]
    async fn show_osd(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        params: HashMap<String, zvariant::OwnedValue>,
    ) -> fdo::Result<()> {
        self.check_sender(&hdr)?;
        let msg = GnomeShellToNiri::ShowOsd {
            connector: params.get("connector").and_then(as_string),
            label: params.get("label").and_then(as_string),
            level: params.get("level").and_then(as_f64),
            max_level: params.get("max_level").and_then(as_f64),
            icon: params.get("icon").and_then(as_string),
        };
        self.to_niri.send(msg).map_err(|err| {
            warn!("error sending message to niri: {err:?}");
            fdo::Error::Failed("internal error".to_owned())
        })
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
        let allowlist = self.allowlist.clone();
        let conn = zbus::blocking::Connection::session()?;
        let flags = RequestNameFlags::AllowReplacement
            | RequestNameFlags::ReplaceExisting
            | RequestNameFlags::DoNotQueue;

        // Subscribe BEFORE the initial ownership queries below, so a name that
        // changes hands in between shows up as a signal rather than being missed.
        let proxy = DBusProxy::new(&conn)?;
        let changed = proxy.receive_name_owner_changed()?;

        // Seed the allowlist. gnome-shell instead *awaits* its watchers' first
        // callback before answering a checked call (`js/misc/util.js:386`); doing
        // it eagerly here means an early ShowOSD is answered against real state
        // rather than an empty map, which would deny it.
        for name in ALLOWED_SENDERS {
            if let Ok(owner) = proxy.get_name_owner(BusName::try_from(*name)?) {
                allowlist
                    .lock()
                    .unwrap()
                    .insert((*name).to_owned(), owner.to_string());
            }
        }

        conn.object_server().at("/org/gnome/Shell", self)?;
        conn.request_name_with_flags("org.gnome.Shell", flags)?;

        // Drop a sender's grabs when it leaves the bus, like gnome-shell's
        // per-sender bus-name watch, and keep the ShowOSD allowlist current.
        thread::Builder::new()
            .name("org.gnome.Shell name watcher".to_owned())
            .spawn(move || {
                for signal in changed {
                    let Ok(args) = signal.args() else { continue };
                    match (&args.name, args.new_owner.as_ref()) {
                        // A unique name disappearing entirely.
                        (BusName::Unique(name), None) => {
                            if to_niri
                                .send(GnomeShellToNiri::SenderVanished(name.to_string()))
                                .is_err()
                            {
                                return;
                            }
                        }
                        (BusName::WellKnown(name), owner)
                            if ALLOWED_SENDERS.contains(&name.as_str()) =>
                        {
                            let mut map = allowlist.lock().unwrap();
                            match owner {
                                Some(owner) => {
                                    map.insert(name.to_string(), owner.to_string());
                                }
                                None => {
                                    map.remove(name.as_str());
                                }
                            }
                        }
                        _ => (),
                    }
                }
            })?;

        Ok(conn)
    }
}
