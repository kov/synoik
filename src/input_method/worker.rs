// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The thread that owns the IBus connection.
//!
//! Everything here runs off the compositor thread. `ProcessKeyEvent` is a D-Bus round trip *per
//! keystroke* (`inputMethod.js:344`), so doing it inline would put the frame loop behind an IPC
//! call for every key.
//!
//! Requests are issued **strictly in order**, one awaited to completion before the next is sent.
//! That is what makes key verdicts come back in the order the keys were typed, which mutter also
//! relies on ("we rely on the IM implementation to notify back of key events in the exact same
//! order they were given", `clutter-input-method.c:398-400`).
//!
//! What is *not* guaranteed is that every queued request is issued. Whatever piled up while a call
//! was in flight is collapsed by [`coalesce`] first, because the queue is a cache of intent rather
//! than a log: only the latest value of each piece of state can still be acted on, and a keystroke
//! the compositor has already given up on must not be acted on at all. Survivors keep their
//! relative order, so the daemon sees a prefix-equivalent of what a daemon that kept up would have.

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
        UnitStart::Started => {
            tracing::info!("started {IBUS_UNIT} to bring the input method back");
            return true;
        }
        // The unit exists and systemd said no — masked, or past its start limit. Both are an
        // administrator's or systemd's decision about this seat's input method, and spawning the
        // daemon behind them would override it. gnome-shell has the same rule from the other
        // side: where the unit exists it *never* spawns the daemon itself.
        UnitStart::Refused => return false,
        // No unit, no systemd, no session bus: the distro gnome-shell's own spawn exists for.
        UnitStart::NotInstalled => {}
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
            // A seat with no ibus installed at all is a perfectly ordinary configuration, and
            // this loop runs forever: say it once at `warn`, then stop shouting about it.
            static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if SAID.swap(true, std::sync::atomic::Ordering::Relaxed) {
                tracing::debug!("could not start an input method daemon: {err}");
            } else {
                tracing::warn!("could not start an input method daemon: {err}");
            }
            false
        }
    }
}

/// What `StartUnit` said, reduced to the three answers that lead anywhere different.
enum UnitStart {
    /// systemd took the job.
    Started,
    /// There is no such unit here, or no systemd to ask.
    NotInstalled,
    /// The unit exists and systemd declined to start it.
    Refused,
}

/// `StartUnit` on the systemd *user* manager.
async fn start_unit() -> UnitStart {
    let call = async {
        let conn = zbus::Connection::session().await?;
        let manager = zbus::Proxy::new(
            &conn,
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.systemd1.Manager",
        )
        .await?;
        // "replace" is systemd's ordinary queueing mode, and what `systemctl start` uses.
        let job: zbus::zvariant::OwnedObjectPath =
            manager.call("StartUnit", &(IBUS_UNIT, "replace")).await?;
        Ok::<_, zbus::Error>(job)
    };

    let err = match call.await {
        Ok(_job) => return UnitStart::Started,
        Err(err) => err,
    };
    tracing::debug!("could not start {IBUS_UNIT}: {err:?}");

    match &err {
        // A distro that does not ship the unit, and a session where systemd is not on the bus
        // at all: both are "ask someone else", which is the spawn.
        zbus::Error::MethodError(name, _, _) => match name.as_str() {
            "org.freedesktop.systemd1.NoSuchUnit" | "org.freedesktop.DBus.Error.ServiceUnknown" => {
                UnitStart::NotInstalled
            }
            _ => UnitStart::Refused,
        },
        // No session bus to ask.
        _ => UnitStart::NotInstalled,
    }
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
        loop {
            let Ok(first) = requests.recv().await else {
                // The request channel closed: the compositor is going away.
                return Ok(());
            };
            // Take whatever else piled up while the previous call was in flight. `try_recv` never
            // waits, so a daemon that is keeping up sees batches of one and this costs nothing;
            // only a daemon we are waiting on can produce a batch worth collapsing.
            let mut batch = vec![first];
            while let Ok(request) = requests.try_recv() {
                batch.push(request);
            }
            for request in coalesce(batch, Instant::now()) {
                handle_request(&bus, &ctx, request, &mut caps, to_compositor).await?;
            }
        }
    };

    let events_pump = std::pin::pin!(events_pump);
    let requests_pump = std::pin::pin!(requests_pump);
    match future::select(events_pump, requests_pump).await {
        future::Either::Left((result, _)) | future::Either::Right((result, _)) => result,
    }
}

/// The classes of request that are a *variable*, not an event: only the latest value of each can
/// still be acted on, however many were queued.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StateClass {
    /// `FocusIn` and `FocusOut` are two values of one variable, not two independent commands.
    Focus,
    ContentType,
    Engine,
    Surrounding,
    /// A command rather than a variable, but a repeated one is idempotent: dropping every `Reset`
    /// but the last is safe, dropping the last is not — nothing else clears a composition.
    Reset,
}

const STATE_CLASSES: usize = 5;

fn state_class(request: &ImRequest) -> Option<StateClass> {
    match request {
        ImRequest::FocusIn | ImRequest::FocusOut => Some(StateClass::Focus),
        ImRequest::ContentType { .. } => Some(StateClass::ContentType),
        ImRequest::SetEngine(_) => Some(StateClass::Engine),
        ImRequest::Surrounding { .. } => Some(StateClass::Surrounding),
        ImRequest::Reset => Some(StateClass::Reset),
        ImRequest::ProcessKey { .. } => None,
    }
}

