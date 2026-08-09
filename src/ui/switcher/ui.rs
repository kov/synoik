// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

//! The live switcher surface: the state machine of [`super::SwitcherPopup`] plus the items,
//! geometry and textures needed to actually draw it.
//!
//! One holder serves both popups. GNOME has two `SwitcherPopup` subclasses, but everything that
//! is not *item art* — the grab, the timers, keynav, commit, placement, the panel — is identical
//! between them, so the split lives in [`Items`] rather than in two parallel surfaces.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use smithay::backend::renderer::element::Kind;
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Size, Transform};
use synoik_config::{Config, Modifiers};

use crate::animation::{Animation, Clock, Curve};
use crate::app_system::AppIconRef;
use crate::render_helpers::inset_ring::InsetRing;
use crate::render_helpers::solid_color::SolidColorRenderElement;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::VkTexture;
use crate::render_helpers::window_thumbnail::{self, WindowThumbnailRenderElement};
use crate::render_helpers::RenderCtx;
use crate::synoik_render_elements;
use crate::ui::switcher::app_switcher::{self, AppItem};
use crate::ui::switcher::{
    thumbnails, window_switcher, PanelLayout, SwitcherKey, SwitcherKind, SwitcherOutcome,
    SwitcherPopup, Visibility, ARROW, ARROW_HIGHLIGHTED, ITEM_HIGHLIGHTED, ITEM_PADDING,
    ITEM_RADIUS, ITEM_SELECTED, LIST_BG, LIST_BORDER, LIST_BORDER_COLOR, LIST_FG, LIST_RADIUS,
    POPUP_SPACING,
};
use crate::ui::widget::{
    self, style, AppIconUploads, BakeCache, Painter, ShapedText, TextShaper, TextStyle,
};
use crate::window::mapped::MappedId;

/// `.alt-tab-app`'s label — `$base_font_size` (`_common.scss:30`), the shell's default. The
/// switcher sets no font size of its own, so the item label is plain body text.
const LABEL_PT: f64 = 11.;

/// `.cycler-highlight`'s `border: 5px solid -st-accent-color` (`_switcher-popup.scss:80-82`).
const CYCLER_HIGHLIGHT_BORDER: f64 = 5.;

/// The height an item's label occupies, in logical px — what the icon ladder measures against.
pub fn label_height() -> f64 {
    crate::ui::pt_to_px(LABEL_PT)
}

synoik_render_elements! {
    SwitcherRenderElement => {
        // The panel, its labels, and the app icons over it.
        Texture = TextureRenderElement<VkTexture>,
        // A window switcher's live previews.
        Thumbnail = WindowThumbnailRenderElement,
        // A cycler's `.cycler-highlight` border, drawn around the window itself.
        Solid = SolidColorRenderElement,
    }
}

/// What the switcher is cycling through.
///
/// The two variants are the two popup classes. An app item carries *all* of that app's windows
/// because committing on it activates one of them, and slice 5's thumbnail sub-list walks them.
#[derive(Debug, Clone)]
pub enum Items {
    Apps(Vec<AppItem>),
    /// `WindowSwitcherPopup` — slice 3.
    Windows(Vec<MappedId>),
    /// `CyclerPopup` (`altTab.js:487-540`) — `cycle-windows` and `cycle-group`.
    ///
    /// Same popup machinery, no popup: `CyclerList` draws nothing, and the selection is shown by
    /// raising the window itself and putting a `.cycler-highlight` border around it. So this
    /// variant deliberately has no [`ItemArt`], no [`PanelLayout`] and no sub-list.
    ///
    /// `group` is which of the two bindings opened it, because a cycler only advances on *its
    /// own* action: `_keyPressHandler` matches one `Meta.KeyBindingAction` and propagates
    /// everything else (`altTab.js:544-554` for the group cycler, `:657-666` for the window one).
    Cycler {
        windows: Vec<MappedId>,
        group: bool,
    },
}

