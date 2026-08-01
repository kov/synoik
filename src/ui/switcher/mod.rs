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

use niri_config::Modifiers;
use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::ui::widget::{self, style, Rgba};

pub mod app_switcher;
pub mod ui;
pub mod window_list;

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

/// `.switcher-list` `box-shadow: 0 8px 8px 0 $shadow_color` (`_switcher-popup.scss:18`), with
/// `$shadow_color` (dark) = `rgba(0,0,0,0.2)` (`_default-colors.scss:36`).
///
/// The one part of the panel `%osd_panel` does *not* give us — the OSD pill has no shadow.
///
/// Carried at the literal spread of 0. `popover.rs` found that St's real shadow renders denser
/// than a naive gaussian and matched its measured reference with a spread of **2** instead — but
/// that was measured at `blur: 4`, and extrapolating someone else's fudge factor to a blur twice
/// as wide is exactly the kind of guess this port is supposed to avoid. Check it against the seat
/// when the panel first draws, and cite the measurement if it moves.
pub const LIST_SHADOW: widget::DropShadowSpec = widget::DropShadowSpec {
    blur: 8.,
    offset: (0., 8.),
    spread: 0.,
    color: [0., 0., 0., 0.2],
};

/// Where the panel and its items sit, in output-logical coordinates.
///
/// Both Tab popups build their list with `squareItems = true` (`altTab.js:698, 1064`), which sets
/// the item container `homogeneous` (`switcherPopup.js:448`) and makes every item's preferred
/// width and height the max over *both* dimensions of *all* items (`:575-590, 600-618`). So the
/// row is N identical squares — an app with a long title cannot make its own tile wider than the
/// rest. The thumbnail sub-list passes `false` (`altTab.js:914`) and is slice 5.
#[derive(Debug, Clone, PartialEq)]
pub struct PanelLayout {
    /// The `.switcher-list` box, including its padding and hairline.
    pub panel: Rectangle<f64, Logical>,
    /// One `.item-box` per item, left to right, in the same space as [`panel`](Self::panel).
    pub items: Vec<Rectangle<f64, Logical>>,
}

impl PanelLayout {
    /// Lay out `contents` (each item's *content* size, inside its `.item-box` padding) centered
    /// on `monitor`.
    ///
    /// `monitor` is the **primary** monitor, not the focused one: `vfunc_allocate` reads
    /// `Main.layoutManager.primaryMonitor` (`switcherPopup.js:96`), so on a multi-head setup the
    /// switcher appears on the primary head however far away you are working. That is GNOME's
    /// behaviour, not an oversight of ours — see `docs/fork/alt-tab-port.md`.
    ///
    /// A row too wide for the monitor is clamped to it here. GNOME's real answer to overflow is
    /// two-part: the app switcher shrinks its icons down a ladder first
    /// (`AppSwitcher._setIconSize`), and only then does the `St.ScrollView` scroll
    /// ([`SCROLL_TIME`]). Both belong to the popups that have item art, so slices 2 and 3.
    pub fn new(contents: &[Size<f64, Logical>], monitor: Rectangle<f64, Logical>) -> Self {
        if contents.is_empty() {
            return Self {
                panel: Rectangle::default(),
                items: Vec::new(),
            };
        }

        // Every item box is its content plus `.item-box`'s padding on all four sides...
        let side = contents
            .iter()
            .map(|c| (c.w + ITEM_PADDING * 2.).max(c.h + ITEM_PADDING * 2.))
            .fold(0f64, f64::max);
        // ...and squareItems makes them all that one square.
        let n = contents.len() as f64;

        let size = Size::<f64, Logical>::from((
            n * side + (n - 1.) * ITEM_SPACING + LIST_PADDING * 2.,
            side + LIST_PADDING * 2.,
        ));

        // Centered on both axes — the switcher sits in the middle of the screen, not near an
        // edge (`switcherPopup.js:104-109`). `floor`, like the reference.
        let loc = Point::from((
            (monitor.loc.x + ((monitor.size.w - size.w) / 2.).floor()).max(monitor.loc.x),
            (monitor.loc.y + ((monitor.size.h - size.h) / 2.).floor()).max(monitor.loc.y),
        ));
        let size = Size::from((size.w.min(monitor.size.w), size.h.min(monitor.size.h)));

        let items = (0..contents.len())
            .map(|i| {
                Rectangle::new(
                    Point::from((
                        loc.x + LIST_PADDING + i as f64 * (side + ITEM_SPACING),
                        loc.y + LIST_PADDING,
                    )),
                    Size::from((side, side)),
                )
            })
            .collect();

        Self {
            panel: Rectangle::new(loc, size),
            items,
        }
    }

