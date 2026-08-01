//! The list a switcher switches through — `getWindows` (`js/ui/altTab.js:51-61`) sitting on
//! mutter's `meta_display_get_tab_list` (`src/core/display.c:1876-1940`).
//!
//! It looks like a detail and is not: the order *is* the feature. Alt-Tab's whole contract is
//! "the second item is the window you were on before", which only holds if this list is in
//! focus-recency order with the same things filtered out that GNOME filters out.
//!
//! Two things here are worth reading before changing anything, because both are places where the
//! obvious implementation is not what a stock GNOME session does — see
//! [`tab_list`] and [`SwitcherWindow::attached_to`].

use std::collections::HashSet;
use std::time::Duration;

use crate::window::mapped::MappedId;

/// One candidate window, reduced to just what the ordering rules read.
///
/// Deliberately a plain snapshot rather than a borrow of the layout: the rules below are pure
/// list arithmetic, and keeping them that way is what lets them be tested against hand-built
/// orders instead of a live compositor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitcherWindow {
    pub id: MappedId,
    /// When this window last had focus; `None` for one that never has. Drives MRU order.
    pub focus_timestamp: Option<Duration>,
    pub on_active_workspace: bool,
    /// mutter's `wm_state_demands_attention`, our `Mapped::is_urgent`.
    pub demands_attention: bool,
    /// The parent this window collapses into, if it is an **attached modal dialog**.
    ///
    /// **Stock GNOME leaves this `None` for every window**, and that is the whole point of the
    /// field being narrow. `is_attached_dialog()` is `window->attached`, set by
    /// `meta_window_should_attach_to_parent` (`mutter/src/core/window.c:910-933`), which requires
    /// *all* of: the `attach-modal-dialogs` pref, a window type of `MODAL_DIALOG`, and a
    /// transient-for parent that is itself normal/dialog/modal. That pref
    /// (`org.gnome.mutter attach-modal-dialogs`) **defaults to false**
    /// (`mutter/data/org.gnome.mutter.gschema.xml.in:26-27`).
    ///
    /// So the `.map()` in `getWindows` that the docs describe as "attached dialogs are mapped to
    /// their parent" is, in a default session, a **no-op** — modal dialogs get their own switcher
    /// entries. Collapsing every parented window into its parent would look like faithfulness and
    /// would in fact show fewer windows than GNOME does. Fill this in only for a window that
    /// genuinely satisfies all three conditions.
    pub attached_to: Option<MappedId>,
}

