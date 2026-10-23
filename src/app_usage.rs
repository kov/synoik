// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! How much each app is used — `Shell.AppUsage` (`src/shell-app-usage.c`), which orders the app
//! search's results within a relevance tier.
//!
//! An app scores a point for every [`FOCUS_TIME_MIN_SECONDS`] it holds the focus; past
//! [`SCORE_MAX`] every score is halved, so the ranking can still change. Going idle ends the
//! current stretch at most [`IDLE_TIME_TRANSITION_SECONDS`] after it began. Scores live in
//! `<user data dir>/gnome-shell/application_state`, the file gnome-shell keeps, in its format, so
//! a user moving between the two keeps one history. A save is queued
//! [`SAVE_APPS_TIMEOUT_SECONDS`] after a change, and on load anything rarely used and unseen for a
//! week is forgotten.
//!
//! Time is wall-clock seconds, passed in by the caller, as `g_get_real_time` is in the C.
//!
//! **Divergence:** with `org.gnome.desktop.privacy remember-app-usage` off nothing is counted.
//! gnome-shell stops its watch when the key goes off but keeps its `notify::focus-app` handler,
//! which starts counting again on the next focus change; the key's description promises the
//! opposite.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::PathBuf;

/// `FOCUS_TIME_MIN_SECONDS`: a point per this many seconds of continuous focus.
pub const FOCUS_TIME_MIN_SECONDS: i64 = 7;
/// `IDLE_TIME_TRANSITION_SECONDS`: how much of a stretch counts when the session goes idle.
pub const IDLE_TIME_TRANSITION_SECONDS: i64 = 30;
/// `SAVE_APPS_TIMEOUT_SECONDS`: how long after a change the scores are written.
pub const SAVE_APPS_TIMEOUT_SECONDS: u64 = 5 * 60;
/// `SCORE_MAX`: past this, every score is halved.
pub const SCORE_MAX: f64 = (3600 * 50 / FOCUS_TIME_MIN_SECONDS) as f64;
/// `SCORE_MIN`: an app below this and unseen for a week is forgotten on load.
pub const SCORE_MIN: f64 = ((3600 * 50 / FOCUS_TIME_MIN_SECONDS) >> 3) as f64;
/// `USAGE_CLEAN_DAYS`.
const WEEK_SECONDS: i64 = 7 * 24 * 60 * 60;
/// Bounds on the file we read: it is ours, but anything in the home directory can write it.
const MAX_FILE_BYTES: u64 = 1 << 20;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Usage {
    score: f64,
    last_seen: i64,
}

#[derive(Debug, Default)]
pub struct AppUsage {
    usages: HashMap<String, Usage>,
    /// The app holding the focus, and since when.
    watched: Option<String>,
    watch_start: i64,
    idle: bool,
    monitoring: bool,
}

/// `<user data dir>/gnome-shell/application_state`.
pub fn data_path() -> PathBuf {
    gio::glib::user_data_dir()
        .join("gnome-shell")
        .join("application_state")
}

impl AppUsage {
    pub fn new() -> Self {
        Self::default()
    }

    /// `restore_from_file`: the scores in `xml`, then `idle_clean_usage` at `now`. The file is
    /// anyone's to write, so it is read leniently: a document that does not parse is no scores
    /// at all (GMarkup keeps what came before the error), an entry without an id is skipped and
    /// a `last-seen` out of range is 0 (the C fails the parse, and wraps).
    pub fn from_xml(xml: &str, now: i64) -> Self {
        let mut usage = Self::new();
        if let Ok(doc) = roxmltree::Document::parse(xml) {
            for node in doc.descendants().filter(|n| n.has_tag_name("application")) {
                let Some(id) = node.attribute("id") else {
                    continue;
                };
                let score = node
                    .attribute("score")
                    .and_then(|s| s.parse::<f64>().ok())
                    .filter(|s| s.is_finite())
                    .unwrap_or(0.);
                let last_seen = node
                    .attribute("last-seen")
                    .and_then(|s| s.parse::<u32>().ok())
                    .map_or(0, i64::from);
                usage
                    .usages
                    .insert(id.to_owned(), Usage { score, last_seen });
            }
        }
        usage.clean(now);
        usage
    }

    /// Read the file at `path`, or start empty when there is none (or it is not ours to read).
    pub fn load(path: &std::path::Path, now: i64) -> Self {
        use std::io::Read;
        let Ok(file) = std::fs::File::open(path) else {
            return Self::new();
        };
        let mut xml = String::new();
        if let Err(err) = file.take(MAX_FILE_BYTES).read_to_string(&mut xml) {
            tracing::warn!("could not load applications usage data: {err}");
            return Self::new();
        }
        Self::from_xml(&xml, now)
    }

