//! Panel popovers: click-anchored popups under a top-panel button.
//!
//! GNOME's panel buttons (dateMenu, quickSettings, …) open a popup menu anchored
//! below the button that grabs input and dismisses on Escape or an outside click.
//! This is the shared mechanism for those; the contents are the [`Calendar`] and
//! the [`QuickSettings`] menu. Unlike the modal dialogs (run dialog, end-session),
//! a popover draws **no** full-screen dim — it's a floating anchored surface, like
//! a GNOME popup menu — but it *does* grab input while open.
//!
//! Reuses the overlay render pattern (offscreen `VkTexture` → `TextureBuffer` →
//! positioned `TextureRenderElement`, like `run_dialog.rs`). A content type may
//! contribute *several* elements (the quick-settings menu composites its icons on
//! top of its chrome), so [`render`](PanelPopover::render) returns a `Vec`. The
//! net-new behavior vs the existing overlays is outside-click dismissal.

use std::cell::RefCell;
use std::rc::Rc;

use niri_config::Config;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::Texture as _;
use smithay::input::keyboard::Keysym;
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Size, Transform};

use crate::animation::{Animation, Clock};
use crate::render_helpers::icon::IconCache;

/// How far the popover slides in (logical px) as it fades open — gnome-shell's
/// `BoxPointer` `-arrow-rise` (`$base_padding` = 6px). It emerges from `rise` above
/// its resting spot (toward the panel) and settles down, reversing on close.
const POPOVER_RISE: f64 = 6.;

/// Resting gap between the popover and the panel / screen edge, logical px. gnome-shell's
/// `.popup-menu-boxpointer { -arrow-rise: $base_padding }` is documented as the "distance
/// from the panel & screen edge" (6px), so the menu doesn't sit flush against either.
const POPOVER_MARGIN: f64 = 6.;

/// `.popup-menu-content` `box-shadow: 0 2px 4px 0 $shadow_color` (`_popovers.scss:32`) — the drop
/// shadow every panel popover (QS / date / input-source BoxPointer) casts; `$shadow_color` (dark)
/// = `rgba(0,0,0,0.2)`.
///
/// The literal CSS spread is 0, but St's shadow rasterizer (`st-private.c` +
/// `st-theme-node-drawing.c`) renders visibly denser than a naive "blur the silhouette, edge =
/// 0.5-coverage" gaussian: measured against a real GNOME 50.1 popover over white, the shadow's
/// core sits at ~full `$shadow_color` alpha right at the box edge and falls off outside — a
/// profile a **spread of 2** reproduces almost exactly (the tail matches pixel-for-pixel). So we
/// carry spread 2 to match GNOME's on-screen result (the true reference), not the literal 0.
const POPOVER_SHADOW: widget::DropShadowSpec = widget::DropShadowSpec {
    blur: 4.,
    offset: (0., 2.),
    spread: 2.,
    color: [0., 0., 0., 0.2],
};

/// `.popup-menu-content` `border: 1px solid $outer_borders_color` (`_popovers.scss:31`);
/// `$outer_borders_color` (dark) = `lighten($bg_color #36363a, 5%)` = `#424247`.
const POPOVER_BORDER: widget::Rgba = [0.260, 0.260, 0.279, 1.];

use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::ui::calendar::DateMenu;
use crate::ui::input_source_menu::{InputSourceItem, InputSourceMenu};
use crate::ui::notification_card::CardGroup;
use crate::ui::panel::PANEL_HEIGHT;
use crate::ui::quick_settings::QuickSettings;
use crate::ui::widget;
use crate::utils::output_size;

