//! The Alt-Tab / Super-Tab switchers — gnome-shell 50.3's `SwitcherPopup`
//! (`js/ui/switcherPopup.js`) and the popups built on it (`js/ui/altTab.js`).
//!
//! **Super-Tab and Alt-Tab are not the same UI.** `WindowManager._startSwitcher`
//! (`js/ui/windowManager.js:1670-1694`) picks a different popup class per keybinding:
//! `switch-applications` (`<Super>Tab`) raises an `AppSwitcherPopup` of 96px *app*
//! icons, while `switch-windows` (`<Alt>Tab`) raises a `WindowSwitcherPopup` of 128px
//! *window* previews. `switch-group` (`Above_Tab`) is the app switcher again, opened
//! within the current app; the two cyclers (`<Alt>Escape`, `<Alt>F6`) have no list at
//! all and just outline each window in turn. The port plan is
//! `docs/fork/alt-tab-port.md`.
//!
//! This module is the part they share: the timing and commit rules that make a
//! switcher *feel* like GNOME's, and the `.switcher-list` panel they all sit in.
//!
//! The timing is the whole feel, and the first constant is the one people notice:
//! the popup does **not** appear for [`POPUP_DELAY`] after the keypress, so a quick
//! Alt-Tab tap switches windows with no visible UI whatsoever
//! (`switcherPopup.js:152-165`). Ours had a 150ms open delay too, but also a 750ms
//! "debounce" with no GNOME counterpart.

use std::time::Duration;

use crate::ui::widget::{style, Rgba};

/// `POPUP_DELAY_TIMEOUT` (`js/ui/switcherPopup.js:8`) — how long the modifier must be
/// held before the popup is drawn at all. A tap shorter than this switches silently.
pub const POPUP_DELAY: Duration = Duration::from_millis(150);
/// `NO_MODS_TIMEOUT` (`:14`) — a switcher opened with **no modifier held** (a gesture,
/// or a bind with no modifier) cannot commit on release, so it commits after this
/// instead (`:315-317`).
pub const NO_MODS_TIMEOUT: Duration = Duration::from_millis(1500);
/// `DISABLE_HOVER_TIMEOUT` (`:13`) — after a keypress the pointer stops selecting until
/// it actually moves (`:263-296`). Without it, a pointer that merely *happens* to rest
/// where the popup opens steals the selection from the keyboard.
pub const DISABLE_HOVER_TIMEOUT: Duration = Duration::from_millis(500);
/// `POPUP_FADE_OUT_TIME` (`:11`).
pub const FADE_OUT: Duration = Duration::from_millis(100);
/// `POPUP_SCROLL_TIME` (`:10`) — how long the list takes to scroll a newly selected
/// item into view.
pub const SCROLL_TIME: Duration = Duration::from_millis(100);

/// `.switcher-popup` `spacing: $base_padding * 4` (`_switcher-popup.scss:10`) — the gap
/// between stacked switcher lists (the app switcher and its thumbnail sub-list).
pub const POPUP_SPACING: f64 = 24.;
/// `.switcher-list` `padding: $switcher_padding` = `$base_padding * 2`
/// (`_switcher-popup.scss:4,15`).
pub const LIST_PADDING: f64 = 12.;
/// `.switcher-list` `border-radius: $switcher_radius` = `$modal_radius + $switcher_padding`
/// (`_switcher-popup.scss:5,16`); `$modal_radius` is `$base_border_radius * 2` = 16
/// (`_common.scss:40`).
pub const LIST_RADIUS: f64 = 28.;
/// `%osd_panel`'s hairline, which `.switcher-list` extends (`_switcher-popup.scss:14`).
pub const LIST_BORDER: f64 = 1.;
/// `.switcher-list-item-container` `spacing: $base_padding * 2` (`:19-21`).
pub const ITEM_SPACING: f64 = 12.;
/// `.item-box` is a `tile_button`, whose padding is `$base_padding` (`_drawing.scss`
/// `tile_button`) and whose radius is `$base_border_radius` — the same tile the overview
/// uses, at OSD colours (`_switcher-popup.scss:24`).
pub const ITEM_PADDING: f64 = 6.;
pub const ITEM_RADIUS: f64 = 8.;
/// `.separator` `width: 1px` (`_switcher-popup.scss:41-44`) — the rule between the app
/// switcher's running apps and the rest.
pub const SEPARATOR_WIDTH: f64 = 1.;

/// `.switcher-list` fill and hairline: `%osd_panel`, shared with the OSD pill.
pub const LIST_BG: Rgba = style::OSD_BG;
pub const LIST_BORDER_COLOR: Rgba = style::OSD_BORDER;
pub const LIST_FG: Rgba = style::OSD_FG;

