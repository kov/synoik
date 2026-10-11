// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! `org.gnome.DisplayManager` on the system bus: GDM's `Manager`, the part of it our clients call
//! (`docs/fork/greeter.md` §2.1-§2.2).

use zbus::fdo;
use zbus::message::Header;
use zbus::names::BusName;
use zbus::zvariant::OwnedObjectPath;

use crate::channel::{self, Target};

const NAME: &str = "org.gnome.DisplayManager";
const PATH: &str = "/org/gnome/DisplayManager/Manager";

/// Own the name and serve the manager. The returned connection must be kept alive.
pub fn serve() -> anyhow::Result<zbus::blocking::Connection> {
    Ok(zbus::blocking::connection::Builder::system()?
        .name(NAME)?
        .serve_at(PATH, Manager)?
        .build()?)
}

struct Manager;

#[zbus::interface(name = "org.gnome.DisplayManager.Manager")]
impl Manager {
    /// A channel that verifies `username` for the caller's own session — the lock screen's
    /// (§2.2). The caller must be that user, in a session of theirs on seat0.
    async fn open_reauthentication_channel(
        &self,
        username: String,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &zbus::Connection,
    ) -> fdo::Result<String> {
        let sender = header
            .sender()
            .ok_or_else(|| fdo::Error::AccessDenied("no sender".into()))?
            .to_owned();
        let credentials = fdo::DBusProxy::new(connection)
            .await?
            .get_connection_credentials(BusName::Unique(sender))
            .await?;
        let (Some(uid), Some(pid)) = (credentials.unix_user_id(), credentials.process_id()) else {
            return Err(fdo::Error::AccessDenied("no credentials".into()));
        };

        let session = caller_session(connection, pid, uid)
            .await
            .map_err(|err| fdo::Error::AccessDenied(format!("no session for the caller: {err}")))?;
        if session.name != username || session.uid != uid || session.seat != "seat0" {
            return Err(fdo::Error::AccessDenied(
                "a reauthentication channel is for the caller's own session on seat0".into(),
            ));
        }

        let address = channel::open(Target {
            user: username,
            client_uid: uid,
            session: session.id,
        })
        .map_err(|err| fdo::Error::Failed(format!("{err:#}")))?;
        Ok(address)
    }

    /// The session tells the display manager it is up; nothing here depends on it.
    fn register_session(&self) {}

    fn register_display(&self) {}

    #[zbus(property)]
    fn version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_owned()
    }
}

struct Session {
    id: String,
    name: String,
    uid: u32,
    seat: String,
}

/// The session a caller acts for: the one its process is in, else — a compositor run as a user
/// service lives in the user's manager, outside any session — the user's display session, as
/// logind names it (GDM looks the same way, `common/gdm-common.c:812-866`).
async fn caller_session(
    connection: &zbus::Connection,
    pid: u32,
    uid: u32,
) -> zbus::Result<Session> {
    let by_pid = logind(connection, "GetSessionByPID", &(pid,)).await;
    let path = match by_pid {
        Ok(path) => path,
        Err(_) => {
            let user = logind(connection, "GetUser", &(uid,)).await?;
            let proxy = zbus::Proxy::new(
                connection,
                "org.freedesktop.login1",
                user,
                "org.freedesktop.login1.User",
            )
            .await?;
            let (_, display): (String, OwnedObjectPath) = proxy.get_property("Display").await?;
            if display.as_str() == "/" {
                return Err(zbus::Error::Failure(
                    "the user has no display session".into(),
                ));
            }
            display
        }
    };
    session(connection, path).await
}

async fn logind<B>(
    connection: &zbus::Connection,
    method: &str,
    body: &B,
) -> zbus::Result<OwnedObjectPath>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    let reply = connection
        .call_method(
            Some("org.freedesktop.login1"),
            "/org/freedesktop/login1",
            Some("org.freedesktop.login1.Manager"),
            method,
            body,
        )
        .await?;
    reply.body().deserialize()
}

async fn session(connection: &zbus::Connection, path: OwnedObjectPath) -> zbus::Result<Session> {
    let proxy = zbus::Proxy::new(
        connection,
        "org.freedesktop.login1",
        path,
        "org.freedesktop.login1.Session",
    )
    .await?;
    let id: String = proxy.get_property("Id").await?;
    let name: String = proxy.get_property("Name").await?;
    let (uid, _): (u32, OwnedObjectPath) = proxy.get_property("User").await?;
    let (seat, _): (String, OwnedObjectPath) = proxy.get_property("Seat").await?;
    Ok(Session {
        id,
        name,
        uid,
        seat,
    })
}
