// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Going to the login screen — the "Log in as another user" button's action.
//!
//! GNOME calls `Gdm.goto_login_session_sync` (`unlockDialog.js:901-905`), a libgdm helper that is
//! not a single D-Bus call but a small algorithm over logind and GDM
//! (`libgdm/gdm-user-switching.c:184-236`):
//!
//! 1. find our own session's **seat**;
//! 2. look for a greeter already on that seat — a session whose `Class` is `greeter`, whose `State`
//!    is not `closing`, and whose `Service` is `gdm-launch-environment` — and if one is there,
//!    `ActivateSessionOnSeat` it;
//! 3. otherwise, and **only on `seat0`**, ask GDM's `LocalDisplayFactory` for a
//!    `CreateTransientDisplay`.
//!
//! Reusing an existing greeter rather than always creating one is the whole point of step 2: hit
//! the button twice and you get one login screen, not two.
//!
//! # Why this is not on the main loop
//!
//! Every call here is a system-bus round trip, and step 2 is one *per session on the seat*. The
//! compositor's event loop is also its render loop, so doing this inline would stall the frame
//! that is drawing the button being pressed. It runs as a task on the same connection's executor
//! as the other system-bus clients, and nothing waits for it: the outcome is a session switch,
//! which is not a thing we can render.

use zbus::zvariant::OwnedObjectPath;

const LOGIN1: &str = "org.freedesktop.login1";
const LOGIN1_PATH: &str = "/org/freedesktop/login1";
const LOGIN1_MANAGER: &str = "org.freedesktop.login1.Manager";
const LOGIN1_SEAT: &str = "org.freedesktop.login1.Seat";
const LOGIN1_SESSION: &str = "org.freedesktop.login1.Session";

const GDM: &str = "org.gnome.DisplayManager";
const GDM_FACTORY_PATH: &str = "/org/gnome/DisplayManager/LocalDisplayFactory";
const GDM_FACTORY: &str = "org.gnome.DisplayManager.LocalDisplayFactory";

/// The greeter's `Service`, which is what distinguishes GDM's login screen from any other session
/// that calls itself a greeter (`gdm-user-switching.c:151-157`).
const GREETER_SERVICE: &str = "gdm-launch-environment";
/// The seat GDM is willing to create a transient display on (`:230`).
const SEAT0: &str = "seat0";

/// Our seat's id, as logind reports it for our own session.
///
/// Also the [`can_switch`] test: libaccountsservice's `act_user_manager_can_switch` is a seat
/// lookup plus `sd_seat_can_multi_session`, and that second half no longer means anything —
/// systemd dropped it from `sd-login(3)` entirely, every seat being multi-session now. So "we know
/// which seat we are on" is the whole of it.
pub async fn seat_id(conn: &zbus::Connection) -> Option<String> {
    let session = crate::dbus::freedesktop_login1::session_path()?;
    let proxy = zbus::Proxy::new(conn, LOGIN1, session.clone(), LOGIN1_SESSION)
        .await
        .ok()?;
    let (id, _path): (String, OwnedObjectPath) = proxy.get_property("Seat").await.ok()?;
    // logind answers with an empty id for a session attached to no seat, which is a session that
    // cannot switch to anything.
    (!id.is_empty()).then_some(id)
}

/// Switch to the login screen, reusing a greeter already on our seat if there is one.
///
/// Errors are logged, not returned: there is no caller left to tell. The button is already gone by
/// the time this runs (GNOME cancels the prompt in the same handler), and the observable outcome
/// either way is "the screen did or did not change".
pub async fn goto_login_session(conn: &zbus::Connection) {
    let Some(seat) = seat_id(conn).await else {
        warn!("cannot switch users: logind reports no seat for our session");
        return;
    };

    if let Some(greeter) = find_greeter(conn, &seat).await {
        match activate(conn, &seat, &greeter).await {
            Ok(()) => return,
            // Fall through to a transient display: a greeter that will not activate (it went away
            // between the listing and the call) is no different from not having found one.
            Err(err) => warn!("could not activate the greeter on {seat}: {err:?}"),
        }
    }

    if seat != SEAT0 {
        warn!("cannot switch users: no greeter on {seat}, and only {SEAT0} takes a new one");
        return;
    }
    if let Err(err) = create_transient_display(conn).await {
        warn!("could not ask the display manager for a login screen: {err:?}");
    }
}

