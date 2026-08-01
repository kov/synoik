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
//! (`switcherPopup.js:159-167`). Ours had a 150ms open delay too, but also a 750ms
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
/// `DISABLE_HOVER_TIMEOUT` (`:13`) — how long pointer selection stays off after keyboard use.
///
/// It is a **timer, not a motion latch**: `_disableHover` (`:290-303`) clears `mouseActive` and
/// re-arms this timeout, which sets it back on when it fires. Nothing waits for the pointer to
/// travel. And it re-arms on *every* keypress (`:198`) and scroll (`:244`), not once at open, so
/// holding Tab down keeps hover suppressed throughout rather than for 500ms from the start.
///
/// Without it, a pointer that merely *happens* to rest where the popup opens steals the
/// selection from the keyboard.
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
/// (`_switcher-popup.scss:4,16`).
pub const LIST_PADDING: f64 = 12.;
/// `.switcher-list` `border-radius: $switcher_radius` = `$modal_radius + $switcher_padding`
/// (`_switcher-popup.scss:5,17`); `$modal_radius` is `$base_border_radius * 2` = 16
/// (`_common.scss:40`).
pub const LIST_RADIUS: f64 = 28.;
/// `%osd_panel`'s hairline, which `.switcher-list` extends (`_switcher-popup.scss:15`).
pub const LIST_BORDER: f64 = 1.;
/// `.switcher-list-item-container` `spacing: $base_padding * 2` (`:21-23`).
pub const ITEM_SPACING: f64 = 12.;
/// `.item-box` is a `tile_button` at OSD colours (`_switcher-popup.scss:26-27`), and
/// `tile_button` is `@extend %tile` (`_drawing.scss:351`) — so its padding is `$base_padding`
/// and its radius comes from `%tile`, which is `$base_border_radius * 2` (`_common.scss:84-86`).
///
/// **Not `$base_border_radius`.** `%tile` doubles it, and the doubling is what keeps the item
/// concentric inside the panel: `LIST_RADIUS - LIST_PADDING` = 28 - 12 = 16 is exactly this
/// value, which is the relation [`tests::the_switcher_panel_geometry_follows_the_scss`] pins.
pub const ITEM_PADDING: f64 = 6.;
pub const ITEM_RADIUS: f64 = 16.;

/// `.switcher-list` fill and hairline: `%osd_panel`, shared with the OSD pill.
pub const LIST_BG: Rgba = style::OSD_BG;
pub const LIST_BORDER_COLOR: Rgba = style::OSD_BORDER;
pub const LIST_FG: Rgba = style::OSD_FG;

/// `.item-box:selected` — `transparentize($osd_fg_color, 0.8)` (`_switcher-popup.scss:32-34`),
/// with `$osd_fg_color` = `$light_1` = `#ffffff` (`_colors.scss:16`, `_palette.scss:37`).
///
/// Deliberately brighter than the overview's selection wash: the SCSS says so in a comment
/// ("brighter than normal selected style"), because this panel is already light-on-dark.
///
/// The high-contrast build uses `transparentize(…, 0.7)`, i.e. alpha 0.35 (`:36-40`). We do not
/// model `$contrast == 'high'` anywhere yet, so this is one more entry on that debt, not a
/// divergence specific to the switcher.
pub const ITEM_SELECTED: Rgba = [1., 1., 1., 0.2];