/// The side effect a popover click asks the caller (the input handler) to apply.
/// Keeps the content widgets pure — they never touch gsettings or spawn — while
/// still driving real behavior.
#[derive(Debug, Clone, PartialEq)]
pub enum PopoverAction {
    /// The click was consumed but has no side effect (e.g. a calendar day, or a
    /// hit on empty menu space). The popover stays open.
    Consumed,
    /// Set `org.gnome.desktop.interface color-scheme` (Dark Style tile).
    SetDarkStyle(bool),
    /// Set the inverse of `org.gnome.desktop.notifications show-banners` (DND).
    SetDoNotDisturb(bool),
    /// Set `org.gnome.settings-daemon.plugins.color night-light-enabled`.
    SetNightLight(bool),
    /// Set the default sink's perceptual volume `0..=1` (the QS volume slider).
    SetVolume(f64),
    /// Toggle the default sink's mute (clicking the slider's speaker icon).
    ToggleMute,
    /// Set the system default output sink to this `node.name` (an output-device-picker row). The
    /// menu stays open (gnome-shell keeps the device list up after picking); the check moves when
    /// the write echoes back.
    SetDefaultSink(String),
    /// Set the default source's perceptual volume `0..=1` (the QS mic slider).
    SetInputVolume(f64),
    /// Toggle the default source's mute (clicking the mic slider's icon).
    ToggleInputMute,
    /// Set the system default input source to this `node.name` (an input-device-picker row). The
    /// menu stays open, like [`SetDefaultSink`](Self::SetDefaultSink).
    SetDefaultSource(String),
    /// Set gsd-rfkill's airplane mode (the QS "Airplane Mode" toggle). The menu stays open; the
    /// tile updates on the gsd echo (not optimistic — a rejected/hw-blocked write has no echo).
    SetAirplaneMode(bool),
    /// Toggle the power profile (the Power Mode tile body): Balanced ↔ last-selected. Carries no
    /// target because *which* profile depends on the compositor-owned last-selected state; the
    /// input layer resolves it (`apply_popover_action`). Menu stays open; echo-driven.
    TogglePowerProfile,
    /// Set power-profiles-daemon's `ActiveProfile` to this profile id (a Power Mode picker row).
    /// The menu stays open; the check moves when the write echoes back (like
    /// [`SetDefaultSink`]).
    SetPowerProfile(String),
    /// Open the interactive screenshot UI (the screenshot system button); the
    /// popover closes.
    Screenshot,
    /// Spawn a command (a system-row button / the battery pill); popover closes.
    Spawn(Vec<String>),
    /// Close this notification, reason Dismissed (a message-list card's close
    /// button). The popover stays open.
    CloseNotification(u32),
    /// Close every notification in a group, reason Dismissed (the close button
    /// of a COLLAPSED group's top card closes the whole group,
    /// `js/ui/messageList.js:1106-1112,1236-1242`). The popover stays open.
    CloseNotificationGroup(Vec<u32>),
    /// Activate this notification (a message-list card body click): with a
    /// default action, emit ActivationToken+ActionInvoked and destroy unless
    /// resident; without one, `source.open()`'s destroy-all-non-resident
    /// (`js/ui/messageList.js:730-732`, `js/ui/notificationDaemon.js:231-240`).
    ActivateNotification { id: u32, has_default: bool },
    /// An expanded message-list card's action button: emit
    /// ActivationToken+ActionInvoked for `key` and destroy unless resident
    /// (`js/ui/notificationDaemon.js:224-227`, `js/ui/messageTray.js:430-442`).
    InvokeNotificationAction { id: u32, key: String },
    /// The message list's Clear pill: close every notification.
    ClearNotifications,
    /// Switch to this input source (a layout row in the keyboard menu): set the
    /// active xkb group and record it in `mru-sources`. The menu closes, like
    /// gnome-shell's popup menu closing on item activation.
    SetInputSource(usize),
}

impl PopoverAction {
    /// Whether applying this action dismisses the menu (GNOME closes quick
    /// settings when a system button is used, but keeps it open for a toggle).
    fn closes_menu(&self) -> bool {
        // Activating a notification also closes the calendar: gnome-shell's
        // no-default-action path runs `source.open()` → `Main.panel
        // .closeCalendar()` (`js/ui/notificationDaemon.js:370-382`), and with
        // a default action the activated app takes focus, dropping the menu
        // grab — which we have no focus-driven dismissal for, so close
        // explicitly in both cases (else the popover's modal key grab lingers
        // over the newly raised window). Invoking an action button gets the
        // same treatment: it carries an activation token, so the common case
        // is the app raising a window under our grab.
        matches!(
            self,
            PopoverAction::Screenshot
                | PopoverAction::Spawn(_)
                | PopoverAction::ActivateNotification { .. }
                | PopoverAction::InvokeNotificationAction { .. }
                // Picking a layout closes the popup, like gnome-shell's popup menu.
                | PopoverAction::SetInputSource(_)
        )
    }
}

/// The content a popover hosts.
pub enum PopoverContent {
    // Boxed: `DateMenu` and `QuickSettings` carry several caches each, so they
    // dominate the enum size (`clippy::large_enum_variant`).
    Calendar(Box<DateMenu>),
    QuickSettings(Box<QuickSettings>),
    InputSources(InputSourceMenu),
}

