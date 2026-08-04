// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The calendar-events model: the day's appointments shown in the dateMenu's
//! Events section, ported from gnome-shell 50.1's `DBusEventSource`
//! (`js/ui/calendar.js:207-405`).
//!
//! This is the fork-owned, observable store behind the Events section
//! (`src/ui/calendar.rs`); it is fed by the `org.gnome.Shell.CalendarServer`
//! client watcher in `src/dbus/calendar_server.rs` (a session-bus service backed
//! by evolution-data-server, which we consume — we do NOT own the name).
//!
//! Process-isolation seam: event summaries originate from external (possibly
//! network) calendars, so everything crossing into this model is plain,
//! validated data. The watcher does ALL sanitizing/bounding on its side (summary
//! text flattened + capped; event ids kept verbatim because their `\n` is
//! structural, see below) and talks to the compositor over plain message
//! channels, so it can be lifted into a separate process later.
//!
//! Event ids encode recurrence: a non-`\n`-terminated id is a recurring
//! *instance* whose parent series is the prefix up to and including its last
//! `\n` (`js/ui/calendar.js:315-321`); updating one instance first evicts the
//! whole series once per batch. Removals (`EventsRemoved`, `ClientDisappeared`)
//! are all *prefix* deletes on that id scheme.

use std::collections::{BTreeMap, HashSet};

/// Hard cap on cached events (defense-in-depth against a buggy/hostile server
/// streaming many batches within one range; the per-batch cap only bounds one
/// signal). GNOME is uncapped; this is the seam rule's "bounded data".
const MAX_STORED_EVENTS: usize = 4096;

/// A calendar update pushed from the watcher to the compositor
/// (`State::on_calendar_events_msg`). Defined here (not in the feature-gated
/// `dbus::calendar_server`) so `Synoik` can name it unconditionally.
#[derive(Debug)]
pub enum CalendarToSynoik {
    /// Sanitized `EventsAddedOrUpdated` batch.
    EventsAddedOrUpdated(Vec<CalendarEvent>),
    /// `EventsRemoved` ids (prefix deletes).
    EventsRemoved(Vec<String>),
    /// `ClientDisappeared` source uid.
    ClientDisappeared(String),
    /// The requested range changed — drop the cache before the new range's
    /// events arrive, mirroring GNOME's forced `_loadEvents` which clears then
    /// reloads (`js/ui/calendar.js:356-360,369-377`). Without it, an event
    /// deleted server-side while a different month was shown would linger
    /// (eds only signals removals for the *current* range).
    CacheReset,
    /// A fresh `HasCalendars` value.
    HasCalendars(bool),
    /// The server gained an owner — clear the cache; the watcher re-requests.
    OwnerAppeared,
    /// The server lost its owner — clear the cache and hide the section.
    OwnerVanished,
}

/// A range request from the compositor to the watcher.
#[derive(Debug, Clone, Copy)]
pub enum SynoikToCalendar {
    /// Set the visible day range, `[since, until)` Unix seconds.
    SetRange { since: i64, until: i64 },
}

/// One appointment. Times are Unix seconds, exactly as the CalendarServer wire
/// tuple `(s id, s summary, x start, x end, a{sv})` delivers them
/// (`data/dbus-interfaces/org.gnome.Shell.CalendarServer.xml`); day-boundary and
/// title/time formatting are pure functions of an explicit `now`, so nothing
/// here reads a clock (headless-test-safe).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarEvent {
    /// Opaque server id (recurrence-structured; never mangled — see module docs).
    pub id: String,
    /// Sanitized, bounded single-line summary (the watcher flattens/caps it).
    pub summary: String,
    /// Start time, Unix seconds.
    pub start: i64,
    /// End time, Unix seconds.
    pub end: i64,
}

/// The day's events plus whether any calendars are configured. Mutations mirror
/// `DBusEventSource` one-for-one; the caller filters per selected day with
/// [`Self::events_for`].
#[derive(Debug, Default)]
pub struct CalendarEventStore {
    /// Cached events keyed by id. `BTreeMap` (not `HashMap`) so `events_for`
    /// ties break deterministically by id (GNOME ties by `Map` insertion order,
    /// which we can't reproduce across removals — recorded divergence).
    events: BTreeMap<String, CalendarEvent>,
    /// `HasCalendars` (`js/ui/calendar.js:268-273`): the section is hidden when
    /// false. Starts false (the `_initialized`-gated read collapses to false
    /// until the watcher reports).
    has_calendars: bool,
}

