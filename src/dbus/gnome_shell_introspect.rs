//! `org.gnome.Shell.Introspect` — the window and application list
//! (`js/misc/introspect.js`, `data/dbus-interfaces/org.gnome.Shell.Introspect.xml`).
//!
//! This is how `xdg-desktop-portal-gnome` builds the chooser you get when a browser asks to share
//! a window, so it is on the critical path for screen sharing.
//!
//! **Everything here is access-controlled.** The window list carries every window's title, which is
//! a running commentary on what the user is doing — a document name, a chat contact, a URL. GNOME
//! answers `GetWindows` and `GetRunningApplications` only for the two portal implementations
//! ([`APP_ALLOWLIST`], `introspect.js:7-11`), and every method begins with the check
//! (`:124-127`, `:139-145`). See [`check_sender`].

use std::collections::HashMap;

use zbus::fdo::{self, RequestNameFlags};
use zbus::interface;
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{SerializeDict, Type, Value};

use super::Start;

/// `INTROSPECT_DBUS_API_VERSION` (`introspect.js:12`).
///
/// `u`, not `i` — the interface XML says
/// `<property name="version" type="u" access="read"/>`. Getting the width right is not cosmetic:
/// xdg-desktop-portal-gnome logs "Received property version with type i does not match expected
/// type u" and its Introspect proxy is no use afterwards.
const API_VERSION: u32 = 3;

/// The only senders allowed to read the window list (`introspect.js:7-11`).
///
/// GNOME hardcodes exactly these two, and we do the same rather than making it configurable: an
/// escape hatch here is a way to reopen the leak this list exists to close.
const APP_ALLOWLIST: [&str; 2] = [
    "org.freedesktop.impl.portal.desktop.gtk",
    "org.freedesktop.impl.portal.desktop.gnome",
];

pub struct Introspect {
    to_niri: calloop::channel::Sender<IntrospectToSynoik>,
    from_niri: async_channel::Receiver<SynoikToIntrospect>,
    /// Filled in by [`Start`], because the object needs the bus it is served on to resolve the
    /// allowlist's owners.
    conn: Option<zbus::Connection>,
}

pub enum IntrospectToSynoik {
    GetWindows,
    GetRunningApplications,
    /// The union bounding box of all outputs — `global.screen_width/height` (`introspect.js:198`).
    GetScreenSize,
    GetAnimationsEnabled,
}

pub enum SynoikToIntrospect {
    Windows(HashMap<u64, WindowProperties>),
    RunningApplications(HashMap<String, AppProperties>),
    ScreenSize(i32, i32),
    AnimationsEnabled(bool),
}

/// `META_WINDOW_CLIENT_TYPE_*` (`meta/window.h:86-90`) — Wayland is 0, X11 is 1.
pub const CLIENT_TYPE_WAYLAND: u32 = 0;

/// One entry of `GetWindows` (`introspect.js:163-181`).
///
/// The six unconditional fields are always sent; `title`, `wm-class` and `sandboxed-app-id` are
/// omitted when GNOME has nothing for them, so they are `Option` here rather than empty strings —
/// a chooser showing a blank row is not the same as one showing no row.
#[derive(Debug, SerializeDict, Type, Value)]
#[zvariant(signature = "dict")]
pub struct WindowProperties {
    /// The resolved **desktop id**, not the Wayland app id — the chooser looks up the icon by it.
    #[zvariant(rename = "app-id")]
    pub app_id: String,
    #[zvariant(rename = "client-type")]
    pub client_type: u32,
    #[zvariant(rename = "is-hidden")]
    pub is_hidden: bool,
    #[zvariant(rename = "has-focus")]
    pub has_focus: bool,
    pub width: u32,
    pub height: u32,
    pub title: Option<String>,
    #[zvariant(rename = "wm-class")]
    pub wm_class: Option<String>,
}

/// One entry of `GetRunningApplications` (`introspect.js:83-99`), keyed by desktop id.
///
/// GNOME sends an *empty* dict for an app that is merely running and adds `active-on-seats` only
/// for the focused one, so this is not a bool.
#[derive(Debug, Default, SerializeDict, Type, Value)]
#[zvariant(signature = "dict")]
pub struct AppProperties {
    #[zvariant(rename = "active-on-seats")]
    pub active_on_seats: Option<Vec<String>>,
}

