// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! gnome-software's offline-updates interface, which is how the end-session dialog offers to
//! install pending updates on the way out.
//!
//! **This is not PackageKit.** gnome-shell spoke `org.freedesktop.PackageKit.Offline` on the system
//! bus until `b47e3763e` (first shipped in 50.0) replaced it with
//! `org.gnome.Software.OfflineUpdates` on the **session** bus. gnome-software declares the
//! interface unstable and owns the policy; we only ask what is pending and say what to do
//! afterwards. See `docs/fork/end-session-dialog-port.md` §3.
//!
//! Three rules, all of them load-bearing:
//!
//! - **Never block the power-off.** Every failure here — no gnome-software, a refused call, a
//!   timeout, a state string we don't know — degrades to [`OfflineUpdateState::Unavailable`], which
//!   makes the dialog behave exactly as it did before this file existed. An update query must not
//!   be able to keep someone from shutting their computer down.
//! - **Ask only when the dialog opens.** gnome-software's systemd unit is deliberately delayed so
//!   login stays cheap, and gnome-shell defers building its proxy for that reason
//!   (`js/ui/endSessionDialog.js:222-225`). A proxy built at startup would drag it in early.
//! - **Absence is not an error.** gnome-shell detects it with `g_name_owner === null` (`:300`),
//!   i.e. *is the service running right now* — deliberately NOT auto-starting it. [`query_state`]
//!   uses `NameHasOwner` for the same reason: activating a service to ask it whether it has updates
//!   would defeat the delay it was given.

use std::time::Duration;

use zbus::fdo;

const BUS_NAME: &str = "org.gnome.Software";
const OBJECT_PATH: &str = "/org/gnome/Software/OfflineUpdates";
const IFACE: &str = "org.gnome.Software.OfflineUpdates";

/// How long any single call gets before we give up and treat updates as unavailable. The dialog is
/// on screen with a countdown running; a wedged gnome-software must not hold it.
const CALL_TIMEOUT: Duration = Duration::from_secs(2);

/// What gnome-software says is pending, from `GetState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OfflineUpdateState {
    /// gnome-software isn't running, didn't answer, or answered something we don't understand.
    /// The dialog shows no checkbox and behaves as though this feature did not exist.
    #[default]
    Unavailable,
    /// Running, nothing pending.
    None,
    /// An update is downloaded and can be scheduled via `SetAction`.
    Prepared,
    /// An update is already scheduled for the next reboot.
    Scheduled,
}

impl OfflineUpdateState {
    /// Map `GetState`'s reply. An unrecognized string is [`Unavailable`](Self::Unavailable), not a
    /// guess: the interface is explicitly unstable, so a future state we've never heard of must
    /// fail closed rather than be lumped in with "there is an update to install".
    pub fn from_wire(s: &str) -> Self {
        match s {
            "none" => Self::None,
            "prepared" => Self::Prepared,
            "scheduled" => Self::Scheduled,
            other => {
                warn!("unknown org.gnome.Software.OfflineUpdates state {other:?}");
                Self::Unavailable
            }
        }
    }

    /// Whether there is something for the checkbox to offer to install.
    pub fn has_pending_update(self) -> bool {
        matches!(self, Self::Prepared | Self::Scheduled)
    }
}

/// What gnome-software should do once the updates are applied (`SetAction`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostUpdateAction {
    Reboot,
    Shutdown,
}

impl PostUpdateAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Reboot => "reboot",
            Self::Shutdown => "shutdown",
        }
    }
}

/// Ask gnome-software what is pending, off the main loop, delivering the answer over `to_synoik`.
///
/// Asynchronous on purpose: gnome-shell made `Open` return immediately (`6b482e172`) so the dialog
/// is never held up by this, and the checkbox simply appears when the answer lands. A dialog that
/// waited would be a dialog that a slow service could stall.
pub fn query_state(
    conn: &zbus::blocking::Connection,
    to_synoik: calloop::channel::Sender<OfflineUpdateState>,
) {
    let async_conn = conn.inner().clone();
    let future = async move {
        let state = read_state(&async_conn).await;
        if let Err(err) = to_synoik.send(state) {
            warn!("error sending offline-update state to synoik: {err:?}");
        }
    };
    conn.inner()
        .executor()
        .spawn(future, "query org.gnome.Software.OfflineUpdates state")
        .detach();
}

