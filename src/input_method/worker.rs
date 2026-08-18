// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The thread that owns the IBus connection.
//!
//! Everything here runs off the compositor thread. `ProcessKeyEvent` is a D-Bus round trip *per
//! keystroke* (`inputMethod.js:344`), so doing it inline would put the frame loop behind an IPC
//! call for every key.
//!
//! The loop is deliberately **sequential**: one request is awaited to completion before the next
//! is taken. That is what makes key verdicts come back in the order the keys were typed, which
//! mutter also relies on ("we rely on the IM implementation to notify back of key events in the
//! exact same order they were given", `clutter-input-method.c:398-400`).

use std::time::{Duration, Instant};

use futures_util::future;
use futures_util::stream::{self, LocalBoxStream, StreamExt};
use zbus::zvariant::Value;

use super::{ImRequest, ImUpdate};
use crate::dbus::ibus::{self, ImEvent, PreeditMode};

/// How long to wait before redialing a daemon that was not there, doubling up to [`RETRY_MAX`].
///
/// An absent `ibus-daemon` is a perfectly normal configuration, so this settles into a slow
/// heartbeat rather than giving up: a daemon started later must still be picked up.
const RETRY_START: Duration = Duration::from_millis(500);
const RETRY_MAX: Duration = Duration::from_secs(30);

/// The systemd user unit that owns `ibus-daemon` in a GNOME session.
///
/// gnome-shell asks whether this unit exists and only spawns the daemon itself when it does not
/// (`ibusManager.js:83-102`) — the unit is the supported path, and a daemon started under it
/// outlives us instead of dying in the compositor's cgroup.
const IBUS_UNIT: &str = "org.freedesktop.IBus.session.GNOME.service";

/// The floor between two attempts to revive the daemon. A daemon that keeps dying must not be
/// fought: one start per interval, no matter how fast the redial loop spins.
const REVIVE_INTERVAL: Duration = Duration::from_secs(30);

/// Start the worker. Returns immediately; the thread lives as long as the request channel does.
///
/// `revive_daemon` belongs to the **session instance** only: reviving `ibus-daemon` is a
/// session-wide side effect, and a nested or headless instance must never reach out and start
/// daemons on the developer's real seat.
pub fn spawn(
    requests: async_channel::Receiver<ImRequest>,
    to_compositor: calloop::channel::Sender<ImUpdate>,
    revive_daemon: bool,
) {
    let builder = std::thread::Builder::new().name("input method".to_owned());
    let run = move || async_io::block_on(run(requests, to_compositor, revive_daemon));
    if let Err(err) = builder.spawn(run) {
        tracing::warn!("could not start the input method thread: {err:?}");
    }
}

async fn run(
    requests: async_channel::Receiver<ImRequest>,
    to_compositor: calloop::channel::Sender<ImUpdate>,
    revive_daemon: bool,
) {
    let mut backoff = RETRY_START;
    let mut last_revive: Option<Instant> = None;
    loop {
        match session(&requests, &to_compositor).await {
            // The request channel closed: the compositor is going away.
            Ok(()) => return,
            Err(err) => {
                tracing::debug!("input method connection ended: {err:?}");
                if to_compositor.send(ImUpdate::Connected(false)).is_err() {
                    return;
                }
            }
        }

        // No daemon means no dead keys and no Compose *for every client on the seat*: we
        // advertise `zwp_text_input_v3` unconditionally, which is what makes GTK drop its own
        // compose table (see the note atop `crate::dbus::ibus`). So an absent daemon is not
        // something to wait out — it is something to fix. Killing a wedged `ibus-daemon` is the
        // standing workaround for it eating a core, and `Restart=on-abnormal` in the unit does
        // not cover a clean SIGTERM, so a killed daemon stays dead until something asks for it.
        let stale = last_revive.is_none_or(|at| at.elapsed() >= REVIVE_INTERVAL);
        if revive_daemon && stale {
            last_revive = Some(Instant::now());
            if revive().await {
                // The daemon is coming up now; redial promptly rather than sitting out the
                // slow heartbeat this loop has settled into.
                backoff = RETRY_START;
            }
        }

        async_io::Timer::after(backoff).await;
        backoff = (backoff * 2).min(RETRY_MAX);
    }
}

