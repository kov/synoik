// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! Session-bus **client** of `org.gnome.Shell.CalendarServer` — the
//! evolution-data-server-backed service that feeds the dateMenu Events section.
//! We consume it (we do NOT own the name); it is D-Bus-activatable, so our first
//! `SetTimeRange` auto-starts `gnome-shell-calendar-server`. Ports gnome-shell's
//! `DBusEventSource` (`js/ui/calendar.js:207-405`).
//!
//! Bidirectional, like the notifications server: the compositor pushes the day
//! range it wants ([`SynoikToCalendar::SetRange`], deduped here like
//! `requestRange`/`_datesEqual`, `js/ui/calendar.js:369-377`) and we forward the
//! server's `EventsAddedOrUpdated` / `EventsRemoved` / `ClientDisappeared`
//! signals plus `HasCalendars` back over a calloop channel
//! ([`CalendarToSynoik`]). Name-owner appearance/vanish re-arms the range with a
//! forced reload / clears the store (`_onNameAppeared`/`_onNameVanished`,
//! `js/ui/calendar.js:294-303`).
//!
//! Process-isolation seam: this task owns its bus connection and does ALL
//! untrusted parsing — event summaries are flattened + bounded here; ids are
//! kept verbatim (their `\n` is structural recurrence, see [`crate::calendar_events`]).
//! The model only ever receives typed, bounded [`CalendarEvent`]s.
//!
//! Two accepted transients (both self-heal, matching GNOME's own edges):
//! - The wake sources are merged with `select_all`, which has no cross-stream ordering, so a fresh
//!   `EventsAddedOrUpdated` could be applied just before an `OwnerAppeared` that resets it — but
//!   owner-appearance re-requests the range with `force_reload`, so the server re-emits and the
//!   store re-fills.
//! - If the initial `HasCalendars` read fails on a healthy long-lived server, the section stays
//!   hidden until the next property/owner change — the same window GNOME has when `init_async`
//!   fails non-timeout (`js/ui/calendar.js:227-237`).

use std::collections::HashMap;

use futures_util::StreamExt;
use zbus::zvariant;

use crate::calendar_events::{CalendarEvent, CalendarToSynoik, SynoikToCalendar};

const BUS_NAME: &str = "org.gnome.Shell.CalendarServer";

/// One raw wire event: `(s id, s summary, x start, x end, a{sv} extra)`
/// (`data/dbus-interfaces/org.gnome.Shell.CalendarServer.xml`). The `extra`
/// dict is unused (GNOME ignores it too).
type WireEvent = (
    String,
    String,
    i64,
    i64,
    HashMap<String, zvariant::OwnedValue>,
);

/// Sanitized-summary cap (bytes). Bounds untrusted calendar text.
const MAX_SUMMARY_BYTES: usize = 1024;
/// Event-id cap (bytes). An over-long id is dropped whole — a truncated id would
/// corrupt the prefix-based recurrence dedup.
const MAX_ID_BYTES: usize = 4096;
/// Per-batch event cap. GNOME doesn't bound this; the seam rule does.
const MAX_EVENTS: usize = 4096;

#[zbus::proxy(
    interface = "org.gnome.Shell.CalendarServer",
    default_service = "org.gnome.Shell.CalendarServer",
    default_path = "/org/gnome/Shell/CalendarServer"
)]
trait CalendarServer {
    /// Request events in `[since, until)` (Unix seconds), reloading if forced.
    fn set_time_range(&self, since: i64, until: i64, force_reload: bool) -> zbus::Result<()>;

    /// Whether any calendars are configured (gates section visibility).
    #[zbus(property)]
    fn has_calendars(&self) -> zbus::Result<bool>;

    #[zbus(signal)]
    fn events_added_or_updated(&self, events: Vec<WireEvent>) -> zbus::Result<()>;

    #[zbus(signal)]
    fn events_removed(&self, ids: Vec<String>) -> zbus::Result<()>;

    #[zbus(signal)]
    fn client_disappeared(&self, source_uid: String) -> zbus::Result<()>;
}

/// A merged wake source inside the watcher task.
enum Ev {
    Request(SynoikToCalendar),
    Added(Vec<CalendarEvent>),
    Removed(Vec<String>),
    Gone(String),
    HasCalChanged,
    /// `true` = the server gained an owner, `false` = it vanished.
    Owner(bool),
}