impl Items {
    pub fn len(&self) -> usize {
        match self {
            Items::Apps(items) => items.len(),
            Items::Windows(ids) => ids.len(),
            Items::Cycler { windows, .. } => windows.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn kind(&self) -> SwitcherKind {
        match self {
            Items::Apps(_) => SwitcherKind::Apps,
            Items::Windows(_) => SwitcherKind::Windows,
            Items::Cycler { .. } => SwitcherKind::Cycler,
        }
    }

    /// The windows a commit on item `n` brings forward, in [`Activation`] order.
    ///
    /// An app item gives up **all** of its windows, because its commit is
    /// `shell_app_activate_window` (`shell-app.c:381-442`) — the whole app comes forward, with the
    /// most recently used window focused on top. `windows` is in tab-list order, which is already
    /// topmost-first.
    fn activation_at(&self, n: usize) -> Activation {
        match self {
            Items::Apps(items) => items.get(n).map_or_else(Vec::new, |a| a.windows.clone()),
            Items::Windows(ids) | Items::Cycler { windows: ids, .. } => {
                ids.get(n).copied().into_iter().collect()
            }
        }
    }
}

/// The windows an activation brings forward, **topmost first**: `[0]` takes focus, and the rest
/// are raised underneath it in that order.
///
/// Longer than one element only for the app switcher's own commit. Every other path activates a
/// lone window: `Main.activateWindow` is what a picked thumbnail (`altTab.js:285`), the window
/// switcher (`:632`) and a cycler (`:525`) all end in.
pub type Activation = Vec<MappedId>;

/// Everything one item needs on screen, resolved once when the popup opens.
#[derive(Debug, Clone)]
pub struct ItemArt {
    pub icon: Option<AppIconRef>,
    pub label: String,
    pub arrow: bool,
    /// An app item's window titles, in the same order as [`AppItem::windows`], for the thumbnail
    /// sub-list's captions. Empty for a window item, which has no sub-list.
    ///
    /// Resolved at open like everything else here, because the sub-list is built *lazily* — half a
    /// second after the selection lands on the app — and by then the UI has no way back to the
    /// window model. GNOME reads `window.get_title()` at that later moment
    /// (`ThumbnailSwitcher._init`, `altTab.js:933`), so a title that changes while the popup is up
    /// updates there and not here; the same freeze already applies to the app's window *list*,
    /// which GNOME caches at open and says so (`:719-720`).
    pub window_titles: Vec<String>,
}

/// Everything [`SwitcherUi::open`] needs, gathered by the caller.
///
/// A struct rather than a long argument list because the caller assembles these from four
/// different places — the app system, the tab list, the seat's modifier state and the output —
/// and positional arguments of the same type (two `Modifiers`, two `f64`) are trivially
/// swappable at a call site.
pub struct OpenRequest {
    pub items: Items,
    pub art: Vec<ItemArt>,
    /// `binding.is_reversed()` — the `-backward` half of the switch binding.
    pub backward: bool,
    /// The binding's modifiers, whose primary bit commits on release.
    pub mask: Modifiers,
    /// What is actually held right now, for the release race.
    pub held: Modifiers,
    pub output: Output,
    /// The **primary** monitor's logical rect — GNOME centers on it, not on the focused output.
    pub monitor: Rectangle<f64, Logical>,
    pub label_height: f64,
}

struct Open {
    state: SwitcherPopup,
    items: Items,
    art: Vec<ItemArt>,
    layout: PanelLayout,
    output: Output,
    icon_px: f64,
    /// Kept so the row can be re-centered when an item disappears.
    monitor: Rectangle<f64, Logical>,
    label_height: f64,
    /// The window sub-list, while it is up — `AppSwitcherPopup._thumbnails` (`altTab.js:66`).
    thumbs: Option<Thumbs>,
    /// When a resting selection pops its own sub-list (`_thumbnailTimeoutId`, `:349-356`).
    thumb_deadline: Option<Duration>,
    /// The focus target the workspace preview has settled on — what the screen is showing.
    ws_preview: Option<MappedId>,
    /// The focus target the selection wants, which is a different answer from `ws_preview` for as
    /// long as the dwell runs. Keeping both is the dwell: see [`WORKSPACE_PREVIEW_DELAY`].
    ws_wanted: Option<MappedId>,
    /// When `ws_wanted` takes over. Re-armed from scratch whenever the selection moves again, so
    /// holding Tab down never settles anywhere — a dwell, not a rate limit.
    ws_deadline: Option<Duration>,
}

/// The app switcher's open window sub-list — see [`thumbnails`].
struct Thumbs {
    windows: Vec<MappedId>,
    titles: Vec<String>,
    layout: thumbnails::ThumbLayout,
    /// `_currentWindow`, whose `-1` is our `None`: the list can be up with **nothing** picked in
    /// it, which is what the popup timer produces (`_timeoutPopupThumbnails`, `:359-364` leaves it
    /// unset). That state still commits to the app's first window, so the distinction is not
    /// cosmetic — it is the difference between "here are the windows" and "this window".
    selected: Option<usize>,
    /// `_thumbnailsFocused` — whether the arrows act on the sub-list or on the app row above it.
    focused: bool,
    /// 0 -> 1 on the way in, 1 -> 0 on the way out (`THUMBNAIL_FADE_TIME`, `altTab.js:366-408`).
    /// Element-level alpha only: it must never reach the bake key, or an animating list would
    /// re-bake its plate and every caption each frame.
    fade: Animation,
}

impl Open {
    /// One item's content box. The app switcher stacks its label under the icon inside the item
    /// (`AppIcon` adds it as a child, `altTab.js:682-686`); the window switcher does **not** — its
    /// `WindowIcon` label is only handed to `addItem` as the accessible `label_actor`
    /// (`switcherPopup.js:460`), and the visible title lives in the panel footer instead.
    fn content_size(&self) -> Size<f64, Logical> {
        Size::from((self.icon_px, self.icon_px + self.item_label_height()))
    }

    fn item_label_height(&self) -> f64 {
        match self.items {
            Items::Apps(_) => self.label_height,
            Items::Windows(_) | Items::Cycler { .. } => 0.,
        }
    }

    /// The height of the panel-wide title label, or 0 for a popup that has none.
    fn footer_height(&self) -> f64 {
        match self.items {
            Items::Apps(_) | Items::Cycler { .. } => 0.,
            Items::Windows(_) => self.label_height,
        }
    }

    /// What committing *right now* would bring forward — the answer both the commit and the
    /// preview need, so neither can drift from the other.
    fn activation(&self) -> Activation {
        // `_finish` (`altTab.js:282-285`): a picked thumbnail goes through `Main.activateWindow`,
        // which is one window and no group raise. Only the app row's own commit is
        // `app.activate_window`, and only that brings the rest of the app with it.
        if let Some(picked) = self
            .thumbs
            .as_ref()
            .and_then(|t| Some(*t.windows.get(t.selected?)?))
        {
            return vec![picked];
        }
        self.items.activation_at(self.state.selected())
    }

    /// Re-arm the workspace-preview dwell when the selection has moved off what it settled on.
    ///
    /// Called after *every* mutation rather than from each of the handful of places a selection
    /// can move — the app row, the sub-list, hover, a removed item — because a path that forgot to
    /// call it would leave the preview pointing at a window that is no longer selected, which is
    /// exactly the failure the dwell exists to avoid.
    fn note_ws_preview(&mut self, now: Duration) {
        let want = self.preview_focus();
        if want != self.ws_wanted {
            self.ws_wanted = want;
            // Moving *back* onto what is already showing cancels the pending switch outright
            // rather than counting down to a no-op.
            self.ws_deadline =
                (want != self.ws_preview).then(|| now + super::WORKSPACE_PREVIEW_DELAY);
        }

        if self.ws_deadline.is_some_and(|at| now >= at) {
            self.ws_deadline = None;
            self.ws_preview = self.ws_wanted;
        }
    }

    /// The window a commit would focus — the head of the activation, and the window whose
    /// workspace the preview follows.
    fn preview_focus(&self) -> Option<MappedId> {
        self.preview_windows().first().copied()
    }

    /// What this popup is showing you it would raise, or nothing while it is still invisible.
    ///
    /// The delay is what makes a quick tap switch with no UI at all (see [`Visibility`]), so it
    /// has to gate the preview too — a tap that slid the screen around on its way through would
    /// be louder than the popup it was avoiding. A cycler is exempt: raising the window *is* its
    /// entire UI, and it starts from `_initialSelection` inside `show()`.
    fn preview_windows(&self) -> Activation {
        if self.state.visibility() == Visibility::Pending
            && !matches!(self.items, Items::Cycler { .. })
        {
            return Activation::new();
        }
        self.activation()
    }

    /// The selected app's windows, or empty for a window switcher (which has no sub-list).
    fn selected_windows(&self) -> &[MappedId] {
        match &self.items {
            Items::Apps(apps) => apps
                .get(self.state.selected())
                .map_or(&[][..], |a| &a.windows),
            Items::Windows(_) | Items::Cycler { .. } => &[],
        }
    }

    /// Build the sub-list for the current selection — `_createThumbnails` (`altTab.js:381-408`).
    ///
    /// `selected`/`focused` are `_select`'s two arguments: the timer opens it with nothing picked
    /// and the app row still holding the arrows, while Down opens it on window 0 with the arrows
    /// moved into it.
    fn open_thumbs(&mut self, selected: Option<usize>, focused: bool, fade: Animation) {
        let windows = self.selected_windows().to_vec();
        if windows.is_empty() {
            return;
        }

        let titles = self
            .art
            .get(self.state.selected())
            .map_or_else(Vec::new, |a| a.window_titles.clone());

        // `addClones` measures against the room left below the sub-list's own top edge.
        let top = self.layout.panel.loc.y + self.layout.panel.size.h;
        let available = self.monitor.loc.y + self.monitor.size.h - (top + POPUP_SPACING);
        let thumb_h = thumbnails::thumb_height(available, self.label_height);
        let anchor = self.layout.items.get(self.state.selected()).map_or(
            self.layout.panel.loc.x + self.layout.panel.size.w / 2.,
            |r| r.loc.x + r.size.w / 2.,
        );

        self.thumb_deadline = None;
        self.thumbs = Some(Thumbs {
            layout: thumbnails::layout(
                windows.len(),
                thumb_h,
                self.label_height,
                anchor,
                top,
                self.monitor,
            ),
            windows,
            titles,
            selected,
            focused,
            fade,
        });
    }

    /// Point the sub-list at `window`, **creating it only if it is not already up** — the window
    /// half of `_select` (`altTab.js:328-357`).
    ///
    /// `_select` destroys the thumbnails only when the app changed or the window is null
    /// (`:329-332`); with the same app and a real window it falls straight through to
    /// `this._thumbnails.highlight(window, ...)` on the list that is already on screen. So
    /// descending into a list the popup timer opened must move a highlight and nothing else.
    ///
    /// Rebuilding instead is invisible to every value-based test — same layout, same windows,
    /// same selection — but it hands the new list a fresh [`thumbnails::FADE_TIME`] animation, so
    /// a list that was fully opaque blinks out and eases back in under the key that was supposed
    /// to move a highlight.
    fn select_thumb(&mut self, window: usize, focused: bool, fade: Animation) {
        self.thumb_deadline = None;
        let Some(thumbs) = self.thumbs.as_mut() else {
            self.open_thumbs(Some(window), focused, fade);
            return;
        };
        thumbs.selected = Some(window);
        thumbs.focused = focused;
    }

    /// `ThumbnailSwitcher._removeThumbnail` (`altTab.js:978-991`) — a window closed while its
    /// preview was on screen.
    ///
    /// The reselect is `mod(index, len)`, not `min(index, len - 1)`: closing the last preview
    /// wraps to the first rather than stepping back one. And an emptied sub-list destroys itself,
    /// which cannot strand the popup — the app it belonged to loses its item in the same pass.
    fn thumbnail_removed(&mut self, id: MappedId) {
        let Some(thumbs) = self.thumbs.as_mut() else {
            return;
        };
        let Some(at) = thumbs.windows.iter().position(|&w| w == id) else {
            return;
        };

        thumbs.windows.remove(at);
        if at < thumbs.titles.len() {
            thumbs.titles.remove(at);
        }
        if thumbs.windows.is_empty() {
            self.thumbs = None;
            return;
        }

        thumbs.selected = Some(at % thumbs.windows.len());
        // Re-measure: the row is one preview narrower, so it re-centres on the app above it. The
        // fade carries over — the list did not reappear, it lost a preview.
        let focused = thumbs.focused;
        let selected = thumbs.selected;
        let fade = thumbs.fade.clone();
        self.open_thumbs(selected, focused, fade);
    }

    /// Drop the sub-list and re-arm its timer if the (possibly new) selection deserves one —
    /// the `window == null` half of `_select` (`altTab.js:328-356`).
    ///
    /// `rearm` is `!forceAppFocus`: closing the list with Up must not immediately re-open it.
    fn close_thumbs(&mut self, now: Duration, rearm: bool) -> Option<Thumbs> {
        let going = self.thumbs.take();
        self.thumb_deadline =
            (rearm && self.selected_windows().len() > 1).then(|| now + thumbnails::POPUP_TIME);
        going
    }
}

/// The switcher surface — at most one is open at a time, as in GNOME (a new `_startSwitcher`
/// destroys any popup already up, `windowManager.js:1699-1701`).
pub struct SwitcherUi {
    clock: Clock,
    config: Rc<RefCell<Config>>,
    open: Option<Open>,
    /// The sub-list on its way out, still drawn while it fades — `_destroyThumbnails` clears
    /// `_thumbnails` immediately and lets the old actor ease itself away (`altTab.js:366-379`), so
    /// a new list can be building while this one is still on screen.
    fading: Option<Thumbs>,
    /// Set on the `Pending` -> `Shown` edge, cleared by [`SwitcherUi::take_just_shown`].
    just_shown: bool,
    chrome: RefCell<BakeCache>,
    /// The sub-list's own bake. A second cache rather than a second key in the first: the two
    /// panels are different sizes and change on different beats, so sharing one would re-bake the
    /// app row every time a preview selection moved.
    thumb_chrome: RefCell<BakeCache>,
    icons: RefCell<AppIconUploads>,
    /// `.cycler-highlight` — the whole visible UI of a cycler, kept across frames so a highlight
    /// that has not moved does not re-damage its four edges.
    cycler_ring: RefCell<InsetRing>,
}

impl SwitcherUi {
    pub fn new(clock: Clock, config: Rc<RefCell<Config>>) -> Self {
        Self {
            clock,
            config,
            open: None,
            fading: None,
            just_shown: false,
            chrome: RefCell::new(BakeCache::default()),
            thumb_chrome: RefCell::new(BakeCache::default()),
            icons: RefCell::new(AppIconUploads::default()),
            cycler_ring: RefCell::new(InsetRing::new()),
        }
    }

    /// `THUMBNAIL_FADE_TIME` as an animation, or an instant one when animations are off — the
    /// only config knob any switcher timing honours, and the same treatment the OSD gives its
    /// own fixed GNOME durations.
    fn fade(&self, from: f64, to: f64) -> Animation {
        let ms = if self.config.borrow().animations.off {
            0
        } else {
            thumbnails::FADE_TIME.as_millis() as u64
        };
        Animation::ease(self.clock.clone(), from, to, 0., ms, Curve::EaseOutQuad)
    }

    /// Whether a sub-list fade is still running, so the compositor keeps drawing frames for it.
    ///
    /// Without this the fade only advances when something *else* forces a frame, which on a
    /// switcher — where the next event is usually the key that ends the session — means it never
    /// visibly runs at all.
    pub fn are_animations_ongoing(&self) -> bool {
        self.fading.is_some()
            || self
                .open
                .as_ref()
                .and_then(|o| o.thumbs.as_ref())
                .is_some_and(|t| !t.fade.is_done())
    }

    /// Send a sub-list on its way out, keeping it on screen until the fade finishes. Fading from
    /// wherever it currently is, so a list dismissed mid-appearance does not jump to opaque first.
    fn retire_thumbs(&mut self, going: Option<Thumbs>) {
        let Some(mut going) = going else {
            return;
        };
        going.fade = self.fade(going.fade.value(), 0.);
        self.fading = Some(going);
    }

    /// How opaque the open sub-list is right now, for tests: 1.0 once it has faded in.
    pub fn thumbnail_alpha(&self) -> Option<f64> {
        Some(self.open.as_ref()?.thumbs.as_ref()?.fade.value())
    }

    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// The selected item's label — its app name or window title.
    ///
    /// What a screen reader announces as the selection moves: `WindowIcon`/`AppIcon` give each
    /// item an `St.Label`, and Orca reads it.
    pub fn selected_label(&self) -> Option<&str> {
        let open = self.open.as_ref()?;
        Some(open.art.get(open.state.selected())?.label.as_str())
    }

    /// How many items the open popup is showing, or `None` when nothing is open.
    ///
    /// Deliberately not a `len`/`is_empty` pair: an *open* switcher is never empty (the last item
    /// going away ends the session), so "empty" and "closed" are the same state and one `Option`
    /// says it once.
    pub fn item_count(&self) -> Option<usize> {
        Some(self.open.as_ref()?.items.len())
    }

    /// The `.switcher-list` panel's box, for tests that need to look at the pixels it covers.
    pub fn panel_rect(&self) -> Option<Rectangle<f64, Logical>> {
        Some(self.open.as_ref()?.layout.panel)
    }

    /// Where item `i` sits, in the same layout the renderer drew from. The per-item counterpart of
    /// [`panel_rect`](Self::panel_rect), and for the same reason: a pixel test that guessed at an
    /// item's position would be testing its own arithmetic.
    pub fn item_rect(&self, i: usize) -> Option<Rectangle<f64, Logical>> {
        self.open.as_ref()?.layout.items.get(i).copied()
    }

    /// The window `w`/`F4` closes — `_closeWindow` (`altTab.js:610-616`) for a window switcher,
    /// `_closeAppWindow` (`:157-167`) for an app one.
    ///
    /// An app switcher answers **only while its sub-list has the focus**: GNOME puts the key in
    /// the `_thumbnailsFocused` branch (`:203-208`), so `w` on the app row does nothing rather
    /// than closing some window of it you cannot see. A sub-list that merely popped up on the
    /// timer has nothing picked, and GNOME lands in the same place from the other direction —
    /// `_closeAppWindow` looks up `cachedWindows[-1]` and returns on the miss (`:157-167`).
    pub fn close_target(&self) -> Option<MappedId> {
        let open = self.open.as_ref()?;
        match &open.items {
            Items::Windows(ids) => ids.get(open.state.selected()).copied(),
            // `CyclerPopup` binds no close key at all.
            Items::Cycler { .. } => None,
            Items::Apps(_) => {
                let thumbs = open.thumbs.as_ref().filter(|t| t.focused)?;
                thumbs.windows.get(thumbs.selected?).copied()
            }
        }
    }

    /// The app `q` quits — `_quitApplication` (`altTab.js:169-175`). `None` for a window
    /// switcher, whose items are windows and which has no `q` binding at all.
    pub fn quit_target(&self) -> Option<&str> {
        let open = self.open.as_ref()?;
        match &open.items {
            Items::Apps(apps) => Some(apps.get(open.state.selected())?.app_id.as_str()),
            Items::Windows(_) | Items::Cycler { .. } => None,
        }
    }

    /// Whether the app switcher's window sub-list is up.
    pub fn thumbnails_open(&self) -> bool {
        self.open.as_ref().is_some_and(|o| o.thumbs.is_some())
    }

    /// Which window the sub-list has picked — `None` both when there is no sub-list and when it
    /// is up with nothing picked (`_currentWindow === -1`), which are different states with the
    /// same answer here. Pair it with [`thumbnails_open`](Self::thumbnails_open) to tell them
    /// apart.
    pub fn thumbnail_selected(&self) -> Option<usize> {
        self.open.as_ref()?.thumbs.as_ref()?.selected
    }

    /// The sub-list's panel box, for tests that need the pixels it covers.
    pub fn thumbnail_panel_rect(&self) -> Option<Rectangle<f64, Logical>> {
        Some(self.open.as_ref()?.thumbs.as_ref()?.layout.panel)
    }

    /// Where preview `i` is drawn, in the layout the renderer used.
    pub fn thumbnail_rect(&self, i: usize) -> Option<Rectangle<f64, Logical>> {
        self.open
            .as_ref()?
            .thumbs
            .as_ref()?
            .layout
            .thumbs
            .get(i)
            .copied()
    }

    /// The panel-wide title label's box — the window switcher's, and `None` for the app switcher,
    /// which labels each item instead.
    pub fn footer_rect(&self) -> Option<Rectangle<f64, Logical>> {
        self.open.as_ref()?.layout.footer
    }

    /// The output the popup is on, so the compositor knows what to redraw.
    pub fn output(&self) -> Option<&Output> {
        self.open.as_ref().map(|o| &o.output)
    }

    /// What the open switcher is showing you it would raise, in [`Activation`] order — the
    /// same list its commit would act on, so the preview cannot promise something the commit will
    /// not do.
    ///
    /// DIVERGENCE: GNOME previews nothing outside the cycler. Empty while the popup is still
    /// inside its [`POPUP_DELAY`](super::POPUP_DELAY), because the tap that never shows a popup
    /// must not disturb the screen on its way through either — the cycler is exempt, since
    /// raising the window *is* its whole UI and it starts doing so from `_initialSelection`.
    pub fn preview_windows(&self) -> Activation {
        self.open
            .as_ref()
            .map_or_else(Activation::new, Open::preview_windows)
    }

    /// The window whose workspace the preview has settled on, once it has rested there for
    /// [`WORKSPACE_PREVIEW_DELAY`](super::WORKSPACE_PREVIEW_DELAY).
    ///
    /// DIVERGENCE, and the only part of a preview that touches state the user can see afterwards:
    /// the compositor takes that window's monitor to its workspace, remembers where it was, and
    /// puts it back if the session is abandoned.
    pub fn workspace_preview(&self) -> Option<MappedId> {
        self.open.as_ref()?.ws_preview
    }

    /// The window a running cycler is showing — `CyclerHighlight.window` (`altTab.js:433-457`).
    ///
    /// `None` for every other popup and when nothing is up, which is also the signal to drop the
    /// layout's raise: no cycler, no window pinned above its neighbours.
    pub fn cycler_window(&self) -> Option<MappedId> {
        let open = self.open.as_ref()?;
        let Items::Cycler { windows, .. } = &open.items else {
            return None;
        };
        windows.get(open.state.selected()).copied()
    }

    /// Whether the running cycler is the *group* one, so the compositor can tell which of the two
    /// cycle actions advances it and which is ignored (`altTab.js:544-554`, `:657-666`).
    pub fn cycler_is_group(&self) -> Option<bool> {
        match &self.open.as_ref()?.items {
            Items::Cycler { group, .. } => Some(*group),
            _ => None,
        }
    }

    /// `.cycler-highlight`: a 5px `-st-accent-color` border around the cycled window
    /// (`_switcher-popup.scss:80-82`), drawn inside its frame rect like any St border.
    ///
    /// Pushed by the compositor at the top of the window layer rather than with the rest of the
    /// switcher, because that is where `CyclerHighlight` lives — `global.window_group`
    /// (`altTab.js:498`), under the panel, not over it.
    pub fn render_cycler(
        &self,
        rect: Rectangle<f64, Logical>,
        accent: [u8; 3],
        push: &mut dyn FnMut(SolidColorRenderElement),
    ) {
        if self.cycler_window().is_none() {
            return;
        }
        let mut ring = self.cycler_ring.borrow_mut();
        ring.update(
            rect,
            CYCLER_HIGHLIGHT_BORDER,
            synoik_config::Color::new_unpremul(
                f32::from(accent[0]) / 255.,
                f32::from(accent[1]) / 255.,
                f32::from(accent[2]) / 255.,
                1.,
            ),
        );
        ring.render(push);
    }

    pub fn selected(&self) -> Option<usize> {
        self.open.as_ref().map(|o| o.state.selected())
    }

    /// Whether anything is actually on screen — a popup inside its open delay is live but
    /// invisible, and must not force a redraw or a backdrop.
    pub fn is_visible(&self) -> bool {
        self.open
            .as_ref()
            .is_some_and(|o| o.state.visibility() != Visibility::Pending)
    }

    /// Open a switcher, or return the outcome if it finished immediately.
    ///
    /// An immediate finish is not an error: it is the modifier-already-released race
    /// (`switcherPopup.js:144-155`), and the caller must still activate the selection — which is
    /// why this returns the same `(outcome, activation)` pair every other entry point does. It
    /// used to return the outcome alone, and the tap that lost its race quietly switched to
    /// nothing.
    pub fn open(
        &mut self,
        req: OpenRequest,
        now: Duration,
    ) -> Option<(SwitcherOutcome, Activation)> {
        debug_assert!(
            matches!(req.items, Items::Cycler { .. }) || req.items.len() == req.art.len()
        );

        let OpenRequest {
            items,
            art,
            backward,
            mask,
            held,
            output,
            monitor,
            label_height,
        } = req;

        let state = SwitcherPopup::show(items.kind(), items.len(), backward, mask, held, now)?;

        // The app switcher walks its icon ladder to make the row fit; the window switcher has no
        // `_setIconSize` override, so its previews are always `WINDOW_PREVIEW_SIZE` and a row too
        // wide for the screen simply overflows (GNOME scrolls it; see `SCROLL_TIME`).
        let icon_px = match items {
            Items::Apps(_) => app_switcher::icon_size(
                items.len(),
                label_height,
                app_switcher::available_width(monitor.size.w),
            ),
            Items::Windows(_) => window_switcher::WINDOW_PREVIEW_SIZE,
            // Nothing is drawn in a panel, so there is no icon to size.
            Items::Cycler { .. } => 0.,
        };
        let mut open = Open {
            state,
            items,
            art,
            // Measured immediately below, once `open` can answer where its labels go.
            layout: PanelLayout::default(),
            output,
            icon_px,
            monitor,
            label_height,
            thumbs: None,
            thumb_deadline: None,
            ws_preview: None,
            ws_wanted: None,
            ws_deadline: None,
        };
        // A cycler draws no panel, and must not measure one either: an empty `PanelLayout` is
        // also what keeps the pointer from hit-testing item rects that are not on screen.
        if !matches!(open.items, Items::Cycler { .. }) {
            open.layout = PanelLayout::new(
                &vec![open.content_size(); open.items.len()],
                monitor,
                open.footer_height(),
            );
        }
        // `_initialSelection` goes through `_select` like every later move, so a popup that opens
        // on a multi-window app is already counting down to its sub-list (`altTab.js:349-356`).
        open.close_thumbs(now, true);
        self.fading = None;
        self.open = Some(open);

        // Read through the popup rather than from `state.outcome()` directly, so a race lost at
        // open still commits to the window the popup would have started on. `take_outcome` clears
        // `self.open` itself; nothing was ever drawn, so there is no fade to wait out.
        self.take_outcome()
    }

    /// Drive the timers. Returns an outcome the moment the session ends.
    pub fn advance(&mut self, now: Duration) -> Option<(SwitcherOutcome, Activation)> {
        // Retire a list that has finished fading away. Before the early return below, so a fade
        // that outlives its popup still ends.
        if self.fading.as_ref().is_some_and(|t| t.fade.is_done()) {
            self.fading = None;
        }

        let fade_in = self.fade(0., 1.);
        let open = self.open.as_mut()?;
        let was = open.state.visibility();
        open.state.poll(now);

        // `_timeoutPopupThumbnails` (`altTab.js:359-364`): the sub-list appears with **nothing**
        // picked in it and the arrows still on the app row, so resting on an app shows you its
        // windows without changing what a release would activate.
        if open.thumb_deadline.is_some_and(|at| now >= at) {
            open.thumb_deadline = None;
            open.open_thumbs(None, false, fade_in);
        }

        // The workspace preview is driven from here alone, rather than pushed from each of the
        // half-dozen places a selection can move: this runs every frame with a real `now`, and a
        // path that forgot to re-arm the dwell would leave the screen on a workspace the
        // selection has already left.
        open.note_ws_preview(now);

        self.note_shown(was);
        self.take_outcome()
    }

    /// Whether the popup became visible since this was last asked.
    ///
    /// `_showImmediately` hides every OSD as it raises the popup (`switcherPopup.js:178`), so the
    /// two are never on screen together. Reported as an edge rather than done inside the state
    /// machine because the OSD lives in the compositor, not in here.
    pub fn take_just_shown(&mut self) -> bool {
        std::mem::take(&mut self.just_shown)
    }

    fn note_shown(&mut self, was: Visibility) {
        if was == Visibility::Pending
            && self
                .open
                .as_ref()
                .is_some_and(|o| o.state.visibility() == Visibility::Shown)
        {
            self.just_shown = true;
        }
    }

    /// When [`advance`](Self::advance) next has something to do.
    pub fn next_deadline(&self) -> Option<Duration> {
        let open = self.open.as_ref()?;
        [
            open.state.next_deadline(),
            open.thumb_deadline,
            open.ws_deadline,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    pub fn key_press(
        &mut self,
        key: SwitcherKey,
        now: Duration,
    ) -> Option<(SwitcherOutcome, Activation)> {
        let fade_in = self.fade(0., 1.);
        let mut going = None;
        let open = self.open.as_mut()?;
        let was = open.state.visibility();

        // `CyclerPopup` subclasses handle *only* their own cycle action and propagate everything
        // else (`altTab.js:544-554`, `:657-666`). The base then acts on Escape and the explicit
        // commit keys and ignores the rest — so the arrows, `w` and `q` do nothing at all here.
        // There is no list to walk and no item to act on.
        if matches!(open.items, Items::Cycler { .. }) {
            match key {
                SwitcherKey::Left
                | SwitcherKey::Right
                | SwitcherKey::Up
                | SwitcherKey::Down
                | SwitcherKey::Group { .. }
                | SwitcherKey::CloseWindow
                | SwitcherKey::QuitApp => {}
                key => open.state.key_press(key, now),
            }
            self.note_shown(was);
            return self.take_outcome();
        }

        // `switch-group` is our window switcher over one app (see `State::switch_group`), so on a
        // window list its own binding just walks the list — there is no sub-list to descend into
        // and nothing else it could sensibly mean.
        let key = match key {
            SwitcherKey::Group { backward } if matches!(open.items, Items::Windows(_)) => {
                SwitcherKey::Advance { backward }
            }
            key => key,
        };

        // `AppSwitcherPopup._keyPressHandler` (`altTab.js:190-208`) takes the arrows before the
        // base does, and what they mean depends on where the focus is: with the sub-list focused
        // they walk *its* windows and Up leaves it, otherwise Down descends into it.
        let focused = open.thumbs.as_ref().is_some_and(|t| t.focused);
        match key {
            SwitcherKey::Left | SwitcherKey::Right if focused => {
                let thumbs = open.thumbs.as_mut().unwrap();
                let n = thumbs.windows.len();
                thumbs.selected = Some(match key {
                    SwitcherKey::Left => thumbnails::previous_window(thumbs.selected, n),
                    _ => thumbnails::next_window(thumbs.selected, n),
                });
                open.state.disable_hover(now);
                open.state.show_immediately();
            }
            // `_select(selectedIndex, null, true)`: window `null` destroys the list, and
            // `forceAppFocus` is what stops the timer from putting it straight back.
            SwitcherKey::Up if focused => {
                going = open.close_thumbs(now, false);
                open.state.disable_hover(now);
                open.state.show_immediately();
            }
            // `switch-group` while up (`altTab.js:180-186`): forward descends into the sub-list
            // on its first press and steps through the windows after; backward always steps back.
            SwitcherKey::Group { backward } => {
                if !focused && !backward {
                    open.select_thumb(0, true, fade_in);
                } else if let Some(thumbs) = open.thumbs.as_mut() {
                    let n = thumbs.windows.len();
                    thumbs.selected = Some(if backward {
                        thumbnails::previous_window(thumbs.selected, n)
                    } else {
                        thumbnails::next_window(thumbs.selected, n)
                    });
                    thumbs.focused = true;
                }
                open.state.disable_hover(now);
                open.state.show_immediately();
            }
            // `_select(this._selectedIndex, 0)` — descend, focused, on the first window.
            SwitcherKey::Down if !focused && open.selected_windows().len() > 1 => {
                open.select_thumb(0, true, fade_in);
                open.state.disable_hover(now);
                open.state.show_immediately();
            }
            _ => {
                let before = open.state.selected();
                open.state.key_press(key, now);
                // Moving to another app tears its predecessor's sub-list down and starts the
                // timer over (`_select`'s first branch, `:328-331`).
                if open.state.selected() != before {
                    going = open.close_thumbs(now, true);
                }
            }
        }

        self.retire_thumbs(going);
        self.note_shown(was);
        self.take_outcome()
    }

    pub fn key_release(
        &mut self,
        held: Modifiers,
        now: Duration,
    ) -> Option<(SwitcherOutcome, Activation)> {
        let open = self.open.as_mut()?;
        open.state.key_release(held, now);
        self.take_outcome()
    }

    /// Pointer motion over the popup. Returns whether the selection moved.
    pub fn pointer_motion(&mut self, pos: Point<f64, Logical>, now: Duration) -> bool {
        let Some(open) = self.open.as_mut() else {
            return false;
        };

        // `_windowEntered` (`altTab.js:262-268`) — hovering a preview picks that window, gated on
        // `mouseActive` exactly like the row above it.
        if let Some(thumbs) = open.thumbs.as_mut() {
            if let Some(n) = thumbs.layout.items.iter().position(|r| r.contains(pos)) {
                if !open.state.hover_selects() || thumbs.selected == Some(n) {
                    return false;
                }
                thumbs.selected = Some(n);
                thumbs.focused = true;
                return true;
            }
        }

        let Some(item) = open.layout.item_at(pos) else {
            return false;
        };
        let moved = open.state.pointer_entered_item(item);
        if moved {
            let going = open.close_thumbs(now, true);
            self.retire_thumbs(going);
        }
        moved
    }

    /// A click — `_itemActivated` (`switcherPopup.js:250-257`) inside an item, and the
    /// click-outside dismissal (`:71-85`) anywhere else.
    ///
    /// Clicking outside **cancels** rather than committing: you did not pick anything, so focus
    /// stays where it was.
    pub fn pointer_click(
        &mut self,
        pos: Point<f64, Logical>,
    ) -> Option<(SwitcherOutcome, Activation)> {
        let open = self.open.as_mut()?;

        // `_windowActivated` (`:255-259`): a click in the sub-list activates *that* window and
        // ends the session, without going back through the app row.
        if let Some(thumbs) = open.thumbs.as_mut() {
            if let Some(n) = thumbs.layout.items.iter().position(|r| r.contains(pos)) {
                thumbs.selected = Some(n);
                thumbs.focused = true;
                open.state.key_press(SwitcherKey::Commit, Duration::ZERO);
                return self.take_outcome();
            }
        }

        match open.layout.item_at(pos) {
            Some(item) => {
                open.state.select(item);
                open.state.key_press(SwitcherKey::Commit, Duration::ZERO);
            }
            // `_isActorOutside` (`:395-398`) counts the sub-list as inside the popup, so a click
            // in its padding dismisses nothing.
            None if open
                .thumbs
                .as_ref()
                .is_some_and(|t| t.layout.panel.contains(pos)) => {}
            None => open.state.key_press(SwitcherKey::Dismiss, Duration::ZERO),
        }
        self.take_outcome()
    }

    /// A window went away while the popup is up (`_itemRemoved`, `switcherPopup.js:269-284`).
    pub fn window_removed(&mut self, id: MappedId) -> Option<(SwitcherOutcome, Activation)> {
        let open = self.open.as_mut()?;

        let index = match &mut open.items {
            Items::Windows(ids) | Items::Cycler { windows: ids, .. } => {
                let index = ids.iter().position(|&w| w == id)?;
                ids.remove(index);
                index
            }
            Items::Apps(apps) => {
                // A window closing only removes the *item* when it was the app's last one.
                let index = apps.iter().position(|a| a.windows.contains(&id))?;
                let at = apps[index].windows.iter().position(|&w| w == id);
                apps[index].windows.retain(|&w| w != id);
                if let Some(at) = at {
                    open.art[index].window_titles.remove(at);
                }
                if !apps[index].windows.is_empty() {
                    // The app is still running; only its arrow may need to go.
                    open.art[index].arrow = apps[index].windows.len() > 1;
                    open.thumbnail_removed(id);
                    return None;
                }
                apps.remove(index);
                index
            }
        };

        // A cycler has no art at all — `CyclerList` draws nothing, which is why `open_with` waives
        // the items/art length check for one. Removing at `index` there is an out-of-bounds panic
        // on an empty vec, and this runs from a wl_resource destructor, where a panic is an abort
        // rather than a failed test.
        if !matches!(open.items, Items::Cycler { .. }) {
            open.art.remove(index);
        }
        open.state.item_removed(index);
        self.relayout();
        self.take_outcome()
    }

    /// Close without committing — used when something else takes over the screen (a modal, a
    /// session lock), matching `system-modal-opened` (`switcherPopup.js:56-57`).
    pub fn cancel(&mut self) {
        self.open = None;
    }

    fn take_outcome(&mut self) -> Option<(SwitcherOutcome, Activation)> {
        let open = self.open.as_ref()?;
        let outcome = open.state.outcome()?;
        let target = match outcome {
            SwitcherOutcome::Commit => open.activation(),
            SwitcherOutcome::Cancel => Activation::new(),
        };
        self.open = None;
        Some((outcome, target))
    }

    /// Re-measure after the item count changed, so the row re-centers instead of leaving a hole
    /// where the removed item was.
    ///
    /// The icon size is deliberately *not* recomputed: GNOME sets `_iconSize` once, on the first
    /// height request (`altTab.js:783-788`), so a row that loses an item does not grow its icons
    /// back. Re-laddering here would make windows closing under the popup resize every item.
    fn relayout(&mut self) {
        let Some(open) = self.open.as_mut() else {
            return;
        };
        if matches!(open.items, Items::Cycler { .. }) {
            return;
        }
        let content = open.content_size();
        open.layout = PanelLayout::new(
            &vec![content; open.items.len()],
            open.monitor,
            open.footer_height(),
        );
    }

    /// Bake the window sub-list's own `.switcher-list` — its plate, the wash under the picked
    /// preview and every caption. The previews themselves are live windows and ride above it.
    ///
    /// `None` when there is no sub-list up, or when the bake failed (already logged).
    fn render_thumbs(
        &self,
        thumbs: &Thumbs,
        ctx: &mut RenderCtx,
        scale: f64,
    ) -> Option<TextureRenderElement<VkTexture>> {
        let panel = thumbs.layout.panel;

        // Panel-relative, like the row above.
        let rel = |r: Rectangle<f64, Logical>| Rectangle::new(r.loc - panel.loc, r.size);
        let items: Vec<_> = thumbs.layout.items.iter().copied().map(rel).collect();
        let labels: Vec<_> = thumbs.layout.labels.iter().copied().map(rel).collect();
        let selected = thumbs.selected;

        // A caption is a window title — arbitrary client text, so it is cut to its preview rather
        // than allowed to widen the item (`.thumbnail`'s `width: 256px` is fixed).
        let captions: Vec<String> = thumbs
            .titles
            .iter()
            .map(|t| widget::ellipsized_line(t, LABEL_PT, thumbnails::THUMBNAIL_SIZE))
            .collect();

        let revision = widget::Revision::new()
            .of(selected)
            .px(panel.size.w)
            .px(panel.size.h)
            .each(captions.iter())
            .done();

        let baked = widget::bake(
            &mut *ctx.renderer,
            &mut self.thumb_chrome.borrow_mut(),
            scale,
            panel.size,
            revision,
            |renderer| {
                let mut shaper = TextShaper::new(renderer, scale);
                captions
                    .iter()
                    .map(|t| shaper.shape(t, TextStyle::new(LABEL_PT)))
                    .collect::<anyhow::Result<Vec<ShapedText>>>()
            },
            |frame, phys, shaped: &Vec<ShapedText>| {
                let mut p = Painter::new(frame, scale, phys);
                p.clear(style::TRANSPARENT)?;
                p.fill_rounded_full(LIST_RADIUS, LIST_BG)?;
                p.stroke_rounded_full(LIST_RADIUS, LIST_BORDER, LIST_BORDER_COLOR)?;

                // Nothing is washed while `_currentWindow` is unset: the list can be up purely to
                // show you what is there.
                if let Some(rect) = selected.and_then(|i| items.get(i)) {
                    p.fill_rounded(*rect, ITEM_RADIUS, ITEM_SELECTED)?;
                }

                for (label, band) in shaped.iter().zip(&labels) {
                    p.text_band(
                        label,
                        band.loc.x + band.size.w / 2.,
                        widget::HAlign::Center,
                        band.loc.y,
                        band.size.h,
                        LIST_FG,
                        *band,
                    )?;
                }
                Ok(())
            },
        );

        match baked {
            Ok(texture) => {
                let buffer = TextureBuffer::from_texture(
                    ctx.renderer,
                    texture,
                    scale,
                    Transform::Normal,
                    Vec::new(),
                );
                Some(TextureRenderElement::from_texture_buffer(
                    buffer,
                    panel.loc,
                    // The fade rides here rather than in the bake: an alpha in the revision would
                    // re-bake the plate and every caption on each frame of it.
                    thumbs.fade.value() as f32,
                    None,
                    None,
                    Kind::Unspecified,
                ))
            }
            Err(err) => {
                tracing::error!("error drawing the switcher's window sub-list: {err:#}");
                None
            }
        }
    }

    /// Front-to-back, like every other UI `render`.
    pub fn render_output(
        &self,
        synoik: &crate::synoik::Synoik,
        output: &Output,
        mut ctx: RenderCtx,
        push: &mut dyn FnMut(SwitcherRenderElement),
    ) {
        let Some(open) = self.open.as_ref() else {
            return;
        };
        if open.output != *output || open.state.visibility() == Visibility::Pending {
            return;
        }
        // A cycler has no panel to draw; its highlight goes out from `render_cycler` instead,
        // down in the window layer — and without waiting out `POPUP_DELAY`, since GNOME
        // highlights from `_initialSelection` inside `show()` and the delay only ever touched
        // the popup actor's opacity (`switcherPopup.js:137-160`).
        if matches!(open.items, Items::Cycler { .. }) {
            return;
        }

        let _span = tracy_client::span!("SwitcherUi::render_output");
        let scale = output.current_scale().fractional_scale();
        let app_icons = &synoik.app_icon_cache;
        let mut elements: Vec<TextureRenderElement<VkTexture>> = Vec::new();
        // Collected rather than pushed as they are drawn: the app badge sits *over* the preview
        // (`WindowIcon` adds the clone and then the icon to one `Clutter.BinLayout`,
        // `altTab.js:1029-1037`, so the icon is the later child and paints on top), and `push` is
        // front-to-back — so every icon must go out before any thumbnail.
        let mut thumbnails: Vec<WindowThumbnailRenderElement> = Vec::new();

        // A window switcher's items are live windows; an app switcher's are just icons.
        if let Items::Windows(ids) = &open.items {
            for (i, id) in ids.iter().enumerate() {
                let Some(item) = open.layout.items.get(i) else {
                    continue;
                };
                let Some((_, mapped)) = synoik.layout.windows().find(|(_, m)| m.id() == *id) else {
                    continue;
                };
                window_thumbnail::render(
                    mapped,
                    ctx.r(),
                    window_switcher::preview_box(*item),
                    scale,
                    1.,
                    // `WindowIcon`'s clone has no rounding of its own — only the sub-list's
                    // `.thumbnail` does.
                    0.,
                    &mut |elem| thumbnails.push(elem),
                );
            }
        }

        // The window sub-list under a multi-window app: its previews are live windows too, and
        // its panel is baked separately below.
        for thumbs in open.thumbs.iter().chain(self.fading.as_ref()) {
            let alpha = thumbs.fade.value() as f32;
            for (i, id) in thumbs.windows.iter().enumerate() {
                let Some(bin) = thumbs.layout.thumbs.get(i) else {
                    continue;
                };
                let Some((_, mapped)) = synoik.layout.windows().find(|(_, m)| m.id() == *id) else {
                    continue;
                };
                window_thumbnail::render(
                    mapped,
                    ctx.r(),
                    *bin,
                    scale,
                    alpha,
                    thumbnails::THUMB_RADIUS,
                    &mut |elem| thumbnails.push(elem),
                );
            }
        }

        // The icons ride above the panel, one texture each. For an app switcher that is the app
        // icon filling the square; for a window switcher it is the small badge in the preview's
        // bottom-right corner.
        let mut uploads = self.icons.borrow_mut();
        for (i, art) in open.art.iter().enumerate() {
            let Some(icon) = art.icon.as_ref() else {
                continue;
            };
            let Some(item) = open.layout.items.get(i) else {
                continue;
            };
            let (icon_px, center) = match &open.items {
                Items::Apps(_) => (
                    open.icon_px,
                    Point::from((item.size.w / 2., ITEM_PADDING + open.icon_px / 2.)),
                ),
                // A cycler has no art, so this loop never runs for one.
                Items::Cycler { .. } => continue,
                Items::Windows(_) => {
                    let preview = window_switcher::preview_box(*item);
                    (
                        window_switcher::APP_ICON_SIZE_SMALL,
                        window_switcher::app_icon_center(preview) - item.loc,
                    )
                }
            };
            if let Some(el) = widget::app_icon_element(
                &mut *ctx.renderer,
                &mut uploads,
                app_icons,
                icon,
                icon_px,
                scale,
                item.loc,
                center,
                1.,
            ) {
                elements.push(el);
            }
        }
        drop(uploads);

        // ...and the panel, its selection wash and every label are one bake behind them.
        let thumbs_focused = open.thumbs.as_ref().is_some_and(|t| t.focused);
        let revision = widget::Revision::new()
            .of(open.state.selected())
            .of(thumbs_focused)
            .px(open.icon_px)
            .each(open.art.iter().map(|a| (a.label.clone(), a.arrow)))
            .done();

        let panel = open.layout.panel;
        let label_height = open.label_height;
        let items: Vec<Rectangle<f64, Logical>> = open
            .layout
            .items
            .iter()
            .map(|r| Rectangle::new(r.loc - panel.loc, r.size))
            .collect();
        // Panel-relative, like `items`.
        let footer = open
            .layout
            .footer
            .map(|r| Rectangle::new(r.loc - panel.loc, r.size));

        // What actually gets shaped. A window switcher draws **one** label — the selected
        // window's title, across the panel's bottom (`WindowSwitcher.highlight` sets the single
        // `_label`, `altTab.js:1130-1134`) — ellipsized to the panel like every `StLabel`
        // (`PANGO_ELLIPSIZE_END`, `st-label.c:331`). An app switcher labels every item, because
        // there the label is a child of the item (`AppIcon`, `:682-686`).
        let texts: Vec<String> = match footer {
            Some(footer) => open
                .art
                .get(open.state.selected())
                .map(|a| widget::ellipsized_line(&a.label, LABEL_PT, footer.size.w))
                .filter(|line| !line.is_empty())
                .into_iter()
                .collect(),
            None => open.art.iter().map(|a| a.label.clone()).collect(),
        };

        let baked = widget::bake(
            &mut *ctx.renderer,
            &mut self.chrome.borrow_mut(),
            scale,
            panel.size,
            revision,
            |renderer| {
                let mut shaper = TextShaper::new(renderer, scale);
                texts
                    .iter()
                    .map(|t| shaper.shape(t, TextStyle::new(LABEL_PT)))
                    .collect::<anyhow::Result<Vec<ShapedText>>>()
            },
            |frame, phys, labels: &Vec<ShapedText>| {
                let mut p = Painter::new(frame, scale, phys);
                p.clear(style::TRANSPARENT)?;
                p.fill_rounded_full(LIST_RADIUS, LIST_BG)?;
                p.stroke_rounded_full(LIST_RADIUS, LIST_BORDER, LIST_BORDER_COLOR)?;

                // The selection wash, and nothing for hover — hovering moves the selection
                // instead, so a second highlight would show two current items at once.
                //
                // `highlight(index, justOutline)` (`switcherPopup.js:493-504`): with the sub-list
                // focused the app wears the dimmer `:highlighted` instead, so the two panels never
                // both look current.
                if let Some(rect) = items.get(open.state.selected()) {
                    let wash = if thumbs_focused {
                        ITEM_HIGHLIGHTED
                    } else {
                        ITEM_SELECTED
                    };
                    p.fill_rounded(*rect, ITEM_RADIUS, wash)?;
                }

                match footer {
                    // The selected window's title, centered across the panel's bottom.
                    Some(footer) => {
                        if let Some(label) = labels.first() {
                            p.text_band(
                                label,
                                footer.loc.x + footer.size.w / 2.,
                                widget::HAlign::Center,
                                footer.loc.y,
                                footer.size.h,
                                LIST_FG,
                                footer,
                            )?;
                        }
                    }
                    // The app name under each icon, centered in the strip the icon leaves free.
                    None => {
                        for (i, rect) in items.iter().enumerate() {
                            if let Some(label) = labels.get(i) {
                                p.text(
                                    label,
                                    Point::from((
                                        rect.loc.x + rect.size.w / 2.,
                                        rect.loc.y + rect.size.h - ITEM_PADDING - label_height / 2.,
                                    )),
                                    widget::Align::CENTER,
                                    LIST_FG,
                                )?;
                            }
                        }
                    }
                }

                // The multi-window chevron, under each app that has more than one window. It
                // brightens with its item rather than appearing: an app with several windows shows
                // a dim arrow at rest and a full-strength one when selected (`highlight`,
                // `altTab.js:857-873`). A single-window app has none at all — its `art.arrow` is
                // false — which is what makes the arrow mean "there is a sub-list here".
                for (i, rect) in items.iter().enumerate() {
                    if open.art.get(i).is_some_and(|a| a.arrow) {
                        let color = if i == open.state.selected() {
                            ARROW_HIGHLIGHTED
                        } else {
                            ARROW
                        };
                        p.triangle(app_switcher::arrow_rect(*rect), widget::Side::Bottom, color)?;
                    }
                }
                Ok(())
            },
        );

        let mut panel_element = None;
        match baked {
            Ok(texture) => {
                let buffer = TextureBuffer::from_texture(
                    ctx.renderer,
                    texture,
                    scale,
                    Transform::Normal,
                    Vec::new(),
                );
                panel_element = Some(TextureRenderElement::from_texture_buffer(
                    buffer,
                    panel.loc,
                    1.,
                    None,
                    None,
                    Kind::Unspecified,
                ));
            }
            Err(err) => tracing::error!("error drawing the switcher panel: {err:#}"),
        }

        let thumb_panels: Vec<_> = open
            .thumbs
            .iter()
            .chain(self.fading.as_ref())
            .filter_map(|t| self.render_thumbs(t, &mut ctx, scale))
            .collect();

        // Front-to-back, so this is the stacking order read topmost-first: the app icons overlay
        // the window previews, and both panels (with their labels and arrows) are behind them.
        for element in elements {
            push(SwitcherRenderElement::Texture(element));
        }
        for element in thumbnails {
            push(SwitcherRenderElement::Thumbnail(element));
        }
        for element in panel_element.into_iter().chain(thumb_panels) {
            push(SwitcherRenderElement::Texture(element));
        }
    }
}