impl CalendarEventStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether any calendars are configured (gates section visibility).
    pub fn has_calendars(&self) -> bool {
        self.has_calendars
    }

    /// Adopt `HasCalendars`. Returns whether it changed.
    pub fn set_has_calendars(&mut self, has: bool) -> bool {
        let changed = self.has_calendars != has;
        self.has_calendars = has;
        changed
    }

    /// Drop all cached events (owner appeared/vanished or a forced reload;
    /// `js/ui/calendar.js:279-283`). Leaves `has_calendars` alone — the watcher
    /// drives that separately.
    pub fn reset(&mut self) {
        self.events.clear();
    }

    /// Apply an `EventsAddedOrUpdated` batch (`js/ui/calendar.js:302-325`).
    /// Recurring instances first evict their whole series once per batch, then
    /// every event is inserted/updated by id. Returns whether anything changed.
    pub fn add_or_update(&mut self, batch: Vec<CalendarEvent>) -> bool {
        let mut changed = false;
        let mut handled_removals: HashSet<String> = HashSet::new();
        for event in batch {
            // A recurring *instance* (id not `\n`-terminated) belongs to the
            // series named by the prefix up to its last `\n`
            // (`js/ui/calendar.js:315-321`). `lastIndexOf('\n') + 1` is 0 when
            // there is no `\n`, so the parent prefix is "" and the series purge
            // clears everything — faithful to GNOME's eds id scheme.
            if !event.id.ends_with('\n') {
                let parent: String = match event.id.rfind('\n') {
                    Some(i) => event.id[..=i].to_string(),
                    None => String::new(),
                };
                if handled_removals.insert(parent.clone()) {
                    // The series purge's own return is subsumed: every event in
                    // the batch is inserted below, so a non-empty batch always
                    // reports changed (matching GNOME's per-add `changed = true`).
                    self.remove_matching(&parent);
                }
            }
            // Updates to existing ids always apply; brand-new ids stop at the
            // cap so the store can't grow without bound.
            if self.events.len() >= MAX_STORED_EVENTS && !self.events.contains_key(&event.id) {
                warn!("calendar event store at cap {MAX_STORED_EVENTS}; dropping new events");
                break;
            }
            self.events.insert(event.id.clone(), event);
            changed = true;
        }
        changed
    }

    /// Apply an `EventsRemoved` batch — each id is a prefix delete
    /// (`js/ui/calendar.js:332-341`). Returns whether anything changed.
    pub fn remove(&mut self, ids: &[String]) -> bool {
        let mut changed = false;
        for id in ids {
            changed |= self.remove_matching(id);
        }
        changed
    }

    /// A source client vanished: evict every event whose id starts with
    /// `source_uid\n` (`js/ui/calendar.js:343-349`). Returns whether it changed.
    pub fn client_disappeared(&mut self, source_uid: &str) -> bool {
        let mut prefix = source_uid.to_string();
        prefix.push('\n');
        self.remove_matching(&prefix)
    }

    /// Delete every cached event whose id starts with `prefix`
    /// (`js/ui/calendar.js:290-297`). Returns whether anything was removed.
    fn remove_matching(&mut self, prefix: &str) -> bool {
        let before = self.events.len();
        self.events.retain(|id, _| !id.starts_with(prefix));
        self.events.len() != before
    }

    /// The events overlapping `[begin, end)` (Unix seconds), sorted for display
    /// (`js/ui/calendar.js:386-397`): by start time, except an event that began
    /// before the day but ends within it sorts by its end time.
    pub fn events_for(&self, begin: i64, end: i64) -> Vec<&CalendarEvent> {
        let mut result: Vec<&CalendarEvent> = self
            .events
            .values()
            .filter(|e| overlaps_interval(e.start, e.end, begin, end))
            .collect();
        // Stable sort: ties keep BTreeMap (id) order.
        result.sort_by_key(|e| {
            if e.start < begin && e.end <= end {
                e.end
            } else {
                e.start
            }
        });
        result
    }
}