/// `.item-box:hover { background: none }` (`_switcher-popup.scss:28-29`).
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
/// **[`Pending`](Visibility::Pending) gates *drawing only*.** Everything else about the switcher
/// is already live before the delay expires — `SwitcherPopup.show` (`switcherPopup.js:122-168`),
/// in order:
///
/// 1. takes the modal grab **first** (`pushModal`, `:125`),
/// 2. maps the actor at `opacity = 0` and forces an allocation (`:137-140`) — explicitly so it can
///    tell whether the initial selection needs a scroll,
/// 3. runs [`initial_selection`] (`:142`),
/// 4. checks the release race (below),
/// 5. *then* arms the [`POPUP_DELAY`] timer, whose whole body is `opacity = 255` (`:159-180`).
///
/// So GNOME is literally the "opacity 0 for 150ms" model — and the grab ordering is not a
/// detail we can reorder for tidiness. The headline behaviour (a quick tap switches with no
/// visible UI) *depends* on it: the Alt release is delivered as a key-release **to the grabbing
/// popup**, which is what calls `_finish` (`:222-234`). Defer the grab until `Shown` and the
/// release goes to the focused client instead, so the tap does nothing at all.
///
/// Two more things end [`Pending`] early, neither of them a timer:
/// - **the release race** (`:144-155`, citing bgo#596695): the modifier can come up before the grab
///   lands, in which case nothing would ever notify us — so `show` samples the pointer modifier
///   state directly and commits on the spot if it is already up.
/// - **any handled keypress** calls `_showImmediately` (`:201-203`), so a second Tab reveals the
///   popup without waiting. The rule is "150ms **or** the next Tab, whichever comes first".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    /// Grab held, allocated, selection made — but `opacity = 0`, so nothing is on screen.
    Pending,
    Shown,
    /// Fading out over [`FADE_OUT`] after a commit or cancel.
    Fading,
}

/// Where the selection starts — `SwitcherPopup._initialSelection` (`switcherPopup.js:113-120`).
///
/// **Forward starts at 1, not 0.** Item 0 is the window/app you are already on, so starting
/// there would make a tap-and-release a no-op — the single most noticeable way to get a
/// switcher wrong. The exception is a one-item list, which has nowhere else to go.
///
/// `backward` is not a separate binding path but `binding.is_reversed()`
/// (`windowManager.js:1705`), i.e. the `-backward` half of every switch binding.
///
/// `AppSwitcherPopup` overrides this for `switch-group` (`altTab.js:118-137`); that override
/// belongs with the app switcher, not here.
pub fn initial_selection(len: usize, backward: bool) -> Option<usize> {
    match () {
        () if len == 0 => None,
        () if backward => Some(len - 1),
        () if len == 1 => Some(0),
        () => Some(1),
    }
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

        // The other half of that relation, and the one with teeth: an item's corner sits
        // LIST_PADDING inside the panel's, so for the two to stay concentric the item radius
        // must be the panel's less the padding. `%tile` gets there by doubling
        // $base_border_radius (_common.scss:84-85) — reading `.item-box` as a plain
        // $base_border_radius tile gives 8, and this assert is what rejects it.
        assert_eq!(
            ITEM_RADIUS,
            LIST_RADIUS - LIST_PADDING,
            "%tile's radius must leave the item concentric inside the panel"
        );
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
        assert_ne!(
            ITEM_HOVER,
            style::HOVER_WASH,
            "the switcher must not inherit the ordinary tile hover"
        );
    }

    /// Forward selection starts on the *previous* item, not the current one.
    ///
    /// `_initialSelection` (`switcherPopup.js:113-120`). This is the assertion that a
    /// tap-and-release actually switches: starting at 0 would select the window you are already
    /// on, and Alt-Tab would appear to do nothing at all.
    #[test]
    fn a_forward_switcher_starts_on_the_previous_item() {
        assert_eq!(initial_selection(4, false), Some(1), "forward starts at 1");
        assert_eq!(
            initial_selection(4, true),
            Some(3),
            "backward starts at the end"
        );

        // A one-item list has nowhere to go, so it is the one case that selects the current
        // item — and it is checked *after* `backward`, so a lone item still selects itself.
        assert_eq!(initial_selection(1, false), Some(0));
        assert_eq!(initial_selection(1, true), Some(0));

        // `show()` bails before any of this on an empty list (`switcherPopup.js:123-124`).
        assert_eq!(initial_selection(0, false), None);
        assert_eq!(initial_selection(0, true), None);
    }
}
