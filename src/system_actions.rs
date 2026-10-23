// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The system actions the app search finds by keyword — `SystemActions.getMatchingActions`
//! (`js/misc/systemActions.js:89-158,296-313`): "shutdown" finds Power Off, "screenshot" finds
//! Take a Screenshot.
//!
//! Which are available is the caller's to say; the keywords, names and icons are here.

use std::sync::OnceLock;

/// One action, in `this._actions`' order — the order matches are listed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SystemAction {
    PowerOff,
    Restart,
    LockScreen,
    Logout,
    Suspend,
    SwitchUser,
    LockOrientation,
    Screenshot,
}

impl SystemAction {
    pub const ALL: [SystemAction; 8] = [
        SystemAction::PowerOff,
        SystemAction::Restart,
        SystemAction::LockScreen,
        SystemAction::Logout,
        SystemAction::Suspend,
        SystemAction::SwitchUser,
        SystemAction::LockOrientation,
        SystemAction::Screenshot,
    ];

    /// The result id (`POWER_OFF_ACTION_ID`, …) — never a `.desktop` id, which is how the app
    /// provider tells the two apart (`appDisplay.js:1773`).
    pub fn id(self) -> &'static str {
        match self {
            SystemAction::PowerOff => "power-off",
            SystemAction::Restart => "restart",
            SystemAction::LockScreen => "lock-screen",
            SystemAction::Logout => "logout",
            SystemAction::Suspend => "suspend",
            SystemAction::SwitchUser => "switch-user",
            SystemAction::LockOrientation => "lock-orientation",
            SystemAction::Screenshot => "open-screenshot-ui",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.id() == id)
    }

    /// The result's name (`C_('search-result', …)`). Lock Orientation's follows its state, which
    /// we have none of: it is never available.
    pub fn name(self) -> &'static str {
        match self {
            SystemAction::PowerOff => "Power Off",
            SystemAction::Restart => "Restart",
            SystemAction::LockScreen => "Lock Screen",
            SystemAction::Logout => "Log Out",
            SystemAction::Suspend => "Suspend",
            SystemAction::SwitchUser => "Switch User",
            SystemAction::LockOrientation => "",
            SystemAction::Screenshot => "Take a Screenshot",
        }
    }

    pub fn icon_name(self) -> &'static str {
        match self {
            SystemAction::PowerOff => "system-shutdown-symbolic",
            SystemAction::Restart => "system-reboot-symbolic",
            SystemAction::LockScreen => "system-lock-screen-symbolic",
            SystemAction::Logout => "system-log-out-symbolic",
            SystemAction::Suspend => "media-playback-pause-symbolic",
            SystemAction::SwitchUser => "system-switch-user-symbolic",
            SystemAction::LockOrientation => "",
            SystemAction::Screenshot => "record-screen-symbolic",
        }
    }

    /// The `;`-separated keywords, as translated (`_('power off;poweroff;…')`).
    fn keyword_source(self) -> &'static str {
        match self {
            SystemAction::PowerOff => "power off;poweroff;shutdown;halt;stop",
            SystemAction::Restart => "reboot;restart;",
            SystemAction::LockScreen => "lock screen",
            SystemAction::Logout => "logout;log out;sign off",
            SystemAction::Suspend => "suspend;sleep",
            SystemAction::SwitchUser => "switch user",
            SystemAction::LockOrientation => "lock orientation;unlock orientation;screen;rotation",
            SystemAction::Screenshot => "screenshot;screencast;snip;capture;record",
        }
    }

    /// `tokenizeKeywords`: every keyword tokenized and folded, with its ASCII alternates — the
    /// keywords keep both lists (`.flat(2)`), only the terms drop the second.
    fn keywords(self) -> &'static [String] {
        static KEYWORDS: OnceLock<Vec<Vec<String>>> = OnceLock::new();
        let all = KEYWORDS.get_or_init(|| {
            SystemAction::ALL
                .iter()
                .map(|a| {
                    a.keyword_source()
                        .split(';')
                        .flat_map(|k| {
                            let (tokens, alternates) = tokenize_and_fold(k);
                            tokens.into_iter().chain(alternates)
                        })
                        .collect()
                })
                .collect()
        });
        &all[self as usize]
    }
}