/// Whether an event `[e0, e1]` overlaps the interval `[i0, i1)`, ported line for
/// line from `_eventOverlapsInterval` (`js/ui/calendar.js:193-204`) — the first
/// clause deliberately admits zero-length events at the interval start.
fn overlaps_interval(e0: i64, e1: i64, i0: i64, i1: i64) -> bool {
    if e0 >= i0 && e1 < i1 {
        return true;
    }
    if e1 <= i0 {
        return false;
    }
    if i1 <= e0 {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(id: &str, start: i64, end: i64) -> CalendarEvent {
        CalendarEvent {
            id: id.to_string(),
            summary: format!("summary {id}"),
            start,
            end,
        }
    }

    #[test]
    fn has_calendars_starts_false_and_tracks() {
        let mut s = CalendarEventStore::new();
        assert!(!s.has_calendars());
        assert!(s.set_has_calendars(true));
        assert!(s.has_calendars());
        assert!(!s.set_has_calendars(true), "no change reported when equal");
        assert!(s.set_has_calendars(false));
    }

    #[test]
    fn recurring_instance_evicts_its_series_once_per_batch() {
        let mut s = CalendarEventStore::new();
        // Two existing instances of series "uid\n".
        s.add_or_update(vec![ev("uid\n100", 100, 200), ev("uid\n300", 300, 400)]);
        assert_eq!(s.events.len(), 2);

        // A batch with two fresh instances of the SAME series purges the old
        // series exactly once (handled-removals set), then inserts both.
        s.add_or_update(vec![ev("uid\n500", 500, 600), ev("uid\n700", 700, 800)]);
        let ids: Vec<&String> = s.events.keys().collect();
        assert_eq!(
            ids,
            vec!["uid\n500", "uid\n700"],
            "old series evicted, {ids:?}"
        );
    }

    #[test]
    fn newline_terminated_id_is_not_a_recurring_instance() {
        let mut s = CalendarEventStore::new();
        s.add_or_update(vec![ev("a\n", 100, 200)]);
        // A `\n`-terminated id updates in place, evicting nothing else.
        s.add_or_update(vec![ev("b\n", 300, 400)]);
        assert_eq!(s.events.len(), 2);
        // Updating "a\n" replaces just it.
        s.add_or_update(vec![ev("a\n", 150, 250)]);
        assert_eq!(s.events.len(), 2);
        assert_eq!(s.events["a\n"].start, 150);
    }

    #[test]
    fn non_recurring_id_without_newline_clears_everything() {
        // GNOME: a non-`\n`-terminated id with no interior `\n` yields parent
        // prefix "" → `_removeMatching("")` clears the whole cache first.
        let mut s = CalendarEventStore::new();
        s.add_or_update(vec![ev("other\n1", 1, 2), ev("more\n2", 3, 4)]);
        s.add_or_update(vec![ev("loner", 500, 600)]);
        assert_eq!(s.events.keys().collect::<Vec<_>>(), vec!["loner"]);
    }

    #[test]
    fn events_removed_is_a_prefix_delete() {
        let mut s = CalendarEventStore::new();
        s.add_or_update(vec![
            ev("uid\n1", 1, 2),
            ev("uid\n2", 3, 4),
            ev("keep\n", 5, 6),
        ]);
        assert!(s.remove(&["uid\n".to_string()]));
        assert_eq!(s.events.keys().collect::<Vec<_>>(), vec!["keep\n"]);
        assert!(!s.remove(&["absent".to_string()]), "no-op returns false");
    }

    #[test]
    fn client_disappeared_appends_newline_before_prefix_delete() {
        let mut s = CalendarEventStore::new();
        s.add_or_update(vec![ev("src\n1", 1, 2), ev("srcOTHER\n1", 3, 4)]);
        // "src" must not match "srcOTHER\n1": the appended `\n` scopes it.
        assert!(s.client_disappeared("src"));
        assert_eq!(s.events.keys().collect::<Vec<_>>(), vec!["srcOTHER\n1"]);
    }

    #[test]
    fn overlap_predicate_matches_gnome_including_zero_length_edges() {
        // Interval [100, 200).
        assert!(overlaps_interval(150, 150, 100, 200), "zero-length inside");
        assert!(
            overlaps_interval(100, 100, 100, 200),
            "zero-length at start"
        );
        assert!(
            !overlaps_interval(200, 200, 100, 200),
            "zero-length at end excluded"
        );
        assert!(
            !overlaps_interval(0, 100, 100, 200),
            "ends exactly at begin excluded"
        );
        assert!(
            !overlaps_interval(200, 300, 100, 200),
            "starts exactly at end excluded"
        );
        assert!(overlaps_interval(50, 150, 100, 200), "straddles begin");
        assert!(overlaps_interval(150, 250, 100, 200), "straddles end");
        assert!(overlaps_interval(50, 250, 100, 200), "spans the whole day");
    }

    #[test]
    fn events_for_filters_and_sorts() {
        let mut s = CalendarEventStore::new();
        // Day [1000, 2000). A: starts-before, ends-within (sorts by END 1200);
        // B: in-day at 1100; C: in-day at 1500; D: fully outside.
        s.add_or_update(vec![
            ev("a\n", 500, 1200),
            ev("b\n", 1100, 1150),
            ev("c\n", 1500, 1600),
            ev("d\n", 5000, 6000),
        ]);
        let got: Vec<&str> = s
            .events_for(1000, 2000)
            .iter()
            .map(|e| e.id.as_str())
            .collect();
        // A sorts by its end (1200) so it lands after B (1100) but before C.
        assert_eq!(got, vec!["b\n", "a\n", "c\n"]);
    }

    #[test]
    fn reset_clears_events_but_not_has_calendars() {
        let mut s = CalendarEventStore::new();
        s.set_has_calendars(true);
        s.add_or_update(vec![ev("a\n", 1, 2)]);
        s.reset();
        assert!(s.events.is_empty());
        assert!(s.has_calendars(), "reset must not touch has_calendars");
    }
}