#[interface(name = "org.gnome.Shell.Introspect")]
impl Introspect {
    async fn get_windows(
        &self,
        #[zbus(header)] hdr: Header<'_>,
    ) -> fdo::Result<HashMap<u64, WindowProperties>> {
        self.check_sender(&hdr, "GetWindows").await?;

        match self.ask(IntrospectToSynoik::GetWindows).await? {
            SynoikToIntrospect::Windows(windows) => Ok(windows),
            _ => Err(fdo::Error::Failed("internal error".to_owned())),
        }
    }

    async fn get_running_applications(
        &self,
        #[zbus(header)] hdr: Header<'_>,
    ) -> fdo::Result<HashMap<String, AppProperties>> {
        self.check_sender(&hdr, "GetRunningApplications").await?;

        match self.ask(IntrospectToSynoik::GetRunningApplications).await? {
            SynoikToIntrospect::RunningApplications(apps) => Ok(apps),
            _ => Err(fdo::Error::Failed("internal error".to_owned())),
        }
    }

    /// The union bounding box of every output, not one monitor's size (`introspect.js:196-206`).
    #[zbus(property)]
    async fn screen_size(&self) -> fdo::Result<(i32, i32)> {
        match self.ask(IntrospectToSynoik::GetScreenSize).await? {
            SynoikToIntrospect::ScreenSize(w, h) => Ok((w, h)),
            _ => Err(fdo::Error::Failed("internal error".to_owned())),
        }
    }

    /// `org.gnome.desktop.interface enable-animations`, which the portal reads to decide whether to
    /// animate its own dialogs (`introspect.js:184-192`).
    #[zbus(property)]
    async fn animations_enabled(&self) -> fdo::Result<bool> {
        match self.ask(IntrospectToSynoik::GetAnimationsEnabled).await? {
            SynoikToIntrospect::AnimationsEnabled(on) => Ok(on),
            _ => Err(fdo::Error::Failed("internal error".to_owned())),
        }
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        API_VERSION
    }

    #[zbus(signal)]
    pub async fn windows_changed(ctxt: &SignalEmitter<'_>) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn running_applications_changed(ctxt: &SignalEmitter<'_>) -> zbus::Result<()>;
}

impl Introspect {
    pub fn new(
        to_niri: calloop::channel::Sender<IntrospectToSynoik>,
        from_niri: async_channel::Receiver<SynoikToIntrospect>,
    ) -> Self {
        Self {
            to_niri,
            from_niri,
            conn: None,
        }
    }

    async fn check_sender(&self, hdr: &Header<'_>, method: &str) -> fdo::Result<()> {
        let Some(conn) = self.conn.as_ref() else {
            return Err(fdo::Error::Failed("internal error".to_owned()));
        };
        super::check_sender(conn, hdr.sender(), &APP_ALLOWLIST, method).await
    }

    async fn ask(&self, msg: IntrospectToSynoik) -> fdo::Result<SynoikToIntrospect> {
        if let Err(err) = self.to_niri.send(msg) {
            warn!("error sending message to synoik: {err:?}");
            return Err(fdo::Error::Failed("internal error".to_owned()));
        }

        self.from_niri.recv().await.map_err(|err| {
            warn!("error receiving message from synoik: {err:?}");
            fdo::Error::Failed("internal error".to_owned())
        })
    }
}

impl Start for Introspect {
    fn start(mut self) -> anyhow::Result<zbus::blocking::Connection> {
        let conn = zbus::blocking::Connection::session()?;
        let flags = RequestNameFlags::AllowReplacement
            | RequestNameFlags::ReplaceExisting
            | RequestNameFlags::DoNotQueue;

        // The allowlist check needs to ask the bus who owns a name, so the object keeps the
        // connection it is served on.
        self.conn = Some(conn.inner().clone());

        conn.object_server()
            .at("/org/gnome/Shell/Introspect", self)?;
        conn.request_name_with_flags("org.gnome.Shell.Introspect", flags)?;

        Ok(conn)
    }
}