async fn read_state(conn: &zbus::Connection) -> OfflineUpdateState {
    // Is it running? Not "can it be started" — see the module docs.
    match fdo::DBusProxy::new(conn).await {
        Ok(dbus) => match dbus.name_has_owner(BUS_NAME.try_into().unwrap()).await {
            Ok(true) => (),
            Ok(false) => return OfflineUpdateState::Unavailable,
            Err(err) => {
                warn!("error checking for gnome-software: {err:?}");
                return OfflineUpdateState::Unavailable;
            }
        },
        Err(err) => {
            warn!("error creating DBusProxy to look for gnome-software: {err:?}");
            return OfflineUpdateState::Unavailable;
        }
    }

    let call = conn.call_method(Some(BUS_NAME), OBJECT_PATH, Some(IFACE), "GetState", &());
    let reply = match with_timeout(call).await {
        Some(Ok(reply)) => reply,
        Some(Err(err)) => {
            warn!("error calling OfflineUpdates.GetState: {err:?}");
            return OfflineUpdateState::Unavailable;
        }
        None => {
            warn!("timed out calling OfflineUpdates.GetState");
            return OfflineUpdateState::Unavailable;
        }
    };
    match reply.body().deserialize::<String>() {
        Ok(state) => OfflineUpdateState::from_wire(&state),
        Err(err) => {
            warn!("error reading OfflineUpdates.GetState reply: {err:?}");
            OfflineUpdateState::Unavailable
        }
    }
}

/// `SetAction`: schedule the prepared update and say what to do when it finishes. Returns whether
/// gnome-software accepted it.
///
/// **The caller needs the answer before it emits its `Confirmed*` signal**, so unlike
/// [`query_state`] this blocks — bounded by [`CALL_TIMEOUT`]. That mirrors gnome-shell, which
/// awaits `SetActionAsync` before emitting (`js/ui/endSessionDialog.js:470-497`), and it is why the
/// return value matters: not every backend can change the action afterwards, and one that can't
/// answers `NOT_SUPPORTED`. A `false` here must leave the caller's signal alone.
pub fn set_action(conn: &zbus::blocking::Connection, action: PostUpdateAction) -> bool {
    let async_conn = conn.inner().clone();
    let res = async_io::block_on(with_timeout(async_conn.call_method(
        Some(BUS_NAME),
        OBJECT_PATH,
        Some(IFACE),
        "SetAction",
        &(action.as_str(),),
    )));
    match res {
        Some(Ok(_)) => true,
        Some(Err(err)) => {
            // NOT_SUPPORTED is expected from backends that can't change the post-update action;
            // gnome-shell logs everything else and carries on either way (`:490-494`).
            debug!("OfflineUpdates.SetAction({action:?}) refused: {err:?}");
            false
        }
        None => {
            warn!("timed out calling OfflineUpdates.SetAction");
            false
        }
    }
}

/// `Cancel`: the user unticked the box, so drop any prepared update. Fire-and-forget — nothing
/// downstream depends on the answer, and the session is on its way out.
pub fn cancel(conn: &zbus::blocking::Connection) {
    let async_conn = conn.inner().clone();
    let future = async move {
        let call = async_conn.call_method(Some(BUS_NAME), OBJECT_PATH, Some(IFACE), "Cancel", &());
        if let Some(Err(err)) = with_timeout(call).await {
            warn!("error calling OfflineUpdates.Cancel: {err:?}");
        }
    };
    conn.inner()
        .executor()
        .spawn(future, "cancel org.gnome.Software offline update")
        .detach();
}

/// `Some(result)` if `future` finished within [`CALL_TIMEOUT`], `None` if it ran out of time.
async fn with_timeout<F: std::future::Future>(future: F) -> Option<F::Output> {
    use futures_util::future::{select, Either};

    let timeout = async_io::Timer::after(CALL_TIMEOUT);
    futures_util::pin_mut!(future);
    futures_util::pin_mut!(timeout);
    match select(future, timeout).await {
        Either::Left((out, _)) => Some(out),
        Either::Right(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The interface is documented as unstable, so an unknown state must fail closed. Treating an
    /// unrecognized string as "there is an update" would offer to install something gnome-software
    /// never said was ready.
    #[test]
    fn unknown_states_are_unavailable_not_pending() {
        assert_eq!(
            OfflineUpdateState::from_wire("none"),
            OfflineUpdateState::None
        );
        assert_eq!(
            OfflineUpdateState::from_wire("prepared"),
            OfflineUpdateState::Prepared
        );
        assert_eq!(
            OfflineUpdateState::from_wire("scheduled"),
            OfflineUpdateState::Scheduled
        );
        for weird in ["", "PREPARED", "downloading", "unknown"] {
            assert_eq!(
                OfflineUpdateState::from_wire(weird),
                OfflineUpdateState::Unavailable,
                "{weird:?}"
            );
            assert!(!OfflineUpdateState::from_wire(weird).has_pending_update());
        }
    }

    #[test]
    fn only_prepared_and_scheduled_offer_an_install() {
        assert!(OfflineUpdateState::Prepared.has_pending_update());
        assert!(OfflineUpdateState::Scheduled.has_pending_update());
        assert!(!OfflineUpdateState::None.has_pending_update());
        assert!(!OfflineUpdateState::Unavailable.has_pending_update());
        // The default must be the safe one: a dialog that has not heard back yet shows no checkbox.
        assert_eq!(
            OfflineUpdateState::default(),
            OfflineUpdateState::Unavailable
        );
    }

    #[test]
    fn actions_use_gnome_softwares_wire_names() {
        assert_eq!(PostUpdateAction::Reboot.as_str(), "reboot");
        assert_eq!(PostUpdateAction::Shutdown.as_str(), "shutdown");
    }
}