pub fn start(
    to_niri: calloop::channel::Sender<CalendarToSynoik>,
    from_niri: async_channel::Receiver<SynoikToCalendar>,
) -> anyhow::Result<zbus::blocking::Connection> {
    let conn = zbus::blocking::Connection::session()?;
    let async_conn = conn.inner().clone();

    let future = async move {
        let proxy = match CalendarServerProxy::new(&async_conn).await {
            Ok(proxy) => proxy,
            Err(err) => {
                warn!("error creating CalendarServer proxy: {err:?}");
                return;
            }
        };

        // Install the signal streams up front, before any activation — the
        // service may appear later, and we finish loading on its owner change
        // (`_initProxy`, `js/ui/calendar.js:220-262`).
        let added = match proxy.receive_events_added_or_updated().await {
            Ok(s) => s,
            Err(err) => {
                warn!("error subscribing to EventsAddedOrUpdated: {err:?}");
                return;
            }
        };
        let removed = match proxy.receive_events_removed().await {
            Ok(s) => s,
            Err(err) => {
                warn!("error subscribing to EventsRemoved: {err:?}");
                return;
            }
        };
        let gone = match proxy.receive_client_disappeared().await {
            Ok(s) => s,
            Err(err) => {
                warn!("error subscribing to ClientDisappeared: {err:?}");
                return;
            }
        };
        let has_cal_changed = proxy.receive_has_calendars_changed().await;

        let dbus = match zbus::fdo::DBusProxy::new(&async_conn).await {
            Ok(p) => p,
            Err(err) => {
                warn!("error creating DBusProxy for CalendarServer name tracking: {err:?}");
                return;
            }
        };
        let owner = match dbus
            .receive_name_owner_changed_with_args(&[(0, BUS_NAME)])
            .await
        {
            Ok(s) => s,
            Err(err) => {
                warn!("error subscribing to CalendarServer NameOwnerChanged: {err:?}");
                return;
            }
        };

        // Best-effort initial read; this also activates the service. A timeout
        // is non-fatal — the owner-change wake retries (`js/ui/calendar.js:227-237`).
        if let Ok(v) = proxy.has_calendars().await {
            let _ = to_niri.send(CalendarToSynoik::HasCalendars(v));
        }

        // Merge every wake source into one `Ev` stream (all parsing on this side
        // of the seam). Proxy calls that need loop state (range re-request,
        // property re-read) happen in the loop body.
        let requests = from_niri.map(Ev::Request).boxed();
        let added = added
            .filter_map(|sig| async move {
                sig.args()
                    .ok()
                    .map(|a| Ev::Added(sanitize_batch(&a.events)))
            })
            .boxed();
        let removed = removed
            .filter_map(
                |sig| async move { sig.args().ok().map(|a| Ev::Removed(sanitize_ids(a.ids))) },
            )
            .boxed();
        let gone = gone
            .filter_map(|sig| async move {
                sig.args()
                    .ok()
                    // An over-long uid can't prefix-match any stored id (≤ cap),
                    // so drop it rather than forward unbounded data.
                    .filter(|a| a.source_uid.len() <= MAX_ID_BYTES)
                    .map(|a| Ev::Gone(a.source_uid))
            })
            .boxed();
        let has_cal_changed = has_cal_changed.map(|_| Ev::HasCalChanged).boxed();
        let owner = owner
            .filter_map(
                |ch| async move { ch.args().ok().map(|a| Ev::Owner(a.new_owner().is_some())) },
            )
            .boxed();

        let mut merged = futures_util::stream::select_all([
            requests,
            added,
            removed,
            gone,
            has_cal_changed,
            owner,
        ]);

        // The last range we asked for, so repeats are free (`_datesEqual`).
        let mut last_range: Option<(i64, i64)> = None;

        while let Some(ev) = merged.next().await {
            match ev {
                Ev::Request(SynoikToCalendar::SetRange { since, until }) => {
                    if last_range != Some((since, until)) {
                        last_range = Some((since, until));
                        // Clear the cache before the new range loads — GNOME's
                        // forced `_loadEvents` does this (`js/ui/calendar.js:356-360`),
                        // so an event removed while another month was shown (no
                        // signal for it in that range) can't linger. Sent on the
                        // same in-order channel, so it lands before the new events.
                        if to_niri.send(CalendarToSynoik::CacheReset).is_err() {
                            return;
                        }
                        if let Err(err) = proxy.set_time_range(since, until, true).await {
                            warn!("CalendarServer SetTimeRange failed: {err:?}");
                        }
                    }
                }
                Ev::Added(batch) => {
                    if to_niri
                        .send(CalendarToSynoik::EventsAddedOrUpdated(batch))
                        .is_err()
                    {
                        return;
                    }
                }
                Ev::Removed(ids) => {
                    if to_niri.send(CalendarToSynoik::EventsRemoved(ids)).is_err() {
                        return;
                    }
                }
                Ev::Gone(uid) => {
                    if to_niri
                        .send(CalendarToSynoik::ClientDisappeared(uid))
                        .is_err()
                    {
                        return;
                    }
                }
                Ev::HasCalChanged => {
                    if let Ok(v) = proxy.has_calendars().await {
                        if to_niri.send(CalendarToSynoik::HasCalendars(v)).is_err() {
                            return;
                        }
                    }
                }
                Ev::Owner(true) => {
                    // Appeared: reset + re-request forcefully (`_onNameAppeared`).
                    if to_niri.send(CalendarToSynoik::OwnerAppeared).is_err() {
                        return;
                    }
                    if let Some((since, until)) = last_range {
                        if let Err(err) = proxy.set_time_range(since, until, true).await {
                            warn!("CalendarServer re-request after owner change failed: {err:?}");
                        }
                    }
                    if let Ok(v) = proxy.has_calendars().await {
                        let _ = to_niri.send(CalendarToSynoik::HasCalendars(v));
                    }
                }
                Ev::Owner(false) => {
                    if to_niri.send(CalendarToSynoik::OwnerVanished).is_err() {
                        return;
                    }
                }
            }
        }
    };
    conn.inner()
        .executor()
        .spawn(future, "monitor org.gnome.Shell.CalendarServer")
        .detach();

    Ok(conn)
}