impl PopoverContent {
    fn logical_size(&self) -> Size<f64, Logical> {
        match self {
            PopoverContent::Calendar(dm) => dm.logical_size(),
            PopoverContent::QuickSettings(qs) => qs.logical_size(),
            PopoverContent::InputSources(m) => m.logical_size(),
        }
    }

    /// The content box's corner radius, for the `.popup-menu-content` drop shadow behind it.
    fn corner_radius(&self) -> f64 {
        match self {
            PopoverContent::Calendar(dm) => dm.corner_radius(),
            PopoverContent::QuickSettings(qs) => qs.corner_radius(),
            PopoverContent::InputSources(m) => m.corner_radius(),
        }
    }
}

/// A single panel popover, owned on `Niri` alongside the other overlays.
pub struct PanelPopover {
    open: bool,
    /// The output the popover is anchored on (drawn/hit-tested only there).
    output: Option<Output>,
    /// The panel button rect it hangs from, output-local logical.
    anchor: Rectangle<f64, Logical>,
    content: Option<PopoverContent>,
    /// The shared animation clock, cloned into each open/close [`Animation`].
    clock: Clock,
    /// The live config, read for the popover open/close animation params on each toggle.
    config: Rc<RefCell<Config>>,
    /// The open/close fade progress (0 = hidden, 1 = fully shown). `None` = no animation
    /// has run yet (treated as fully shown). On close it runs current→0.
    anim: Option<Animation>,
    /// While closing, the content is kept and rendered (fading out) until the animation
    /// settles, then dropped by [`advance_animations`](Self::advance_animations).
    closing: bool,
    /// The `.popup-menu-content` drop shadow, baked into its own texture and cached by
    /// `(scale, size)` (keyed on the content radius so a same-size different-radius content
    /// re-bakes). Composited behind whatever content is up.
    shadow_cache: RefCell<widget::BakeCache>,
    /// The `.popup-menu-content` 1px border, baked as a transparent ring texture and composited
    /// on top (a multi-texture popover would otherwise seam if bordered per-texture). Same keying.
    border_cache: RefCell<widget::BakeCache>,
    /// The `.popup-menu-content` background fill (`$bg_color` #36363a), baked once and composited
    /// BEHIND the content and above the shadow — the shared chrome's single bg, so the three
    /// contents (bake with a transparent bg) can't drift the popover box color. Same keying.
    fill_cache: RefCell<widget::BakeCache>,
}

impl PanelPopover {
    pub fn new(clock: Clock, config: Rc<RefCell<Config>>) -> Self {
        Self {
            open: false,
            output: None,
            anchor: Rectangle::default(),
            content: None,
            clock,
            config,
            anim: None,
            closing: false,
            shadow_cache: RefCell::new(widget::BakeCache::new()),
            border_cache: RefCell::new(widget::BakeCache::new()),
            fill_cache: RefCell::new(widget::BakeCache::new()),
        }
    }