    /// The item under `point`, for hover selection.
    ///
    /// Only the item boxes hit, not the gaps between them: GNOME's `item-entered` comes from a
    /// motion handler on each `SwitcherButton` (`switcherPopup.js:487-489`), so the spacing
    /// belongs to no item and the pointer crossing it selects nothing new.
    pub fn item_at(&self, point: Point<f64, Logical>) -> Option<usize> {
        self.items.iter().position(|r| r.contains(point))
    }
}

// The panel's *painting* — fill, hairline, shadow, selection wash — lands with the first popup
// that has items to draw inside it (slice 2). Everything it needs is already here: the colours,
// the radii, `LIST_SHADOW`, and `PanelLayout` for the boxes.
//
// It is deliberately not written ahead of that caller. Our render tests go through a real output
// (`src/tests/vulkan_render.rs`), so a painter with no popup behind it cannot be pixel-tested —
// and paint code whose pixels have never been looked at reads as finished when it is not.

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
    /// Escape: leave focus where it was (`switcherPopup.js:208-209`).
    Cancel,
}

/// A key the *base* popup acts on, already classified by the caller.
///
/// GNOME splits this across two layers: `vfunc_key_press_event` (`switcherPopup.js:194-219`) asks
/// the subclass's `_keyPressHandler` first and only falls through to Escape/Tab/commit if the
/// subclass did not consume the key. The subclass matches on the **keybinding action**
/// (`global.display.get_keybinding_action`, `:196-197`), not the keysym, which is how a rebound
/// Alt-Tab keeps working.
///
/// We classify in the caller for the same reason: input already resolves a keypress to a
/// [`GnomeKeyAction`](crate::gnome::GnomeKeyAction), so it is the layer that knows whether this
/// key is "the switch binding again" or a plain keysym. The per-popup keys (`w`/`q`/`F4`, the
/// arrows, the thumbnail list) belong to the subclasses and arrive in slices 2 and 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitcherKey {
    /// The switch binding fired again — move the selection. Subclass-handled in GNOME, so it
    /// also reveals the popup immediately (`_showImmediately`, `:201-203`).
    Advance { backward: bool },
    /// Escape, or Tab *not* consumed by the popup's own shortcut (`:207-209`).
    Dismiss,
    /// Space, Return, KP_Enter or ISO_Enter (`:211-217`) — an explicit "take this one", which is
    /// the only way to commit a popup opened with no modifier to release.
    Commit,
}

/// GNOME's `primaryModifier` (`switcherPopup.js:25-35`): the **highest set bit** of the binding's
/// modifier mask.
///
/// Commit waits on that one modifier rather than on the whole mask, which is what makes
/// `<Super><Shift>Tab` commit when Super comes up while Shift is still held. Note GNOME reads it
/// from live pointer state at release time (`:223-227`) rather than tracking key events.
fn primary_modifier(mask: Modifiers) -> Modifiers {
    Modifiers::from_bits_truncate(if mask.is_empty() {
        0
    } else {
        1 << (u8::BITS - 1 - mask.bits().leading_zeros())
    })
}

/// The shared switcher state machine — `SwitcherPopup` (`js/ui/switcherPopup.js`) minus the item
/// art, which is the subclasses' job.
///
/// It deliberately knows only how *many* items there are, never what they are: an app switcher's
/// item is an app and a window switcher's is a window, but every rule in here — the delay, the
/// commit, wraparound, hover suppression, reselection after a removal — is index arithmetic and
/// timing. That is also what makes it testable before either popup exists.
///
/// **Timers are deadlines, not callbacks.** GNOME arms `GLib` timeouts; we store the instant each
/// one is due and let the caller drive [`poll`](Self::poll), scheduling with
/// [`next_deadline`](Self::next_deadline). The caller passing `now` explicitly is deliberate —
/// see [[headless-animation-clock-trap]] for what reading a lazy clock inside a UI does to tests.
#[derive(Debug)]
pub struct SwitcherPopup {
    kind: SwitcherKind,
    len: usize,
    selected: usize,
    visibility: Visibility,
    /// The modifier whose release commits. Empty for a no-modifier popup, which instead lives on
    /// [`NO_MODS_TIMEOUT`].
    modifier: Modifiers,
    reveal_at: Option<Duration>,
    no_mods_deadline: Option<Duration>,
    /// When pointer selection comes back on — see [`DISABLE_HOVER_TIMEOUT`].
    hover_deadline: Option<Duration>,
    mouse_active: bool,
    outcome: Option<SwitcherOutcome>,
}