/// Build the switcher's list: MRU order, attached dialogs collapsed, duplicates dropped.
///
/// `current_workspace_only` is the gsetting, and which schema it comes from depends on the popup
/// — `org.gnome.shell.app-switcher` defaults it **false** while
/// `org.gnome.shell.window-switcher` defaults it **true**, so stock Super-Tab spans workspaces
/// and stock Alt-Tab does not. This function only applies the flag; picking it is the caller's.
///
/// Filtering by workspace does **not** simply drop everything elsewhere: windows on other
/// workspaces that demand attention are added back, at the **front** of the list
/// (`display.c:1924-1934`). A window shouting for you is worth reaching in one Tab even though it
/// is somewhere else.
///
/// Two of mutter's rules are deliberately absent because our model has nowhere to put them:
/// - `META_TAB_LIST_NORMAL_ALL` is *not* pure MRU — it sorts unminimized windows ahead of minimized
///   ones (`display.c:1899-1921`; only `NORMAL_ALL_MRU` is pure). We have no minimized state, so
///   the split is currently vacuous. It stops being vacuous the day we add one, and this is where
///   it goes.
/// - `in_tab_chain` for `NORMAL_ALL` excludes `DOCK` and `DESKTOP` window types
///   (`display.c:1730-1738`). Those are X11 types; on Wayland a dock is a layer-shell surface,
///   which is not a toplevel and so never reaches this list.
///
/// Note `NORMAL_ALL` does not filter `skip_taskbar` itself — `getWindows` does that afterwards,
/// deliberately, so that an attached dialog's *position* in the MRU list can be used before its
/// parent replaces it. We have no `skip_taskbar` equivalent (there is no such xdg-toplevel hint),
/// so that filter has no counterpart here.
pub fn tab_list(windows: &[SwitcherWindow], current_workspace_only: bool) -> Vec<MappedId> {
    let mut candidates: Vec<&SwitcherWindow> = windows.iter().collect();

    // MRU: most recently focused first, never-focused windows last. Stable, so windows that
    // share a timestamp (or have none) keep the caller's order.
    candidates.sort_by(|a, b| b.focus_timestamp.cmp(&a.focus_timestamp));

    if current_workspace_only {
        let (here, elsewhere): (Vec<_>, Vec<_>) =
            candidates.into_iter().partition(|w| w.on_active_workspace);

        // Attention-demanding windows from other workspaces go in front of everything.
        candidates = elsewhere
            .into_iter()
            .filter(|w| w.demands_attention)
            .chain(here)
            .collect();
    }

    let mut seen = HashSet::new();
    candidates
        .into_iter()
        .map(|w| w.attached_to.unwrap_or(w.id))
        .filter(|id| seen.insert(*id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(id: MappedId, focused_at: Option<u64>) -> SwitcherWindow {
        SwitcherWindow {
            id,
            focus_timestamp: focused_at.map(Duration::from_millis),
            on_active_workspace: true,
            demands_attention: false,
            attached_to: None,
        }
    }

    /// The order is focus recency, and it is what makes item 1 "the window before this one".
    ///
    /// A window that has never held focus sorts last rather than first — an app that just opened
    /// in the background must not become the Alt-Tab target.
    #[test]
    fn the_list_is_in_focus_recency_order() {
        let (a, b, c, d) = (
            MappedId::next(),
            MappedId::next(),
            MappedId::next(),
            MappedId::next(),
        );
        let windows = [
            win(a, Some(10)),
            win(b, Some(30)),
            win(c, None),
            win(d, Some(20)),
        ];

        assert_eq!(tab_list(&windows, false), [b, d, a, c]);
    }

    /// Windows sharing a timestamp keep their given order rather than shuffling between opens.
    #[test]
    fn ties_are_broken_stably() {
        let (a, b, c) = (MappedId::next(), MappedId::next(), MappedId::next());
        let windows = [win(a, Some(5)), win(b, Some(5)), win(c, Some(5))];

        assert_eq!(tab_list(&windows, false), [a, b, c]);
    }

    /// `current-workspace-only` filters — **except** for windows shouting from elsewhere, which
    /// are pulled to the front (`display.c:1924-1934`).
    #[test]
    fn attention_reaches_across_workspaces_even_when_filtering() {
        let (here_a, here_b, quiet, urgent) = (
            MappedId::next(),
            MappedId::next(),
            MappedId::next(),
            MappedId::next(),
        );

        let mut urgent_win = win(urgent, Some(1));
        urgent_win.on_active_workspace = false;
        urgent_win.demands_attention = true;

        let mut quiet_win = win(quiet, Some(50));
        quiet_win.on_active_workspace = false;

        let windows = [
            win(here_a, Some(30)),
            quiet_win,
            urgent_win,
            win(here_b, Some(20)),
        ];

        // Unfiltered, it is plain MRU and the urgent window sits where its recency puts it.
        assert_eq!(tab_list(&windows, false), [quiet, here_a, here_b, urgent]);

        // Filtered, the quiet window elsewhere is gone and the urgent one jumps the queue
        // despite being the *least* recently focused of the lot.
        assert_eq!(tab_list(&windows, true), [urgent, here_a, here_b]);
    }

    /// An attached modal dialog collapses into its parent, and the parent inherits the dialog's
    /// place in the MRU list rather than its own.
    ///
    /// That is the reason `getWindows` maps before it dedups: the dialog is what you were just
    /// using, so its recency is the more useful one.
    #[test]
    fn an_attached_dialog_lends_its_recency_to_its_parent() {
        let (parent_id, dialog_id, other) = (MappedId::next(), MappedId::next(), MappedId::next());

        let mut dialog = win(dialog_id, Some(40));
        dialog.attached_to = Some(parent_id);

        let windows = [win(parent_id, Some(10)), dialog, win(other, Some(20))];

        // The parent appears at the dialog's position (first), not its own (last), and appears
        // exactly once.
        assert_eq!(tab_list(&windows, false), [parent_id, other]);
    }

    /// Several dialogs on one parent collapse to a single entry.
    #[test]
    fn duplicates_collapse_to_one_entry() {
        let (parent_id, a, b) = (MappedId::next(), MappedId::next(), MappedId::next());

        let mut first = win(a, Some(40));
        first.attached_to = Some(parent_id);
        let mut second = win(b, Some(30));
        second.attached_to = Some(parent_id);

        assert_eq!(
            tab_list(&[win(parent_id, Some(10)), first, second], false),
            [parent_id]
        );
    }

    /// An empty list stays empty — `show()` refuses to open on it, so this is the "no windows,
    /// no popup" path.
    #[test]
    fn no_windows_means_no_list() {
        assert!(tab_list(&[], false).is_empty());
        assert!(tab_list(&[], true).is_empty());
    }
}