/// The path of a live GDM greeter on `seat`, if there is one.
async fn find_greeter(conn: &zbus::Connection, seat: &str) -> Option<OwnedObjectPath> {
    let seat_path = format!("/org/freedesktop/login1/seat/{}", escape_id(seat));
    let proxy = zbus::Proxy::new(conn, LOGIN1, seat_path, LOGIN1_SEAT)
        .await
        .ok()?;
    let sessions: Vec<(String, OwnedObjectPath)> = proxy.get_property("Sessions").await.ok()?;

    for (_id, path) in sessions {
        let session = match zbus::Proxy::new(conn, LOGIN1, path.clone(), LOGIN1_SESSION).await {
            Ok(proxy) => proxy,
            Err(_) => continue,
        };
        let class = session.get_property::<String>("Class").await;
        if class.as_deref() != Ok("greeter") {
            continue;
        }
        // A greeter on its way out is not one to send anybody to.
        if session.get_property::<String>("State").await.as_deref() == Ok("closing") {
            continue;
        }
        if session.get_property::<String>("Service").await.as_deref() == Ok(GREETER_SERVICE) {
            return Some(path);
        }
    }
    None
}

async fn activate(
    conn: &zbus::Connection,
    seat: &str,
    session: &OwnedObjectPath,
) -> zbus::Result<()> {
    // `ActivateSessionOnSeat` takes the session *id*, not its path — the last component of the
    // path, unescaped. Ask the session for it rather than parsing, so systemd's escaping stays
    // systemd's business.
    let proxy = zbus::Proxy::new(conn, LOGIN1, session.clone(), LOGIN1_SESSION).await?;
    let id: String = proxy.get_property("Id").await?;

    let manager = zbus::Proxy::new(conn, LOGIN1, LOGIN1_PATH, LOGIN1_MANAGER).await?;
    manager
        .call_method("ActivateSessionOnSeat", &(id.as_str(), seat))
        .await?;
    Ok(())
}

async fn create_transient_display(conn: &zbus::Connection) -> zbus::Result<()> {
    let factory = zbus::Proxy::new(conn, GDM, GDM_FACTORY_PATH, GDM_FACTORY).await?;
    factory.call_method("CreateTransientDisplay", &()).await?;
    Ok(())
}

/// systemd's `bus_label_escape` (`src/libsystemd/sd-bus/bus-label.c`), which is how logind turns
/// an id into an object-path component.
///
/// Kept verbatim: **letters** always, **digits** only when they are not first (a path component
/// cannot begin with one), and everything else — including `_` itself — becomes `_<hex>`. The rule
/// is visible in any `loginctl` listing: session `116` is exported at `.../session/_3116`.
fn escape_id(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for (i, b) in id.bytes().enumerate() {
        if b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit()) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("_{b:02x}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The seat path we build must be the one logind exports.
    ///
    /// `seat0` is the only id most machines ever have and it needs no escaping at all, so a broken
    /// escape would look perfect on every desktop and fail on exactly the multi-seat setups the
    /// button exists for. The escaped forms are checked against systemd's rule rather than against
    /// a machine we happen to be on.
    #[test]
    fn seat_ids_escape_the_way_systemd_does() {
        assert_eq!(escape_id("seat0"), "seat0");
        assert_eq!(escape_id("seat-foo"), "seat_2dfoo");
        // A leading digit cannot begin an object-path component.
        assert_eq!(escape_id("0seat"), "_30seat");
        // Underscore is not in systemd's keep set either, easy as it is to assume it is.
        assert_eq!(escape_id("seat_1"), "seat_5f1");
        // The rule as `loginctl` shows it: session 116 lives at `.../session/_3116`.
        assert_eq!(escape_id("116"), "_3116");
    }
}