impl SwitcherPopup {
    /// `SwitcherPopup.show` (`switcherPopup.js:122-168`).
    ///
    /// Returns `None` for an empty list, where GNOME returns `false` and the binding does nothing
    /// at all (`:123-124`) — no grab, no popup.
    ///
    /// `mask` is the binding's modifiers and `held` is what is *actually* down right now. When the
    /// modifier is already up, this commits immediately and the popup never reaches
    /// [`Visibility::Shown`]: the release-before-grab race of bgo#596695 (`:144-155`). The caller
    /// must still treat that as a completed session and read [`outcome`](Self::outcome).
    pub fn show(
        kind: SwitcherKind,
        len: usize,
        backward: bool,
        mask: Modifiers,
        held: Modifiers,
        now: Duration,
    ) -> Option<Self> {
        let selected = initial_selection(len, backward)?;

        let modifier = primary_modifier(mask);
        let mut popup = Self {
            kind,
            len,
            selected,
            visibility: Visibility::Pending,
            modifier,
            reveal_at: Some(now + POPUP_DELAY),
            no_mods_deadline: None,
            hover_deadline: None,
            // GNOME leaves `mouseActive` true until something disables it, so a popup opened
            // under a resting pointer is hover-selectable from the start.
            mouse_active: true,
            outcome: None,
        };

        if modifier.is_empty() {
            // Nothing to release, so the only automatic commit is the timeout (`:156`).
            popup.no_mods_deadline = Some(now + NO_MODS_TIMEOUT);
        } else if !held.contains(modifier) {
            popup.finish();
            return Some(popup);
        }

        Some(popup)
    }