/// Convert a wire batch to sanitized, bounded [`CalendarEvent`]s (the seam:
/// untrusted parsing lives here). Over-long ids are dropped; the batch is capped.
fn sanitize_batch(wire: &[WireEvent]) -> Vec<CalendarEvent> {
    if wire.len() > MAX_EVENTS {
        warn!(
            "CalendarServer batch of {} events exceeds cap {MAX_EVENTS}; dropping the excess",
            wire.len()
        );
    }
    wire.iter()
        .take(MAX_EVENTS)
        .filter_map(|(id, summary, start, end, _extra)| {
            if id.len() > MAX_ID_BYTES {
                warn!(
                    "dropping calendar event with an over-long id ({} bytes)",
                    id.len()
                );
                return None;
            }
            Some(CalendarEvent {
                id: id.clone(),
                summary: sanitize_summary(summary),
                start: *start,
                end: *end,
            })
        })
        .collect()
}

/// Cap and drop over-long ids from a removal batch (transient prefixes, but the
/// seam rule bounds everything crossing it; an over-long prefix can't match a
/// stored id anyway).
fn sanitize_ids(mut ids: Vec<String>) -> Vec<String> {
    ids.truncate(MAX_EVENTS);
    ids.retain(|id| id.len() <= MAX_ID_BYTES);
    ids
}

/// Unicode format chars that don't render but can spoof display order (bidi
/// controls, zero-width spaces/joiners, BOM). Neutralized to a space — GNOME
/// renders summaries raw, but our shaper walks a bidi path (the visual-order
/// glyph-range trap), so we don't feed it attacker-chosen reordering.
fn is_format_char(ch: char) -> bool {
    matches!(ch,
        '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}')
}

/// Flatten a summary to a single bounded line: every control/whitespace/format
/// run becomes one space, trimmed, capped on a char boundary.
fn sanitize_summary(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(MAX_SUMMARY_BYTES));
    let mut prev_space = false;
    for ch in s.chars() {
        let ch = if ch.is_whitespace() || ch.is_control() || is_format_char(ch) {
            ' '
        } else {
            ch
        };
        if ch == ' ' {
            if prev_space {
                continue;
            }
            prev_space = true;
        } else {
            prev_space = false;
        }
        if out.len() + ch.len_utf8() > MAX_SUMMARY_BYTES {
            break;
        }
        out.push(ch);
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_is_flattened_collapsed_and_bounded() {
        assert_eq!(sanitize_summary("  hello\n\tworld  "), "hello world");
        assert_eq!(sanitize_summary("a\u{0007}b"), "a b"); // control char → space
        assert_eq!(sanitize_summary("multi   space"), "multi space");
        let long = "x".repeat(MAX_SUMMARY_BYTES + 100);
        assert_eq!(sanitize_summary(&long).len(), MAX_SUMMARY_BYTES);
    }

    #[test]
    fn sanitize_batch_drops_overlong_ids_and_caps() {
        let ok: WireEvent = ("a\n".into(), " Meeting ".into(), 10, 20, HashMap::new());
        let bad: WireEvent = (
            "x".repeat(MAX_ID_BYTES + 1),
            "y".into(),
            0,
            1,
            HashMap::new(),
        );
        let out = sanitize_batch(&[ok, bad]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "a\n");
        assert_eq!(out[0].summary, "Meeting");
    }
}