    /// Whether the popover is showing (including while it fades out on close).
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// The panel button role whose menu is up, so the panel can keep that button's
    /// container in its active state. `None` once the popover starts closing, so the
    /// button de-highlights immediately as the menu fades out (like gnome-shell
    /// dropping `:checked` on dismiss).
    pub fn open_role(&self) -> Option<&'static str> {
        if !self.open || self.closing {
            return None;
        }
        match self.content.as_ref()? {
            PopoverContent::Calendar(_) => Some(crate::ui::panel::ROLE_DATE_MENU),
            PopoverContent::QuickSettings(_) => Some(crate::ui::panel::ROLE_QUICK_SETTINGS),
            PopoverContent::InputSources(_) => Some(crate::ui::panel::ROLE_KEYBOARD),
        }
    }

    /// Build an open/close fade animation from `from` to `to` using the configured
    /// `panel_popover_open_close` params (gnome-shell's `BoxPointer` timing).
    fn make_anim(&self, from: f64, to: f64) -> Animation {
        let c = self.config.borrow();
        Animation::new(
            self.clock.clone(),
            from,
            to,
            0.,
            c.animations.panel_popover_open_close.0,
        )
    }

    /// The current fade progress in `[0, 1]` (1 when fully open with no animation).
    fn progress(&self) -> f32 {
        self.anim
            .as_ref()
            .map_or(1., |a| a.clamped_value().clamp(0., 1.) as f32)
    }

    /// Settle the open/close animation: once a close fade finishes, drop the content.
    pub fn advance_animations(&mut self) {
        if self.closing && self.anim.as_ref().is_none_or(|a| a.is_done()) {
            self.open = false;
            self.closing = false;
            self.output = None;
            self.content = None;
            self.anim = None;
        }
    }

    /// Whether an open/close fade is still running (keeps the redraw loop ticking).
    pub fn are_animations_ongoing(&self) -> bool {
        self.anim.as_ref().is_some_and(|a| !a.is_done())
    }

    /// The output the popover is anchored on, while open.
    pub fn output(&self) -> Option<&Output> {
        self.output.as_ref()
    }

    /// Toggle the dateMenu popover (message list + calendar): open it anchored
    /// at `anchor` on `output`, or close it if it's already open (from the same
    /// button). `cards` is the notification-store snapshot for the message
    /// list. Returns whether it opened — the caller acknowledges the store
    /// exactly then (`js/ui/messageList.js:1193-1199`), never on close.
    pub fn toggle_calendar(
        &mut self,
        output: Output,
        anchor: Rectangle<f64, Logical>,
        week_start: u8,
        show_week_numbers: bool,
        accent: [u8; 3],
        groups: Vec<CardGroup>,
    ) -> bool {
        if self.is_showing::<CalendarTag>() {
            self.close();
            return false;
        }
        self.open = true;
        self.closing = false;
        self.anchor = anchor;
        let mut date_menu = DateMenu::new(week_start, show_week_numbers, accent, groups);
        // Grow to fit the content but stay within the work area, leaving the
        // same margin at the bottom as the top (`js/ui/panelMenu.js:177-185`,
        // `js/ui/boxpointer.js:117-137`): output height minus the panel and both
        // margins. Past this the message list scrolls.
        let available_h =
            (output_size(&output).h - PANEL_HEIGHT - 2. * POPOVER_MARGIN).max(POPOVER_MARGIN);
        date_menu.set_available_height(available_h);
        self.output = Some(output);
        self.content = Some(PopoverContent::Calendar(Box::new(date_menu)));
        self.anim = Some(self.make_anim(0., 1.));
        true
    }

    /// Push a fresh notification snapshot to an open calendar popover, so the
    /// message list tracks store changes live — WITHOUT re-acknowledging
    /// (notifications arriving while open stay unseen,
    /// `js/ui/messageList.js:1193-1199`). Returns whether it changed anything.
    pub fn set_notifications(&mut self, groups: Vec<CardGroup>) -> bool {
        match &mut self.content {
            Some(PopoverContent::Calendar(dm)) if self.open && !self.closing => {
                dm.set_notifications(groups)
            }
            _ => false,
        }
    }

    /// Push a freshly-formatted Events section model into the open dateMenu.
    /// Returns whether it changed anything.
    pub fn set_calendar_events(&mut self, model: crate::ui::calendar::EventsSectionModel) -> bool {
        match &mut self.content {
            Some(PopoverContent::Calendar(dm)) if self.open && !self.closing => {
                dm.set_events(model)
            }
            _ => false,
        }
    }

    /// Introspection/test hook: the open dateMenu content.
    pub fn date_menu(&self) -> Option<&DateMenu> {
        match &self.content {
            Some(PopoverContent::Calendar(dm)) if self.open => Some(dm),
            // (dm is &Box<DateMenu>; auto-derefs to &DateMenu at the return.)
            _ => None,
        }
    }

    /// Toggle the input-source (keyboard-layout) menu, anchored at `anchor` on
    /// `output`. `items` are the configured layouts (in source order) and
    /// `active` the current one.
    pub fn toggle_input_sources(
        &mut self,
        output: Output,
        anchor: Rectangle<f64, Logical>,
        items: Vec<InputSourceItem>,
        active: usize,
    ) {
        if self.is_showing::<InputSourcesTag>() {
            self.close();
            return;
        }
        self.open = true;
        self.closing = false;
        self.output = Some(output);
        self.anchor = anchor;
        self.content = Some(PopoverContent::InputSources(InputSourceMenu::new(
            items, active,
        )));
        self.anim = Some(self.make_anim(0., 1.));
    }

    /// The popover's content origin on `output` (its resting top-left),
    /// output-local logical — for tests that click inside the content.
    pub fn content_location(&self, output: &Output) -> Point<f64, Logical> {
        self.location(output)
    }

    /// Toggle the quick-settings menu, anchored at `anchor` on `output`. `battery`
    /// feeds the power pill (`None` hides it); `audio` feeds the volume slider.
    #[allow(clippy::too_many_arguments)]
    pub fn toggle_quick_settings(
        &mut self,
        output: Output,
        anchor: Rectangle<f64, Logical>,
        toggles: crate::gnome::QuickToggles,
        network: crate::system_status::NetworkStatus,
        airplane: crate::system_status::AirplaneStatus,
        power: crate::system_status::PowerProfileStatus,
        battery: Option<crate::system_status::BatteryStatus>,
        audio: Option<crate::audio::AudioStatus>,
        sink_list: crate::audio::SinkList,
        mic: crate::audio::MicStatus,
        source_list: crate::audio::SourceList,
        accent: [u8; 3],
    ) {
        if self.is_showing::<QuickSettingsTag>() {
            self.close();
            return;
        }
        self.open = true;
        self.closing = false;
        self.output = Some(output);
        self.anchor = anchor;
        self.content = Some(PopoverContent::QuickSettings(Box::new(QuickSettings::new(
            toggles,
            network,
            airplane,
            power,
            battery,
            audio,
            sink_list,
            mic,
            source_list,
            accent,
        ))));
        self.anim = Some(self.make_anim(0., 1.));
    }

    /// Push a fresh audio snapshot to an open quick-settings popover (from the
    /// PipeWire watcher), so the volume slider tracks live changes. Returns whether
    /// it changed anything.
    pub fn set_audio(&mut self, audio: Option<crate::audio::AudioStatus>) -> bool {
        match &mut self.content {
            Some(PopoverContent::QuickSettings(qs)) if self.open && !self.closing => {
                qs.set_audio(audio)
            }
            _ => false,
        }
    }

    /// Push a fresh output-sink list to an open quick-settings popover, so the device picker tracks
    /// sinks appearing/disappearing and default changes. Returns whether it changed anything.
    pub fn set_sink_list(&mut self, sink_list: crate::audio::SinkList) -> bool {
        match &mut self.content {
            Some(PopoverContent::QuickSettings(qs)) if self.open && !self.closing => {
                qs.set_sink_list(sink_list)
            }
            _ => false,
        }
    }

    /// Push a fresh mic snapshot to an open quick-settings popover, so the mic slider tracks live
    /// level/mute changes and appears/disappears with recording. Returns whether it changed.
    pub fn set_mic(&mut self, mic: crate::audio::MicStatus) -> bool {
        match &mut self.content {
            Some(PopoverContent::QuickSettings(qs)) if self.open && !self.closing => {
                qs.set_mic(mic)
            }
            _ => false,
        }
    }

    /// Push a fresh input-source list to an open quick-settings popover, so the input-device picker
    /// tracks sources appearing/disappearing and default changes. Returns whether it changed.
    pub fn set_source_list(&mut self, source_list: crate::audio::SourceList) -> bool {
        match &mut self.content {
            Some(PopoverContent::QuickSettings(qs)) if self.open && !self.closing => {
                qs.set_source_list(source_list)
            }
            _ => false,
        }
    }

    /// Push a fresh airplane-mode snapshot to an open quick-settings popover, so the "Airplane
    /// Mode" toggle tile appears/vanishes with the hardware and reflects the live state. Returns
    /// whether it changed.
    pub fn set_airplane(&mut self, airplane: crate::system_status::AirplaneStatus) -> bool {
        match &mut self.content {
            Some(PopoverContent::QuickSettings(qs)) if self.open && !self.closing => {
                qs.set_airplane(airplane)
            }
            _ => false,
        }
    }

    /// Push a fresh power-profile snapshot to an open quick-settings popover, so the "Power Mode"
    /// tile appears/vanishes with the daemon and tracks the live profile. Returns whether it
    /// changed.
    pub fn set_power_profile(&mut self, power: crate::system_status::PowerProfileStatus) -> bool {
        match &mut self.content {
            Some(PopoverContent::QuickSettings(qs)) if self.open && !self.closing => {
                qs.set_power_profile(power)
            }
            _ => false,
        }
    }

    /// Continue a quick-settings volume-slider drag at output-local `pos`; returns
    /// the action to apply, or `None` when not over a live slider drag.
    pub fn pointer_drag(
        &mut self,
        output: &Output,
        pos: Point<f64, Logical>,
    ) -> Option<PopoverAction> {
        if !self.open || self.closing || self.output.as_ref() != Some(output) {
            return None;
        }
        let local = pos - self.location(output);
        match &mut self.content {
            Some(PopoverContent::QuickSettings(qs)) => qs.pointer_drag(local),
            _ => None,
        }
    }

    /// Update the hovered control from the current pointer position. The content
    /// highlights the control under `pos` when the popover is open and `pos` is
    /// inside its content rect; otherwise the hover is cleared (the pointer left
    /// the content, or the popover is closed/closing/on another output). Returns
    /// whether the highlight changed, so the caller can redraw.
    pub fn pointer_hover(&mut self, output: &Output, pos: Point<f64, Logical>) -> bool {
        let local = if self.open && !self.closing && self.output.as_ref() == Some(output) {
            let origin = self.location(output);
            let size = self
                .content
                .as_ref()
                .map(|c| c.logical_size())
                .unwrap_or_default();
            let l = pos - origin;
            (l.x >= 0. && l.y >= 0. && l.x < size.w && l.y < size.h).then_some(l)
        } else {
            None
        };
        match self.content.as_mut() {
            Some(PopoverContent::Calendar(dm)) => dm.pointer_hover(local),
            Some(PopoverContent::QuickSettings(qs)) => qs.pointer_hover(local),
            Some(PopoverContent::InputSources(m)) => m.pointer_hover(local),
            None => false,
        }
    }

    /// End any quick-settings slider drag (pointer released). Returns whether the release changed
    /// the menu geometry (a sink hot-plugged mid-drag), so the caller can redraw.
    pub fn end_drag(&mut self) -> bool {
        if let Some(PopoverContent::QuickSettings(qs)) = &mut self.content {
            qs.end_drag()
        } else {
            false
        }
    }

    /// Whether the popover is open showing a particular content kind (so a second
    /// click on the *same* button toggles it closed, but clicking a different
    /// panel button swaps content instead of no-op-toggling). A popover that is
    /// fading out (`closing`) is not "showing", so its button re-opens it fresh.
    fn is_showing<T: ContentTag>(&self) -> bool {
        self.open && !self.closing && self.content.as_ref().is_some_and(T::matches)
    }

    /// Start the fade-out. The content stays and keeps rendering (fading) until the
    /// animation settles, when [`advance_animations`](Self::advance_animations) drops
    /// it. Idempotent while already closing.
    pub fn close(&mut self) {
        if !self.open || self.closing {
            return;
        }
        self.closing = true;
        let from = f64::from(self.progress());
        self.anim = Some(self.make_anim(from, 0.));
    }

    /// Feed a key while the popover is open. Escape closes it; every other key is
    /// swallowed (a modal grab, like GNOME popup menus). Returns whether the key
    /// was consumed. A closing (fading-out) popover no longer grabs input.
    pub fn handle_key(&mut self, raw: Option<Keysym>, pressed: bool) -> bool {
        if !self.open || self.closing {
            return false;
        }
        if pressed && raw == Some(Keysym::Escape) {
            self.close();
        }
        true
    }

    /// Feed a pointer click at output-local logical `pos` on `output`. A click
    /// inside the popover routes to the content (returning its action); anywhere
    /// else (including another output) closes it. Returns `None` when the popover
    /// wasn't open (the caller handles the click normally), or `Some(action)` when
    /// it consumed the click.
    pub fn pointer_click(
        &mut self,
        output: &Output,
        pos: Point<f64, Logical>,
    ) -> Option<PopoverAction> {
        // A closed or fading-out popover doesn't grab: let the caller handle the click
        // normally (so a click during the close fade still hits whatever is beneath).
        if !self.open || self.closing {
            return None;
        }
        if self.output.as_ref() != Some(output) {
            self.close();
            return Some(PopoverAction::Consumed);
        }
        let origin = self.location(output);
        let size = self
            .content
            .as_ref()
            .map(|c| c.logical_size())
            .unwrap_or_default();
        let local = pos - origin;
        let inside = local.x >= 0. && local.y >= 0. && local.x < size.w && local.y < size.h;
        if inside {
            let action = match self.content.as_mut() {
                Some(PopoverContent::Calendar(dm)) => dm.pointer_click(local),
                Some(PopoverContent::QuickSettings(qs)) => qs.pointer_click(local),
                Some(PopoverContent::InputSources(m)) => m.pointer_click(local),
                None => PopoverAction::Consumed,
            };
            // A system button (screenshot / settings / lock / power / pill)
            // closes the menu, like GNOME.
            if action.closes_menu() {
                self.close();
            }
            return Some(action);
        }
        // Outside click — dismiss and consume it (GNOME's grab swallows the click
        // that closes the menu rather than also acting on what's beneath).
        self.close();
        Some(PopoverAction::Consumed)
    }

    /// Whether output-local `pos` falls inside the open popover's content rect
    /// (so a wheel event there belongs to the popover, not the panel/window
    /// beneath).
    pub fn contains(&self, output: &Output, pos: Point<f64, Logical>) -> bool {
        if !self.open || self.output.as_ref() != Some(output) {
            return false;
        }
        let origin = self.location(output);
        let size = self
            .content
            .as_ref()
            .map(|c| c.logical_size())
            .unwrap_or_default();
        let local = pos - origin;
        local.x >= 0. && local.y >= 0. && local.x < size.w && local.y < size.h
    }

    /// Route a wheel/scroll of `delta` content px at output-local `pos` to the
    /// open popover (the dateMenu message list). Returns whether the content
    /// scrolled (so the caller can redraw).
    pub fn pointer_scroll(
        &mut self,
        output: &Output,
        pos: Point<f64, Logical>,
        delta: f64,
    ) -> bool {
        if !self.open || self.closing || self.output.as_ref() != Some(output) {
            return false;
        }
        let origin = self.location(output);
        let local = pos - origin;
        match self.content.as_mut() {
            Some(PopoverContent::Calendar(dm)) => dm.scroll(local, delta),
            _ => false,
        }
    }

    /// The popover's resting top-left, output-local logical: centered under the anchor,
    /// clamped into the output with a `POPOVER_MARGIN` inset from the screen edges, and
    /// sitting `POPOVER_MARGIN` below the panel (not flush); snapped to the pixel grid.
    fn location(&self, output: &Output) -> Point<f64, Logical> {
        let scale = output.current_scale().fractional_scale();
        let ow = output_size(output).w;
        let size = self
            .content
            .as_ref()
            .map(|c| c.logical_size())
            .unwrap_or_default();
        let center_x = self.anchor.loc.x + self.anchor.size.w / 2.;
        // Keep a margin from both screen edges (upper bound falls back to the lower one
        // when the popover is wider than the margined area).
        let max_x = (ow - size.w - POPOVER_MARGIN).max(POPOVER_MARGIN);
        let x = (center_x - size.w / 2.).clamp(POPOVER_MARGIN, max_x);
        Point::from((x, PANEL_HEIGHT + POPOVER_MARGIN))
            .to_physical_precise_round(scale)
            .to_logical(scale)
    }

    /// The popover render elements for `output`, or empty when closed / on another
    /// output. `icons` supplies the symbolic icons the quick-settings menu needs.
    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        icons: &IconCache,
        output: &Output,
    ) -> Vec<TextureRenderElement<VkTexture>> {
        if !self.open || self.output.as_ref() != Some(output) {
            return Vec::new();
        }
        let _span = tracy_client::span!("PanelPopover::render");
        let scale = output.current_scale().fractional_scale();
        let progress = self.progress();

        // Slide: emerge from `POPOVER_RISE` above the resting spot as it opens (and
        // slide back up on close), coupled with the fade — gnome-shell's BoxPointer.
        // Applied only here, not in `location`, so hit-testing uses the resting rect
        // (input is inactive until fully open anyway).
        let mut origin = self.location(output);
        origin.y -= POPOVER_RISE * (1. - f64::from(progress));

        let mut elements = match self.content.as_ref() {
            Some(PopoverContent::Calendar(dm)) => dm.render(renderer, icons, scale, origin),
            Some(PopoverContent::QuickSettings(qs)) => qs.render(renderer, icons, scale, origin),
            Some(PopoverContent::InputSources(m)) => m.render(renderer, icons, scale, origin),
            None => Vec::new(),
        };

        // The `.popup-menu-content` background fill, drawn ONCE by the shared chrome behind the
        // content (which bakes transparent) and above the drop shadow. This is the single home
        // for the popover box bg (`$bg_color`): the three contents used to each fill their own
        // box with a different, too-dark value. Pushed before the shadow so it lands above it.
        if let Some(content) = self.content.as_ref() {
            let card = content.logical_size();
            let radius = content.corner_radius();
            let mut cache = self.fill_cache.borrow_mut();
            match widget::bake_card_fill(
                renderer,
                &mut cache,
                scale,
                radius as u64,
                card,
                radius,
                widget::style::MENU_BG,
            ) {
                Ok(tex) => {
                    // The fill is the popover's one opaque surface, so it carries the rounded
                    // opaque region (two bands excluding the transparent corners). The content
                    // textures above it are transparent-bg and report none.
                    let opaque =
                        widget::rounded_opaque_regions(tex.size(), (radius * scale).round() as i32);
                    let buffer = TextureBuffer::from_texture(
                        renderer,
                        tex,
                        scale,
                        Transform::Normal,
                        opaque,
                    );
                    elements.push(TextureRenderElement::from_texture_buffer(
                        buffer,
                        origin,
                        1.,
                        None,
                        None,
                        Kind::Unspecified,
                    ));
                }
                Err(err) => tracing::warn!("error baking popover fill: {err:?}"),
            }
        }

        // The `.popup-menu-content` drop shadow, behind the content (appended last in the
        // FIRST=topmost Vec). Added before the fade+scale pass below so it animates with the
        // popover. Keyed by the content radius so a same-size, different-radius content re-bakes.
        if let Some(content) = self.content.as_ref() {
            let card = content.logical_size();
            let radius = content.corner_radius();
            let mut cache = self.shadow_cache.borrow_mut();
            match widget::bake_card_shadow(
                renderer,
                &mut cache,
                scale,
                radius as u64,
                card,
                radius,
                POPOVER_SHADOW,
            ) {
                Ok((tex, off)) => {
                    let loc = origin - off.to_f64().to_logical(scale);
                    let buffer = TextureBuffer::from_texture(
                        renderer,
                        tex,
                        scale,
                        Transform::Normal,
                        Vec::new(),
                    );
                    elements.push(TextureRenderElement::from_texture_buffer(
                        buffer,
                        loc,
                        1.,
                        None,
                        None,
                        Kind::Unspecified,
                    ));
                }
                Err(err) => tracing::warn!("error baking popover shadow: {err:?}"),
            }
        }

        // The `.popup-menu-content` 1px border, on TOP of everything (front of the FIRST=topmost
        // Vec) as a transparent ring texture — so a multi-texture popover (calendar column over its
        // bg box) is bordered on its true outer edge without an inner seam.
        if let Some(content) = self.content.as_ref() {
            let card = content.logical_size();
            let radius = content.corner_radius();
            let mut cache = self.border_cache.borrow_mut();
            match widget::bake_card_border(
                renderer,
                &mut cache,
                scale,
                radius as u64,
                card,
                radius,
                POPOVER_BORDER,
            ) {
                Ok(tex) => {
                    let buffer = TextureBuffer::from_texture(
                        renderer,
                        tex,
                        scale,
                        Transform::Normal,
                        Vec::new(),
                    );
                    elements.insert(
                        0,
                        TextureRenderElement::from_texture_buffer(
                            buffer,
                            origin,
                            1.,
                            None,
                            None,
                            Kind::Unspecified,
                        ),
                    );
                }
                Err(err) => tracing::warn!("error baking popover border: {err:?}"),
            }
        }

        // Fade + scale the whole popover by the open/close progress. gnome-shell's
        // BoxPointer opens from 0.96→1.0 scale about the panel-adjacent edge it emerges
        // from (its arrow); we pivot on the popover's top-center. Applied only while
        // animating (`progress < 1`), where the fade already makes every element
        // translucent — so the scaled geometry not being reflected in `opaque_regions`
        // is harmless (a translucent element reports none). At rest the elements are
        // untouched, so their opaque regions stay exact.
        if progress < 1. {
            let scale_f = 0.96 + 0.04 * f64::from(progress); // lerp(0.96, 1.0, progress)
            let menu_w = self
                .content
                .as_ref()
                .map(|c| c.logical_size().w)
                .unwrap_or_default();
            let pivot = Point::<f64, Logical>::from((origin.x + menu_w / 2., origin.y));
            for el in &mut elements {
                el.set_alpha(progress);
                let loc = el.location();
                let sz = el.logical_size();
                el.set_location(Point::from((
                    pivot.x + (loc.x - pivot.x) * scale_f,
                    pivot.y + (loc.y - pivot.y) * scale_f,
                )));
                el.set_size(Size::from((sz.w * scale_f, sz.h * scale_f)));
            }
        }
        elements
    }
}

/// Type-level tags for [`PanelPopover::is_showing`], so the toggle helpers can ask
/// "is *this* content already up?" without a public content discriminant.
trait ContentTag {
    fn matches(content: &PopoverContent) -> bool;
}
struct CalendarTag;
impl ContentTag for CalendarTag {
    fn matches(content: &PopoverContent) -> bool {
        matches!(content, PopoverContent::Calendar(_))
    }
}
struct QuickSettingsTag;
impl ContentTag for QuickSettingsTag {
    fn matches(content: &PopoverContent) -> bool {
        matches!(content, PopoverContent::QuickSettings(_))
    }
}
struct InputSourcesTag;
impl ContentTag for InputSourcesTag {
    fn matches(content: &PopoverContent) -> bool {
        matches!(content, PopoverContent::InputSources(_))
    }
}