    pub fn kind(&self) -> SwitcherKind {
        self.kind
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn visibility(&self) -> Visibility {
        self.visibility
    }

    /// `Some` once the session has ended; the caller activates the selection on
    /// [`SwitcherOutcome::Commit`] and then drops the popup after [`FADE_OUT`].
    pub fn outcome(&self) -> Option<SwitcherOutcome> {
        self.outcome
    }

    /// Whether pointer motion currently moves the selection (`mouseActive`).
    pub fn hover_selects(&self) -> bool {
        self.mouse_active
    }

    /// The earliest instant [`poll`](Self::poll) has anything to do, for the event loop to sleep
    /// until. `None` means no timer is armed.
    pub fn next_deadline(&self) -> Option<Duration> {
        [self.reveal_at, self.no_mods_deadline, self.hover_deadline]
            .into_iter()
            .flatten()
            .min()
    }

    /// Fire whatever is due. Idempotent, and safe to call more often than the deadlines.
    pub fn poll(&mut self, now: Duration) {
        if self.reveal_at.is_some_and(|at| now >= at) {
            self.show_immediately();
        }

        if self.hover_deadline.is_some_and(|at| now >= at) {
            self.hover_deadline = None;
            self.mouse_active = true;
        }

        if self.no_mods_deadline.is_some_and(|at| now >= at) {
            self.no_mods_deadline = None;
            self.finish();
        }
    }

    /// `_showImmediately` (`:169-180`) — the delay's only effect is opacity, so this is just
    /// "stop waiting". Does nothing once the popup is visible or already ending.
    fn show_immediately(&mut self) {
        if self.reveal_at.take().is_some() && self.visibility == Visibility::Pending {
            self.visibility = Visibility::Shown;
        }
    }

    /// `vfunc_key_press_event` (`:194-219`), with the key already classified — see
    /// [`SwitcherKey`].
    pub fn key_press(&mut self, key: SwitcherKey, now: Duration) {
        if self.outcome.is_some() {
            return;
        }

        // Every keypress suppresses hover, before the key is even dispatched (`:198`).
        self.disable_hover(now);

        match key {
            SwitcherKey::Advance { backward } => {
                if backward {
                    self.select_previous();
                } else {
                    self.select_next();
                }
                // A handled key reveals the popup without waiting out the delay (`:201-203`).
                self.show_immediately();
            }
            SwitcherKey::Dismiss => self.cancel(),
            SwitcherKey::Commit => self.finish(),
        }
    }

    /// `vfunc_key_release_event` (`:222-234`): commit when the primary modifier comes up.
    ///
    /// `held` is live modifier state, matching GNOME's `global.get_pointer()` sample rather than
    /// a tally of key events. A no-modifier popup has nothing to release, so a release instead
    /// re-arms its [`NO_MODS_TIMEOUT`] — the deadline is 1500ms from the *last* key release, not
    /// from open.
    pub fn key_release(&mut self, held: Modifiers, now: Duration) {
        if self.outcome.is_some() {
            return;
        }

        if self.modifier.is_empty() {
            self.no_mods_deadline = Some(now + NO_MODS_TIMEOUT);
        } else if !held.contains(self.modifier) {
            self.finish();
        }
    }

    /// `_itemEntered` (`:263-266`): hover moves the selection, but only while `mouseActive`.
    ///
    /// Returns whether the selection actually moved, so the caller can skip a redraw.
    pub fn pointer_entered_item(&mut self, item: usize) -> bool {
        if !self.mouse_active || self.outcome.is_some() || item == self.selected {
            return false;
        }

        self.select(item);
        true
    }

    /// `_disableHover` (`:290-303`) — clears the flag and (re-)arms the timer that restores it.
    fn disable_hover(&mut self, now: Duration) {
        self.mouse_active = false;
        self.hover_deadline = Some(now + DISABLE_HOVER_TIMEOUT);
    }

    /// `_itemRemovedHandler` (`:269-284`) — an app stopped running, or a window was closed or
    /// unmanaged, while the popup is up.
    ///
    /// Removing the last item destroys the popup, which is a **cancel**: there is nothing left to
    /// activate, so committing would have to invent a target.
    pub fn item_removed(&mut self, n: usize) {
        if n >= self.len || self.outcome.is_some() {
            return;
        }

        self.len -= 1;
        if self.len == 0 {
            self.cancel();
            return;
        }

        // Below the selection everything shifts up; at it, the slot is reused unless it was the
        // last. Above it, indices are unaffected and GNOME explicitly reselects nothing.
        if n < self.selected {
            self.selected -= 1;
        } else if n == self.selected {
            self.selected = n.min(self.len - 1);
        }
    }

    pub fn select(&mut self, n: usize) {
        if self.len > 0 {
            self.selected = n.min(self.len - 1);
        }
    }

    /// `_next` (`:181-183`), wrapping with `mod` (`:21-23`).
    pub fn select_next(&mut self) {
        if self.len > 0 {
            self.selected = (self.selected + 1) % self.len;
        }
    }

    /// `_previous` (`:185-187`).
    pub fn select_previous(&mut self) {
        if self.len > 0 {
            self.selected = (self.selected + self.len - 1) % self.len;
        }
    }

    /// `_finish` (`:335-341`): activate the selection.
    fn finish(&mut self) {
        self.end(SwitcherOutcome::Commit);
    }

    /// `fadeAndDestroy` reached from Escape (`:208-209`): leave focus alone.
    fn cancel(&mut self) {
        self.end(SwitcherOutcome::Cancel);
    }

    fn end(&mut self, outcome: SwitcherOutcome) {
        self.outcome = Some(outcome);
        // A popup that never became visible has nothing to fade — the quick-tap case, where the
        // whole point is that no frame ever showed it.
        self.visibility = match self.visibility {
            Visibility::Pending => Visibility::Pending,
            _ => Visibility::Fading,
        };
        self.reveal_at = None;
        self.no_mods_deadline = None;
        self.hover_deadline = None;
    }
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

    const ALT: Modifiers = Modifiers::ALT;
    const T0: Duration = Duration::ZERO;

    fn open(len: usize) -> SwitcherPopup {
        SwitcherPopup::show(SwitcherKind::Windows, len, false, ALT, ALT, T0).unwrap()
    }

    /// The headline: tap Alt-Tab and release inside the delay, and the switch happens with no
    /// frame ever showing the popup.
    ///
    /// This is the behaviour the whole delay exists for, and it is also what pins the grab
    /// ordering — the release only reaches us because the grab was taken at `show`, long before
    /// the popup was due to be drawn. Asserting the popup stayed [`Visibility::Pending`] *and*
    /// committed is the pair that would fail if we ever "simplified" this into a fade-in.
    #[test]
    fn a_tap_shorter_than_the_delay_commits_without_ever_drawing() {
        let mut popup = open(4);
        assert_eq!(popup.visibility(), Visibility::Pending);

        let tap = POPUP_DELAY / 2;
        popup.poll(tap);
        popup.key_release(Modifiers::empty(), tap);

        assert_eq!(popup.outcome(), Some(SwitcherOutcome::Commit));
        assert_eq!(
            popup.selected(),
            1,
            "the tap switches to the previous window"
        );
        assert_eq!(
            popup.visibility(),
            Visibility::Pending,
            "a popup that was never drawn has nothing to fade out"
        );

        // And it stays invisible: polling past the deadline must not resurrect a dead popup.
        popup.poll(POPUP_DELAY * 4);
        assert_eq!(popup.visibility(), Visibility::Pending);
    }

    /// Holding past the delay draws it; releasing then commits with a fade.
    #[test]
    fn holding_past_the_delay_shows_the_popup_and_then_fades_it() {
        let mut popup = open(4);

        popup.poll(POPUP_DELAY);
        assert_eq!(popup.visibility(), Visibility::Shown);

        popup.key_press(SwitcherKey::Advance { backward: false }, POPUP_DELAY);
        assert_eq!(popup.selected(), 2);

        popup.key_release(Modifiers::empty(), POPUP_DELAY);
        assert_eq!(popup.outcome(), Some(SwitcherOutcome::Commit));
        assert_eq!(popup.visibility(), Visibility::Fading);
    }

    /// A second Tab inside the delay reveals the popup immediately (`_showImmediately`).
    ///
    /// So the rule is "150ms **or** the next Tab, whichever first" — a fast double-Tab shows the
    /// popup rather than flickering past it.
    #[test]
    fn a_second_tab_inside_the_delay_reveals_the_popup_at_once() {
        let mut popup = open(4);
        let early = POPUP_DELAY / 3;

        popup.key_press(SwitcherKey::Advance { backward: false }, early);

        assert_eq!(popup.visibility(), Visibility::Shown);
        assert_eq!(popup.selected(), 2);
    }

    /// The modifier can come up before the grab lands, in which case no release is ever
    /// delivered — so `show` samples it directly (bgo#596695, `switcherPopup.js:144-155`).
    ///
    /// Getting this wrong strands the popup on screen until the no-mods timeout, which is the
    /// visible form of the bug: a switcher that ignores the key you already let go of.
    #[test]
    fn a_modifier_released_before_the_grab_commits_at_once() {
        let popup =
            SwitcherPopup::show(SwitcherKind::Windows, 4, false, ALT, Modifiers::empty(), T0)
                .unwrap();

        assert_eq!(popup.outcome(), Some(SwitcherOutcome::Commit));
        assert_eq!(popup.selected(), 1);
        assert_eq!(popup.visibility(), Visibility::Pending);
    }

    /// Commit waits on the mask's *highest* bit, not the whole mask.
    ///
    /// `primaryModifier` (`:25-35`). With `<Super><Shift>Tab`, letting go of Shift while Super is
    /// still down must **not** commit — otherwise shift-tabbing backwards through the list ends
    /// the session on the first release.
    #[test]
    fn only_the_primary_modifier_commits() {
        let mask = Modifiers::SUPER | Modifiers::SHIFT;
        assert_eq!(primary_modifier(mask), Modifiers::SUPER);
        assert_eq!(primary_modifier(Modifiers::empty()), Modifiers::empty());

        let mut popup = SwitcherPopup::show(SwitcherKind::Apps, 4, true, mask, mask, T0).unwrap();
        assert_eq!(popup.selected(), 3, "backward starts at the end");

        popup.key_release(Modifiers::SUPER, T0);
        assert_eq!(
            popup.outcome(),
            None,
            "Shift came up, but Super still holds it open"
        );

        popup.key_release(Modifiers::empty(), T0);
        assert_eq!(popup.outcome(), Some(SwitcherOutcome::Commit));
    }

    /// With no modifier to release, the popup commits on a timeout that re-arms on every key
    /// release (`:229-231`) — so it is 1500ms of *quiet*, not 1500ms from open.
    #[test]
    fn a_no_modifier_switcher_commits_after_a_quiet_period() {
        let mut popup = SwitcherPopup::show(
            SwitcherKind::Apps,
            4,
            false,
            Modifiers::empty(),
            Modifiers::empty(),
            T0,
        )
        .unwrap();

        let late = NO_MODS_TIMEOUT - Duration::from_millis(1);
        popup.poll(late);
        assert_eq!(popup.outcome(), None);

        // A key release here pushes the deadline out rather than leaving it where it was.
        popup.key_release(Modifiers::empty(), late);
        popup.poll(NO_MODS_TIMEOUT);
        assert_eq!(
            popup.outcome(),
            None,
            "the deadline runs from the last release"
        );

        popup.poll(late + NO_MODS_TIMEOUT);
        assert_eq!(popup.outcome(), Some(SwitcherOutcome::Commit));
    }

    /// Keyboard use parks the pointer, and it stays parked while typing continues.
    #[test]
    fn keyboard_use_suppresses_hover_until_things_go_quiet() {
        let mut popup = open(5);
        assert!(
            popup.hover_selects(),
            "a resting pointer selects until a key is pressed"
        );

        popup.key_press(SwitcherKey::Advance { backward: false }, T0);
        assert!(!popup.hover_selects());
        assert!(
            !popup.pointer_entered_item(4),
            "hover is ignored while suppressed"
        );
        assert_eq!(popup.selected(), 2);

        // Held Tab keeps re-arming, so hover does not come back mid-traversal.
        let mut t = T0;
        for _ in 0..5 {
            t += DISABLE_HOVER_TIMEOUT - Duration::from_millis(1);
            popup.poll(t);
            popup.key_press(SwitcherKey::Advance { backward: false }, t);
            assert!(!popup.hover_selects(), "still typing at {t:?}");
        }

        popup.poll(t + DISABLE_HOVER_TIMEOUT);
        assert!(popup.hover_selects());
        assert!(popup.pointer_entered_item(4));
        assert_eq!(popup.selected(), 4);
    }

    /// Escape leaves focus where it was; the caller must not activate anything.
    #[test]
    fn escape_cancels_rather_than_committing() {
        let mut popup = open(4);
        popup.poll(POPUP_DELAY);

        popup.key_press(SwitcherKey::Dismiss, POPUP_DELAY);

        assert_eq!(popup.outcome(), Some(SwitcherOutcome::Cancel));
        assert_eq!(popup.visibility(), Visibility::Fading);

        // A release arriving after the cancel must not turn it into a commit.
        popup.key_release(Modifiers::empty(), POPUP_DELAY);
        assert_eq!(popup.outcome(), Some(SwitcherOutcome::Cancel));
    }

    /// Windows vanish while the popup is up — `_itemRemovedHandler` (`:269-284`).
    ///
    /// Reachable in one step: F4 inside the switcher. Each branch shifts the selection
    /// differently, and the last removal ends the session rather than leaving an empty panel.
    #[test]
    fn removing_an_item_reselects_by_position() {
        // Below the selection: everything shifts up, so the selection follows.
        let mut popup = open(5);
        popup.select(3);
        popup.item_removed(1);
        assert_eq!((popup.len(), popup.selected()), (4, 2));

        // Above it: indices are unaffected and GNOME reselects nothing.
        let mut popup = open(5);
        popup.select(1);
        popup.item_removed(4);
        assert_eq!((popup.len(), popup.selected()), (4, 1));

        // At it: the slot is reused by whatever shifted into it.
        let mut popup = open(5);
        popup.select(2);
        popup.item_removed(2);
        assert_eq!((popup.len(), popup.selected()), (4, 2));

        // At it, and it was last: clamp back onto the new end.
        let mut popup = open(5);
        popup.select(4);
        popup.item_removed(4);
        assert_eq!((popup.len(), popup.selected()), (4, 3));
    }

    /// Closing the last window ends the session — and ends it as a *cancel*, because there is
    /// nothing left to activate and a commit would have to invent a target.
    #[test]
    fn removing_the_last_item_ends_the_session() {
        let mut popup = open(1);
        popup.item_removed(0);

        assert_eq!(popup.len(), 0);
        assert_eq!(popup.outcome(), Some(SwitcherOutcome::Cancel));
    }

    /// Traversal wraps in both directions (`mod`, `:21-23`).
    #[test]
    fn traversal_wraps_at_both_ends() {
        let mut popup = open(3);
        popup.select(2);
        popup.select_next();
        assert_eq!(popup.selected(), 0);
        popup.select_previous();
        assert_eq!(popup.selected(), 2);
    }

    /// An empty list opens nothing at all — no grab, no popup, and the binding is a no-op
    /// (`:123-124`).
    #[test]
    fn an_empty_list_does_not_open() {
        assert!(SwitcherPopup::show(SwitcherKind::Windows, 0, false, ALT, ALT, T0).is_none());
    }

    fn monitor() -> Rectangle<f64, Logical> {
        Rectangle::new(Point::from((0., 0.)), Size::from((1920., 1080.)))
    }

    /// Every item is the same square, however differently shaped its contents are.
    ///
    /// `squareItems` is true for both Tab popups (`altTab.js:698, 1064`), and the size is the max
    /// over *both* dimensions of *all* items (`switcherPopup.js:575-590`). Without it a window
    /// with a long title would widen its own tile and the row would look ragged.
    #[test]
    fn square_items_make_one_size_for_every_item() {
        let contents = [
            Size::from((128., 96.)),
            Size::from((40., 40.)),
            Size::from((60., 150.)),
        ];
        let layout = PanelLayout::new(&contents, monitor());

        // The tallest content (150) plus the item padding on both sides sets the square.
        let side = 150. + ITEM_PADDING * 2.;
        for (i, item) in layout.items.iter().enumerate() {
            assert_eq!(item.size, Size::from((side, side)), "item {i}");
        }

        // Panel = the row, plus its own padding.
        let row_w = 3. * side + 2. * ITEM_SPACING;
        assert_eq!(
            layout.panel.size,
            Size::from((row_w + LIST_PADDING * 2., side + LIST_PADDING * 2.))
        );
    }

    /// The panel is centered on both axes, and the items sit inside its padding.
    #[test]
    fn the_panel_is_centered_on_the_monitor() {
        let layout = PanelLayout::new(&[Size::from((96., 96.)); 4], monitor());
        let panel = layout.panel;

        assert_eq!(
            panel.loc.x + panel.size.w / 2.,
            960.,
            "horizontally centered"
        );
        assert_eq!(panel.loc.y + panel.size.h / 2., 540., "vertically centered");

        let first = layout.items[0];
        assert_eq!(first.loc.x, panel.loc.x + LIST_PADDING);
        assert_eq!(first.loc.y, panel.loc.y + LIST_PADDING);

        // The last item's far edge lands on the panel's inner edge — no drift from accumulating
        // the spacing.
        let last = layout.items[3];
        assert_eq!(
            last.loc.x + last.size.w,
            panel.loc.x + panel.size.w - LIST_PADDING
        );
    }

    /// The gaps between items belong to no item, so crossing one selects nothing.
    ///
    /// GNOME hangs `item-entered` off each `SwitcherButton`'s motion handler
    /// (`switcherPopup.js:487-489`) rather than dividing the row into hit zones.
    #[test]
    fn the_spacing_between_items_is_not_hoverable() {
        let layout = PanelLayout::new(&[Size::from((96., 96.)); 3], monitor());

        let first = layout.items[0];
        let mid_y = first.loc.y + first.size.h / 2.;

        assert_eq!(
            layout.item_at(Point::from((first.loc.x + 1., mid_y))),
            Some(0)
        );

        // A point in the gap after the first item.
        let gap_x = first.loc.x + first.size.w + ITEM_SPACING / 2.;
        assert_eq!(layout.item_at(Point::from((gap_x, mid_y))), None);

        assert_eq!(
            layout.item_at(Point::from((layout.items[2].loc.x + 1., mid_y))),
            Some(2)
        );

        // Outside the panel entirely.
        assert_eq!(layout.item_at(Point::from((0., 0.))), None);
    }

    /// An empty layout is empty rather than a zero-sized panel someone might still paint.
    #[test]
    fn no_items_means_no_panel() {
        let layout = PanelLayout::new(&[], monitor());
        assert!(layout.items.is_empty());
        assert_eq!(layout.panel.size, Size::from((0., 0.)));
    }
}
