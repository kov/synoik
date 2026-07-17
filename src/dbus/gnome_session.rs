//! The `org.gnome.SessionManager.EndSessionDialog` interface, and the client calls that trigger it.
//!
//! niri runs as the `org.gnome.Shell` component of a real gnome-session (systemd-managed, so no
//! `RegisterClient` handshake is needed — gnome-shell 50.1 doesn't register either). The one piece
//! of the session lifecycle the shell owns is the logout/shutdown/restart confirmation dialog:
//!
//! - **We implement** `org.gnome.SessionManager.EndSessionDialog` at
//!   `/org/gnome/SessionManager/EndSessionDialog`. gnome-session calls `Open(type, timestamp,
//!   seconds, inhibitors)` on us to raise the dialog and waits for us to emit `ConfirmedLogout` /
//!   `ConfirmedReboot` / `ConfirmedShutdown` (proceed) or `Canceled` (abort). Those signals are
//!   broadcast (gnome-session listens on the object), and emitted from the main loop by
//!   [`crate::niri::Niri::emit_end_session_signal`]; the [`crate::end_session::EndSession`] state
//!   machine holds the dialog lifecycle.
//! - **We call** `org.gnome.SessionManager.{Logout,Shutdown,Reboot}` to *start* that flow, from the
//!   `Logout` / `PowerOff` / `Reboot` actions — the same methods gnome-shell's systemActions.js
//!   invokes. gnome-session then calls `Open` back on us, closing the loop.
//!
//! The inhibitor object paths passed to `Open` (apps blocking logout, other logged-in sessions) are
//! accepted and ignored for now — the dialog doesn't yet list them.

use zbus::interface;
use zbus::zvariant::OwnedObjectPath;

use super::Start;
use crate::end_session::SessionRequest;

pub struct EndSessionDialog {
    to_niri: calloop::channel::Sender<EndSessionDialogToNiri>,
}

pub enum EndSessionDialogToNiri {
    /// `Open`: raise the dialog for `kind` (0 logout / 1 shutdown / 2 restart), auto-confirming
    /// after `seconds` (0 = no countdown).
    Open { kind: u32, seconds: u64 },
    /// `Close`: gnome-session withdrew the request; dismiss without emitting a signal.
    Close,
}

impl EndSessionDialog {
    pub fn new(to_niri: calloop::channel::Sender<EndSessionDialogToNiri>) -> Self {
        Self { to_niri }
    }

    fn notify(&self, msg: EndSessionDialogToNiri) {
        if let Err(err) = self.to_niri.send(msg) {
            warn!("error sending message to niri: {err:?}");
        }
    }
}

#[interface(name = "org.gnome.SessionManager.EndSessionDialog")]
impl EndSessionDialog {
    async fn open(
        &self,
        r#type: u32,
        _timestamp: u32,
        seconds_to_stay_open: u32,
        _inhibitor_object_paths: Vec<OwnedObjectPath>,
    ) {
        self.notify(EndSessionDialogToNiri::Open {
            kind: r#type,
            seconds: u64::from(seconds_to_stay_open),
        });
    }

    async fn close(&self) {
        self.notify(EndSessionDialogToNiri::Close);
    }

    // Emitted (broadcast) from the main loop by `Niri::emit_end_session_signal`; these declarations
    // just provide the introspection XML. Names map to `ConfirmedLogout` / `ConfirmedReboot` /
    // `ConfirmedShutdown` / `Canceled` / `Closed`.
    #[zbus(signal)]
    pub async fn confirmed_logout(
        emitter: &zbus::object_server::SignalEmitter<'_>,
    ) -> zbus::Result<()>;
    #[zbus(signal)]
    pub async fn confirmed_reboot(
        emitter: &zbus::object_server::SignalEmitter<'_>,
    ) -> zbus::Result<()>;
    #[zbus(signal)]
    pub async fn confirmed_shutdown(
        emitter: &zbus::object_server::SignalEmitter<'_>,
    ) -> zbus::Result<()>;
    #[zbus(signal)]
    pub async fn canceled(emitter: &zbus::object_server::SignalEmitter<'_>) -> zbus::Result<()>;
    #[zbus(signal)]
    pub async fn closed(emitter: &zbus::object_server::SignalEmitter<'_>) -> zbus::Result<()>;
}

impl Start for EndSessionDialog {
    fn start(self) -> anyhow::Result<zbus::blocking::Connection> {
        let conn = zbus::blocking::Connection::session()?;
        conn.object_server()
            .at("/org/gnome/SessionManager/EndSessionDialog", self)?;
        Ok(conn)
    }
}

/// Fire-and-forget the matching `org.gnome.SessionManager` method — the trigger the `Logout` /
/// `PowerOff` / `Reboot` actions use. gnome-session responds by calling `EndSessionDialog.Open`
/// back on us (mode 0 = show the confirmation dialog). Spawned on the connection's executor so it
/// never blocks the main loop; a failure (no gnome-session, denied) is only logged, matching
/// gnome-shell's `.catch(logError)`.
pub fn request_session_action(conn: &zbus::blocking::Connection, request: SessionRequest) {
    let async_conn = conn.inner().clone();
    let future = async move {
        let dest = Some("org.gnome.SessionManager");
        let path = "/org/gnome/SessionManager";
        let iface = Some("org.gnome.SessionManager");
        let res = match request {
            // mode 0 = normal logout (show the confirmation dialog).
            SessionRequest::Logout => {
                async_conn
                    .call_method(dest, path, iface, "Logout", &(0u32,))
                    .await
            }
            SessionRequest::PowerOff => {
                async_conn
                    .call_method(dest, path, iface, "Shutdown", &())
                    .await
            }
            SessionRequest::Reboot => {
                async_conn
                    .call_method(dest, path, iface, "Reboot", &())
                    .await
            }
        };
        if let Err(err) = res {
            warn!("error requesting {request:?} from org.gnome.SessionManager: {err:?}");
        }
    };
    conn.inner()
        .executor()
        .spawn(future, "call org.gnome.SessionManager")
        .detach();
}
