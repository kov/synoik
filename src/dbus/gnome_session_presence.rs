//! gnome-session's presence, the shield's idle source (`js/misc/gnomeSession.js`, used from
//! `screenShield.js:78-88`).
//!
//! **The idle threshold is not ours.** `org.gnome.desktop.session idle-delay` belongs to
//! gnome-session, which watches the seat through mutter's `IdleMonitor` — the same one we serve on
//! `org.gnome.Mutter.IdleMonitor` — and publishes the verdict here. The shell only listens.
//!
//! Reimplementing the threshold against our own idle monitor would look like less machinery and be
//! worse: gsd's power plugin, the presence indicator and anything else honouring idleness would go
//! idle at a moment the screen did not, and `idle-delay = 0` (never) would stop meaning never.

use futures_util::StreamExt;

const PRESENCE_PATH: &str = "/org/gnome/SessionManager/Presence";
const PRESENCE_IFACE: &str = "org.gnome.SessionManager.Presence";

/// `GnomeSession.PresenceStatus`. Only `Idle` matters to the shield, but the others have to be
/// distinguishable from it — a change *away* from idle is what un-idles the seat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresenceStatus {
    Available,
    Invisible,
    Busy,
    Idle,
    /// Anything gnome-session grows later. Treated as not-idle, which is the safe reading: a
    /// status we do not understand must not blank the screen.
    Unknown(u32),
}

impl From<u32> for PresenceStatus {
    fn from(value: u32) -> Self {
        match value {
            0 => Self::Available,
            1 => Self::Invisible,
            2 => Self::Busy,
            3 => Self::Idle,
            other => Self::Unknown(other),
        }
    }
}

pub enum PresenceToSynoik {
    StatusChanged(PresenceStatus),
}

/// Watch `StatusChanged`, and report the status gnome-session already holds.
///
/// The initial read matters: the shell can be started (or restarted) into an already-idle session,
/// and a watcher that only ever hears *changes* would sit there with an uncovered screen until the
/// user came back and made it available again.
pub fn start(
    to_niri: calloop::channel::Sender<PresenceToSynoik>,
) -> anyhow::Result<zbus::blocking::Connection> {
    let conn = zbus::blocking::Connection::session()?;

    let async_conn = conn.inner().clone();
    let future = async move {
        let presence = match zbus::Proxy::new(
            &async_conn,
            "org.gnome.SessionManager",
            PRESENCE_PATH,
            PRESENCE_IFACE,
        )
        .await
        {
            Ok(proxy) => proxy,
            Err(err) => {
                warn!("error creating the gnome-session presence proxy: {err:?}");
                return;
            }
        };

        // Subscribe before reading, so a change landing between the two is not lost.
        let mut signals = match presence.receive_signal("StatusChanged").await {
            Ok(stream) => stream,
            Err(err) => {
                warn!("error subscribing to gnome-session StatusChanged: {err:?}");
                return;
            }
        };

        match presence.get_property::<u32>("status").await {
            Ok(status) => {
                let _ = to_niri.send(PresenceToSynoik::StatusChanged(status.into()));
            }
            Err(err) => warn!("error reading the initial gnome-session presence: {err:?}"),
        }

        while let Some(signal) = signals.next().await {
            let Ok(status) = signal.body().deserialize::<u32>() else {
                continue;
            };
            if to_niri
                .send(PresenceToSynoik::StatusChanged(status.into()))
                .is_err()
            {
                break;
            }
        }
    };

    conn.inner()
        .executor()
        .spawn(future, "monitor gnome-session presence")
        .detach();

    Ok(conn)
}