    /// `idle_save_application_usage`'s document: every app `known` still knows.
    pub fn to_xml(&self, known: impl Fn(&str) -> bool) -> String {
        let mut out = String::from("<?xml version=\"1.0\"?>\n<application-state>\n");
        out.push_str("  <context id=\"\">\n");
        let mut ids: Vec<&String> = self.usages.keys().filter(|id| known(id)).collect();
        ids.sort();
        for id in ids {
            let usage = self.usages[id];
            out.push_str(&format!(
                "    <application id=\"{}\" score=\"{}\" last-seen=\"{}\"/>\n",
                gio::glib::markup_escape_text(id),
                usage.score,
                usage.last_seen.clamp(0, i64::from(u32::MAX)),
            ));
        }
        out.push_str("  </context>\n</application-state>\n");
        out
    }

    /// `idle_clean_usage`: forget apps used little and not seen for a week.
    fn clean(&mut self, now: i64) {
        let week_ago = now - WEEK_SECONDS;
        self.usages
            .retain(|_, u| !(u.score < SCORE_MIN && u.last_seen < week_ago));
    }

    /// `shell_app_usage_compare`: the more used first, apps never seen last.
    ///
    /// **Divergence:** the C returns the score difference truncated to an `int`, so scores less
    /// than a point apart tie — which is not transitive (3.5 ties 4, 4 ties 4.5, 3.5 does not
    /// tie 4.5), and a sort may panic on a comparator that is not a total order. The scores are
    /// compared whole instead.
    pub fn compare(&self, a: &str, b: &str) -> Ordering {
        match (self.usages.get(a), self.usages.get(b)) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(a), Some(b)) => b.score.total_cmp(&a.score),
        }
    }

    /// `increment_usage_for_app_at_time`; whether a save should be queued.
    fn increment(&mut self, id: &str, at: i64) -> bool {
        let start = self.watch_start;
        let usage = self.usages.entry(id.to_owned()).or_default();
        usage.last_seen = at;
        let count = (at - start).max(0) / FOCUS_TIME_MIN_SECONDS;
        if count == 0 {
            return false;
        }
        usage.score += count as f64;
        if usage.score > SCORE_MAX {
            for u in self.usages.values_mut() {
                u.score /= 2.;
            }
        }
        true
    }

    /// `on_focus_app_changed`: the stretch of the app that had the focus is counted, and `app`
    /// starts one. Whether a save should be queued.
    pub fn focus_changed(&mut self, app: Option<&str>, now: i64) -> bool {
        if !self.monitoring || self.watched.as_deref() == app {
            return false;
        }
        let changed = match self.watched.take() {
            Some(watched) => self.increment(&watched, now),
            None => false,
        };
        self.watched = app.map(str::to_owned);
        self.watch_start = now;
        changed
    }

    /// `on_app_state_changed`: an app found running is seen now.
    pub fn app_running(&mut self, id: &str, now: i64) {
        if self.monitoring {
            self.usages.entry(id.to_owned()).or_default().last_seen = now;
        }
    }

    /// `on_session_status_changed`: going idle ends the stretch, counting at most
    /// [`IDLE_TIME_TRANSITION_SECONDS`] of it; coming back starts a new one. Whether a save
    /// should be queued.
    pub fn set_idle(&mut self, idle: bool, now: i64) -> bool {
        if self.idle == idle {
            return false;
        }
        self.idle = idle;
        if !idle {
            self.watch_start = now;
            return false;
        }
        match (self.monitoring, self.watched.clone()) {
            (true, Some(watched)) => {
                self.increment(&watched, self.watch_start + IDLE_TIME_TRANSITION_SECONDS)
            }
            _ => false,
        }
    }

    /// `update_enable_monitoring`: turning it on starts watching `focused`; off stops counting.
    pub fn set_monitoring(&mut self, enable: bool, focused: Option<&str>, now: i64) {
        if enable == self.monitoring {
            return;
        }
        self.monitoring = enable;
        self.watched = None;
        if enable {
            self.watched = focused.map(str::to_owned);
            self.watch_start = now;
        }
    }

    pub fn is_monitoring(&self) -> bool {
        self.monitoring
    }

    #[cfg(test)]
    fn score(&self, id: &str) -> Option<f64> {
        self.usages.get(id).map(|u| u.score)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watching(app: &str, now: i64) -> AppUsage {
        let mut usage = AppUsage::new();
        usage.set_monitoring(true, Some(app), now);
        usage
    }

    #[test]
    fn focus_scores_a_point_per_seven_seconds() {
        let mut usage = watching("a.desktop", 0);
        assert!(
            !usage.focus_changed(Some("b.desktop"), 6),
            "under seven seconds"
        );
        assert_eq!(usage.score("a.desktop"), Some(0.));
        assert!(usage.focus_changed(Some("a.desktop"), 6 + 15));
        assert_eq!(usage.score("b.desktop"), Some(2.));
        assert_eq!(usage.compare("b.desktop", "a.desktop"), Ordering::Less);
        assert_eq!(usage.compare("b.desktop", "c.desktop"), Ordering::Less);
        assert_eq!(usage.compare("c.desktop", "d.desktop"), Ordering::Equal);
    }

    #[test]
    fn comparing_is_a_total_order_on_fractional_scores() {
        let usage = AppUsage::from_xml(
            "<application-state><context id=\"\">\
             <application id=\"a\" score=\"3.5\" last-seen=\"0\"/>\
             <application id=\"b\" score=\"4\" last-seen=\"0\"/>\
             <application id=\"c\" score=\"4.5\" last-seen=\"0\"/>\
             </context></application-state>",
            0,
        );
        assert_eq!(usage.compare("c", "b"), Ordering::Less);
        assert_eq!(usage.compare("b", "a"), Ordering::Less);
        assert_eq!(usage.compare("c", "a"), Ordering::Less);
        // Enough ids for the sort to go past its insertion-sort threshold.
        let mut ids: Vec<String> = (0..64)
            .map(|i| ["a", "b", "c", "z"][i % 4].to_owned())
            .collect();
        ids.sort_by(|x, y| usage.compare(x, y));
        assert_eq!(ids.first().map(String::as_str), Some("c"));
        assert_eq!(ids.last().map(String::as_str), Some("z"));
    }

    #[test]
    fn idle_counts_at_most_thirty_seconds_and_restarts_the_stretch() {
        let mut usage = watching("a.desktop", 0);
        assert!(usage.set_idle(true, 600));
        assert_eq!(usage.score("a.desktop"), Some((30 / 7) as f64));
        usage.set_idle(false, 1000);
        usage.focus_changed(None, 1014);
        assert_eq!(usage.score("a.desktop"), Some((30 / 7 + 2) as f64));
    }

    #[test]
    fn past_the_maximum_every_score_halves() {
        let mut usage = watching("a.desktop", 0);
        usage.focus_changed(Some("b.desktop"), 70);
        usage.focus_changed(None, 70 + (SCORE_MAX as i64 + 1) * 7);
        assert_eq!(usage.score("a.desktop"), Some(5.));
        assert_eq!(usage.score("b.desktop"), Some((SCORE_MAX + 1.) / 2.));
    }

    #[test]
    fn nothing_counts_with_monitoring_off() {
        let mut usage = AppUsage::new();
        assert!(!usage.focus_changed(Some("a.desktop"), 0));
        assert!(!usage.focus_changed(None, 100));
        assert_eq!(usage.score("a.desktop"), None);
    }

    #[test]
    fn the_file_round_trips_and_forgets_the_rarely_used() {
        let now = 30 * 24 * 3600;
        let xml = format!(
            "<?xml version=\"1.0\"?>\n<application-state>\n  <context id=\"\">\n\
             <application id=\"kept.desktop\" score=\"12.5\" last-seen=\"{now}\"/>\n\
             <application id=\"busy.desktop\" score=\"{}\" last-seen=\"0\"/>\n\
             <application id=\"stale.desktop\" score=\"3\" last-seen=\"0\"/>\n\
             <application score=\"3\"/>\n  </context>\n</application-state>\n",
            SCORE_MIN + 1.
        );
        let usage = AppUsage::from_xml(&xml, now);
        assert_eq!(usage.score("kept.desktop"), Some(12.5));
        assert!(usage.score("busy.desktop").is_some(), "used enough to keep");
        assert_eq!(usage.score("stale.desktop"), None, "rare and a week unseen");

        let written = usage.to_xml(|id| id != "busy.desktop");
        let again = AppUsage::from_xml(&written, now);
        assert_eq!(again.score("kept.desktop"), Some(12.5));
        assert_eq!(
            again.score("busy.desktop"),
            None,
            "an app no longer installed is dropped"
        );
        assert!(AppUsage::from_xml("not xml", now).usages.is_empty());
    }
}