/// `getMatchingActions`: the available actions every term is the start of one of whose keywords
/// is, in order. Terms tokenize and fold the way the keywords did, so "Shut" finds "shutdown"
/// and "log out" is two terms that each have to match.
pub fn matching(terms: &[String], available: impl Fn(SystemAction) -> bool) -> Vec<SystemAction> {
    let terms: Vec<String> = terms.iter().flat_map(|t| tokenize_and_fold(t).0).collect();
    if terms.is_empty() {
        return Vec::new();
    }
    SystemAction::ALL
        .into_iter()
        .filter(|&action| available(action))
        .filter(|action| {
            let keywords = action.keywords();
            terms
                .iter()
                .all(|t| keywords.iter().any(|k| k.starts_with(t.as_str())))
        })
        .collect()
}

/// `GLib.str_tokenize_and_fold(text, null)`: the words of `text`, normalized and case-folded,
/// and their ASCII transliterations where they differ.
fn tokenize_and_fold(text: &str) -> (Vec<String>, Vec<String>) {
    use gio::glib;
    use gio::glib::translate::{FromGlibPtrContainer, ToGlibPtr};

    // A C string ends at the first NUL; whatever came after it was never a word.
    let text = text.split('\0').next().unwrap_or_default();
    // SAFETY: `text` is a valid NUL-terminated string for the duration of the call (the stash
    // lives to the end of the statement); a null locale is GLib's documented default; both
    // returned vectors are newly allocated and owned by us, and `from_glib_full` takes them over.
    let (tokens, alternates): (Vec<glib::GString>, Vec<glib::GString>) = unsafe {
        let mut alternates: *mut *mut std::ffi::c_char = std::ptr::null_mut();
        let tokens = glib::ffi::g_str_tokenize_and_fold(
            text.to_glib_none().0,
            std::ptr::null(),
            &mut alternates,
        );
        (
            FromGlibPtrContainer::from_glib_full(tokens),
            FromGlibPtrContainer::from_glib_full(alternates),
        )
    };
    (
        tokens.into_iter().map(String::from).collect(),
        alternates.into_iter().map(String::from).collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(query: &[&str]) -> Vec<SystemAction> {
        let terms: Vec<String> = query.iter().map(|s| s.to_string()).collect();
        matching(&terms, |a| a != SystemAction::LockOrientation)
    }

    #[test]
    fn keywords_match_by_prefix_and_every_term_must() {
        assert_eq!(find(&["shut"]), [SystemAction::PowerOff]);
        assert_eq!(find(&["Power", "off"]), [SystemAction::PowerOff]);
        assert_eq!(
            find(&["re"]),
            [SystemAction::Restart, SystemAction::Screenshot]
        );
        assert_eq!(
            find(&["lo"]),
            [SystemAction::LockScreen, SystemAction::Logout]
        );
        assert_eq!(find(&["log", "out"]), [SystemAction::Logout]);
        assert_eq!(find(&["sleep"]), [SystemAction::Suspend]);
        assert_eq!(find(&["firefox"]), []);
        assert_eq!(find(&["  "]), [], "nothing to match on");
        assert_eq!(
            find(&["SCREEN"]),
            [SystemAction::LockScreen, SystemAction::Screenshot]
        );
        // A term's ASCII alternates are dropped — an accent that is not in the keyword does
        // not match, in gnome-shell either.
        assert_eq!(find(&["scréen"]), []);
    }

    #[test]
    fn an_unavailable_action_is_never_found() {
        let terms = vec!["rotation".to_string()];
        assert_eq!(matching(&terms, |_| true), [SystemAction::LockOrientation]);
        assert_eq!(matching(&terms, |a| a != SystemAction::LockOrientation), []);
    }

    #[test]
    fn ids_round_trip() {
        for action in SystemAction::ALL {
            assert_eq!(SystemAction::from_id(action.id()), Some(action));
            assert!(!action.id().ends_with(".desktop"));
        }
    }
}