/// Collapse a batch of queued requests to what the daemon can still usefully act on.
///
/// Two rules, and neither ever reorders anything: **superseded state is deleted in place** (only
/// the last request of each [`StateClass`] survives, at its original position), and **a keystroke
/// nobody is waiting for is dropped**.
///
/// A key is not worth asking about once either of these holds:
///
/// * It was queued before the last focus transition in the batch. It belonged to the entry that
///   lost the focus, and `sync_im_focus` already flushed it to that entry.
/// * It is older than [`super::KEY_TIMEOUT`], so `expire_im_keys_at` has already delivered it.
///
/// Both are *hazards*, not merely waste: a verdict is ignored for a key we no longer hold, but the
/// `commit` an engine emits alongside one is not, and it would land as a character the user never
/// typed a second time. Dropped **silently** — synthesizing a `KeyResult` to keep the books
/// straight would read as the engine answering, clearing `unanswered` and cancelling
/// `is_unresponsive` on the strength of a reply that never came.
fn coalesce(mut batch: Vec<ImRequest>, now: Instant) -> Vec<ImRequest> {
    let mut last = [None; STATE_CLASSES];
    for (index, request) in batch.iter().enumerate() {
        if let Some(class) = state_class(request) {
            last[class as usize] = Some(index);
        }
    }
    let last_focus = last[StateClass::Focus as usize];

    let mut next = 0;
    batch.retain(|request| {
        let index = next;
        next += 1;
        match request {
            ImRequest::ProcessKey { queued_at, .. } => {
                let outlived_its_focus = last_focus.is_some_and(|focus| index < focus);
                let expired = now.saturating_duration_since(*queued_at) >= super::KEY_TIMEOUT;
                !outlived_its_focus && !expired
            }
            // `state_class` is exhaustive over the rest, so the fallback is unreachable.
            other => state_class(other).is_none_or(|class| last[class as usize] == Some(index)),
        }
    });
    batch
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
            // Staleness was already ruled on by `coalesce`; anything that reaches here is live.
            queued_at: _,
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

    /// A `ProcessKey` stamped `age` ago.
    fn key(id: u64, age: Duration) -> ImRequest {
        ImRequest::ProcessKey {
            id,
            keysym: 0x61,
            keycode: 30,
            state: 0,
            queued_at: Instant::now() - age,
        }
    }

    fn content(purpose: u32) -> ImRequest {
        ImRequest::ContentType { purpose, hints: 0 }
    }

    #[test]
    fn a_batch_of_one_is_left_alone() {
        // The case that actually happens on a healthy session, and the one that must cost nothing.
        let batch = vec![ImRequest::FocusIn];
        assert_eq!(coalesce(batch, Instant::now()), vec![ImRequest::FocusIn]);
    }

    #[test]
    fn a_focus_flip_flop_collapses_to_where_it_ended_up() {
        // 90 minutes of a client toggling its text input is worth one focus request by the time
        // anyone reads it. The engine cannot act on the intermediate states — they are gone.
        let batch = vec![
            ImRequest::FocusIn,
            content(1),
            ImRequest::FocusOut,
            content(0),
            ImRequest::FocusIn,
            content(2),
        ];
        assert_eq!(
            coalesce(batch, Instant::now()),
            vec![ImRequest::FocusIn, content(2)]
        );
    }

    #[test]
    fn survivors_keep_the_order_they_were_queued_in() {
        // Deleted in place, never regrouped: the daemon sees what a daemon that kept up would.
        let batch = vec![
            content(1),
            ImRequest::SetEngine("xkb:us::eng".to_owned()),
            ImRequest::FocusIn,
            ImRequest::SetEngine("xkb:us:intl:eng".to_owned()),
        ];
        assert_eq!(
            coalesce(batch, Instant::now()),
            vec![
                content(1),
                ImRequest::FocusIn,
                ImRequest::SetEngine("xkb:us:intl:eng".to_owned()),
            ]
        );
    }

    #[test]
    fn a_key_that_outlived_its_focus_is_not_offered_to_the_engine() {
        // The key was flushed to the entry that lost the focus. Asking anyway invites a `commit`
        // for it, which would type the character a second time — into whatever has the focus now.
        let survivor = key(1, Duration::ZERO);
        let batch = vec![
            key(0, Duration::ZERO),
            ImRequest::FocusOut,
            ImRequest::FocusIn,
            survivor.clone(),
        ];
        assert_eq!(
            coalesce(batch, Instant::now()),
            vec![ImRequest::FocusIn, survivor]
        );
    }

    #[test]
    fn a_key_the_compositor_already_gave_up_on_is_dropped() {
        // Same hazard from the other direction: `expire_im_keys_at` delivered this one a second
        // ago. A batch that is minutes old is entirely made of these.
        let stale = key(0, super::super::KEY_TIMEOUT * 2);
        let fresh = key(1, Duration::ZERO);
        assert_eq!(
            coalesce(vec![stale, fresh.clone()], Instant::now()),
            vec![fresh]
        );
    }

    #[test]
    fn keys_are_never_collapsed_into_each_other() {
        // Every keystroke is its own event: three fresh keys are three requests, in order.
        let batch = vec![
            key(0, Duration::ZERO),
            key(1, Duration::ZERO),
            key(2, Duration::ZERO),
        ];
        assert_eq!(coalesce(batch.clone(), Instant::now()), batch);
    }

    #[test]
    fn the_last_reset_survives() {
        // Repeated resets are idempotent, but losing the last one leaves a composition standing.
        let batch = vec![ImRequest::Reset, ImRequest::FocusIn, ImRequest::Reset];
        assert_eq!(
            coalesce(batch, Instant::now()),
            vec![ImRequest::FocusIn, ImRequest::Reset]
        );
    }

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