/// `.item-box:selected` — `transparentize($osd_fg_color, 0.8)` (`_switcher-popup.scss:31`).
///
/// Deliberately brighter than the overview's selection wash: the SCSS says so in a comment
/// ("brighter than normal selected style"), because this panel is already light-on-dark.
pub const ITEM_SELECTED: Rgba = [1., 1., 1., 0.2];

/// `.item-box:hover { background: none }` (`_switcher-popup.scss:26-27`).
///
/// **Not an oversight, and not a wash we forgot to pick** — the SCSS comments it
/// "override %tile style so mouse doesn't steal focus". Hovering an item moves the
/// *selection* (`_itemEntered`, `switcherPopup.js:263-266`), so painting a second,
/// weaker highlight under the pointer would show two "current" items at once. The
/// pointer's feedback here is the selection itself.
pub const ITEM_HOVER: Rgba = style::TRANSPARENT;

/// Why a switcher is showing — which popup class GNOME would have raised, and therefore
/// what the items are.
///
/// Kept as data rather than one popup with a mode flag because the two really are
/// different lists: an app switcher's item is an app (with every one of its windows
/// behind it), a window switcher's item is a single window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitcherKind {
    /// `AppSwitcherPopup` — `switch-applications` (`<Super>Tab`) and `switch-group`.
    Apps,
    /// `WindowSwitcherPopup` — `switch-windows` (`<Alt>Tab`).
    Windows,
}

/// Whether the popup has become visible yet.
///
/// `SwitcherPopup.show` starts a [`POPUP_DELAY`] timer and only maps the actor when it
/// fires (`switcherPopup.js:152-165`); if the modifier is released first, `_finish` runs
/// and the popup is destroyed **without ever having been drawn**. Modelling that as a
/// state rather than "opacity 0 for 150ms" matters: a switcher that never showed also
/// never took a pointer grab and never scrolled, and a test can assert the difference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    /// Counting down to [`POPUP_DELAY`]; nothing is drawn.
    Pending,
    Shown,
    /// Fading out over [`FADE_OUT`] after a commit or cancel.
    Fading,
}

/// How a switcher session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitcherOutcome {
    /// Modifier released (or [`NO_MODS_TIMEOUT`] elapsed): activate the selection.
    Commit,
    /// Escape: leave focus where it was (`switcherPopup.js:201-217`).
    Cancel,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The switcher panel's geometry, derived from the SCSS rather than eyeballed.
    ///
    /// `$switcher_radius` is the one worth pinning: it is `$modal_radius + $switcher_padding`
    /// (`_switcher-popup.scss:5`), i.e. the panel's corner is the *content* radius grown by its
    /// own padding, so the inner tiles' corners stay concentric with the panel's. Writing 28 as a
    /// literal loses that, and it drifts the moment either input changes.
    #[test]
    fn the_switcher_panel_geometry_follows_the_scss() {
        const MODAL_RADIUS: f64 = 16.;
        assert_eq!(
            LIST_RADIUS,
            MODAL_RADIUS + LIST_PADDING,
            "$switcher_radius = $modal_radius + $switcher_padding"
        );
        assert_eq!(LIST_PADDING, 2. * 6., "$base_padding * 2");
        assert_eq!(ITEM_SPACING, 2. * 6., "$base_padding * 2");
        assert_eq!(POPUP_SPACING, 4. * 6., "$base_padding * 4");
    }

    /// A hovered item draws **nothing**, and that is the spec.
    ///
    /// `.item-box:hover { background: none }` overrides the tile's own hover
    /// (`_switcher-popup.scss:26-27`, commented "so mouse doesn't steal focus"), because hovering
    /// already moves the selection. A wash here would paint two current items at once. This is
    /// the shape [[hover-direction-per-widget]] warns about — a widget overriding an inherited
    /// state colour — so it is pinned rather than left to look like an unfinished constant.
    #[test]
    fn a_hovered_item_paints_nothing_because_hover_moves_the_selection() {
        assert_eq!(ITEM_HOVER, style::TRANSPARENT);
        assert_ne!(
            ITEM_HOVER,
            style::HOVER_WASH,
            "the switcher must not inherit the ordinary tile hover"
        );
        assert_eq!(ITEM_SELECTED[3], 0.2, "transparentize($osd_fg_color, 0.8)");
    }

    /// The delay is what makes a quick tap invisible, so it must be the *gate* on drawing,
    /// not a fade.
    #[test]
    fn the_popup_delay_is_gnomes_and_precedes_visibility() {
        assert_eq!(POPUP_DELAY, Duration::from_millis(150));
        assert_eq!(NO_MODS_TIMEOUT, Duration::from_millis(1500));
        assert!(
            NO_MODS_TIMEOUT > POPUP_DELAY,
            "a no-modifier switcher must become visible long before it self-commits"
        );
    }
}