/// Bring `ibus-daemon` back, the way gnome-shell would. `true` if something was actually started.
///
/// Unit first, spawn second — the same order and the same argv as `ibusManager.js:111-122`. The
/// unit is `Type=dbus`, so systemd returning from `StartUnit` does not mean the daemon has taken
/// its name yet; the redial loop is what waits for that.
async fn revive() -> bool {
    match start_unit().await {
        Ok(()) => {
            tracing::info!("started {IBUS_UNIT} to bring the input method back");
            return true;
        }
        // Every distro that ships the unit is served above. Anything else — no unit, no systemd,
        // no session bus — falls through to gnome-shell's own spawn.
        Err(err) => tracing::debug!("could not start {IBUS_UNIT}: {err:?}"),
    }

    // `--panel disable`, exactly as gnome-shell spawns it (`ibusManager.js:112`): the panel is
    // the candidate popup and the shell draws that itself.
    match std::process::Command::new("ibus-daemon")
        .args(["--panel", "disable"])
        .spawn()
    {
        Ok(mut child) => {
            tracing::info!("spawned ibus-daemon to bring the input method back");
            // Reaped off-thread: a child left unwaited is a zombie for the life of the session,
            // and this thread is busy being an event loop.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            true
        }
        Err(err) => {
            tracing::warn!("could not start an input method daemon: {err}");
            false
        }
    }
}

/// `StartUnit` on the systemd *user* manager. Fails when the unit is not installed, which is the
/// case gnome-shell's spawn path exists for.
async fn start_unit() -> anyhow::Result<()> {
    let conn = zbus::Connection::session().await?;
    let manager = zbus::Proxy::new(
        &conn,
        "org.freedesktop.systemd1",
        "/org/freedesktop/systemd1",
        "org.freedesktop.systemd1.Manager",
    )
    .await?;
    // "replace" is systemd's ordinary queueing mode, and what `systemctl start` uses.
    let _job: zbus::zvariant::OwnedObjectPath =
        manager.call("StartUnit", &(IBUS_UNIT, "replace")).await?;
    Ok(())
}

/// One connection's lifetime. `Ok(())` means a clean shutdown; any error is a reason to redial.
async fn session(
    requests: &async_channel::Receiver<ImRequest>,
    to_compositor: &calloop::channel::Sender<ImUpdate>,
) -> anyhow::Result<()> {
    let (_conn, bus, ctx) = ibus::connect().await?;
    tracing::info!("connected to ibus");

    // Subscribed before the connection is announced, so engine output cannot land in a gap where
    // nobody is listening. Each stream is mapped to `ImEvent` at creation, which is what lets
    // seven differently-typed signal streams merge into one.
    let streams: Vec<LocalBoxStream<'_, ImEvent>> = vec![
        ctx.receive_commit_text()
            .await?
            .filter_map(|signal| async move {
                let args = signal.args().ok()?;
                Some(ImEvent::Commit(ibus::ibus_text(&args.text)?))
            })
            .boxed_local(),
        ctx.receive_update_preedit_text_with_mode()
            .await?
            .filter_map(|signal| async move {
                let args = signal.args().ok()?;
                Some(ImEvent::Preedit {
                    text: ibus::ibus_text(&args.text),
                    cursor: args.cursor_pos,
                    visible: args.visible,
                    mode: PreeditMode::from_wire(args.mode),
                })
            })
            .boxed_local(),
        ctx.receive_show_preedit_text()
            .await?
            .map(|_| ImEvent::ShowPreedit)
            .boxed_local(),
        ctx.receive_hide_preedit_text()
            .await?
            .map(|_| ImEvent::HidePreedit)
            .boxed_local(),
        ctx.receive_forward_key_event()
            .await?
            .filter_map(|signal| async move {
                let args = signal.args().ok()?;
                Some(ImEvent::ForwardKey {
                    keyval: args.keyval,
                    keycode: args.keycode,
                    state: args.state,
                    press: args.state & ibus::RELEASE_MASK == 0,
                })
            })
            .boxed_local(),
        ctx.receive_delete_surrounding_text()
            .await?
            .filter_map(|signal| async move {
                let args = signal.args().ok()?;
                Some(ImEvent::DeleteSurrounding {
                    offset: args.offset,
                    n_chars: args.n_chars,
                })
            })
            .boxed_local(),
        ctx.receive_require_surrounding_text()
            .await?
            .map(|_| ImEvent::RequireSurrounding)
            .boxed_local(),
    ];
    let mut events = stream::select_all(streams);

    if to_compositor.send(ImUpdate::Connected(true)).is_err() {
        return Ok(());
    }

    let mut caps = ibus::CAP_PREEDIT_TEXT | ibus::CAP_FOCUS;

    // Two pumps, concurrent on this one thread. They must not be folded back into a single
    // `select` over both: awaiting a request there stops polling `events`, and zbus caps each
    // incoming queue at 64 messages — "when the queue is full, no more messages can be received"
    // (zbus `Connection` docs), **method replies included**. A left-biased `select` under
    // sustained typing starves the signal streams, the queue fills, and the `ProcessKeyEvent`
    // reply we are awaiting can never arrive. The connection wedges for good: no reply, and no
    // disconnect either, so even restarting ibus-daemon does not recover it. That cost a live
    // session 18 minutes of one-second-per-keystroke input on 2026-08-15.
    //
    // Requests stay **strictly sequential** inside their own pump — that is the key-ordering
    // guarantee in the module note, and it is unaffected by draining signals concurrently.
    let events_pump = async {
        while let Some(event) = events.next().await {
            if to_compositor.send(ImUpdate::Event(event)).is_err() {
                return Ok(());
            }
        }
        // Every signal stream ended at once, which only happens if the connection died.
        anyhow::bail!("ibus signal streams ended")
    };

    let requests_pump = async {
        while let Ok(request) = requests.recv().await {
            handle_request(&bus, &ctx, request, &mut caps, to_compositor).await?;
        }
        // The request channel closed: the compositor is going away.
        Ok(())
    };

    let events_pump = std::pin::pin!(events_pump);
    let requests_pump = std::pin::pin!(requests_pump);
    match future::select(events_pump, requests_pump).await {
        future::Either::Left((result, _)) | future::Either::Right((result, _)) => result,
    }
}

async fn handle_request(
    bus: &ibus::IBusProxy<'_>,
    ctx: &ibus::InputContextProxy<'_>,
    request: ImRequest,
    caps: &mut u32,
    to_compositor: &calloop::channel::Sender<ImUpdate>,
) -> anyhow::Result<()> {
    match request {
        ImRequest::FocusIn => ctx.focus_in().await?,
        ImRequest::FocusOut => ctx.focus_out().await?,
        ImRequest::Reset => ctx.reset().await?,
        // A *property* whose type is the tuple `(uu)`, so it is set with one struct value
        // rather than two scalars.
        ImRequest::ContentType { purpose, hints } => ctx.set_content_type((purpose, hints)).await?,
        ImRequest::SetEngine(engine) => {
            // The one call gnome-shell puts a timeout on (`ibusManager.js:59-61`): an engine that
            // will not start must not wedge every later request behind it.
            let call = std::pin::pin!(bus.set_global_engine(&engine));
            let deadline = std::pin::pin!(async_io::Timer::after(ibus::ENGINE_ACTIVATION_TIMEOUT));
            match future::select(call, deadline).await {
                future::Either::Left((Ok(()), _)) => {}
                future::Either::Left((Err(err), _)) => {
                    tracing::warn!("could not select engine {engine}: {err}");
                }
                future::Either::Right(_) => {
                    tracing::warn!("engine {engine} did not activate in time");
                }
            }
        }
        ImRequest::Surrounding {
            text,
            cursor,
            anchor,
        } => {
            // The engine may only ask for surrounding text once we claim the capability, and we
            // can only claim it once a client has actually given us some.
            if *caps & ibus::CAP_SURROUNDING_TEXT == 0 {
                *caps |= ibus::CAP_SURROUNDING_TEXT;
                ctx.set_capabilities(*caps).await?;
            }
            // Crossing the boundary the other way: the client counts in bytes, IBus in characters.
            let value = Value::from(ibus::make_ibus_text(&text));
            ctx.set_surrounding_text(
                &value,
                byte_to_char(&text, cursor),
                byte_to_char(&text, anchor),
            )
            .await?;
        }
        ImRequest::ProcessKey {
            id,
            keysym,
            keycode,
            state,
        } => {
            let filtered = match ctx.process_key_event(keysym, keycode, state).await {
                Ok(filtered) => filtered,
                Err(err) => {
                    // Fail open: a key nobody could rule on belongs to the client. The compositor
                    // is holding it and will not release it until this answer arrives.
                    //
                    // `warn!`, not `debug!`: this is the one line that separates "the engine said
                    // no" from "the engine never answered", and the sessions where that matters
                    // run a release build, where `debug!` is the floor and `trace!` does not exist.
                    tracing::warn!("process_key_event failed: {err}");
                    false
                }
            };
            if to_compositor
                .send(ImUpdate::KeyResult { id, filtered })
                .is_err()
            {
                anyhow::bail!("compositor channel closed");
            }
        }
    }
    Ok(())
}

/// Character offset of a byte offset — the mirror of [`super::char_to_byte`].
fn byte_to_char(text: &str, byte: u32) -> u32 {
    let byte = (byte as usize).min(text.len());
    // Count only characters that end at or before the offset, so an offset landing *inside* a
    // multi-byte character rounds down to that character's start. Slicing would panic on one and
    // counting starts would round it up — naming a caret position one character too far right.
    text.char_indices()
        .take_while(|(i, c)| i + c.len_utf8() <= byte)
        .count()
        .try_into()
        .unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_offsets_become_character_offsets() {
        // The mirror of `char_to_byte`: a caret at the end of "héllo" is byte 6, character 5.
        assert_eq!(byte_to_char("héllo", 6), 5);
        assert_eq!(byte_to_char("héllo", 3), 2);
        assert_eq!(byte_to_char("héllo", 1), 1);
        assert_eq!(byte_to_char("héllo", 0), 0);
        // Past the end clamps, and an offset inside the é does not panic.
        assert_eq!(byte_to_char("héllo", 99), 5);
        assert_eq!(byte_to_char("héllo", 2), 1);
    }
}
