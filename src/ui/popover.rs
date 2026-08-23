// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>

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

use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::Texture as _;
use smithay::input::keyboard::Keysym;
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle, Size};
use synoik_config::Config;

use crate::animation::{Animation, Clock};
use crate::image_source::ImageSource;
use crate::render_helpers::icon::{AppIconCache, IconCache, ImageCache};

/// How far the popover slides in (logical px) as it fades open — gnome-shell's
/// `BoxPointer` `-arrow-rise` (`$base_padding` = 6px). It emerges from `rise` above
/// its resting spot (toward the panel) and settles down, reversing on close.
const POPOVER_RISE: f64 = 6.;

/// Resting gap between the popover and the panel / screen edge, logical px. gnome-shell's
/// `.popup-menu-boxpointer { -arrow-rise: $base_padding }` is documented as the "distance
/// from the panel & screen edge" (6px), so the menu doesn't sit flush against either.
pub(crate) const POPOVER_MARGIN: f64 = 6.;

/// Horizontal inset a **panel** menu keeps from the screen edge when its anchor is close
/// enough that the clamp binds, logical px. Not `POPOVER_MARGIN`: this one exists to line
/// the menu's edge up with the edge-most panel button's *pill*, which the `panel_button`
/// mixin already floats [`BTN_MARGIN_X`] in from the screen (`_drawing.scss`). Two
/// different jobs — one is a gap, this one is an alignment — so they are two constants
/// even while the shell's theme happens to make them close.
const PANEL_EDGE_INSET: f64 = crate::ui::panel::BTN_MARGIN_X;

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

use crate::render_helpers::texture::TextureRenderElement;
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::ui::a11y_menu::A11yMenu;
use crate::ui::app_menu::AppMenu;
use crate::ui::calendar::DateMenu;
use crate::ui::indicator_menu::IndicatorMenu;
use crate::ui::input_source_menu::{InputSourceItem, InputSourceMenu};
use crate::ui::notification_card::CardGroup;
use crate::ui::panel::panel_height;
use crate::ui::quick_settings::QuickSettings;
use crate::ui::widget;
use crate::ui::window_menu::{WindowMenu, WindowMenuContext};
use crate::ui::workspace_menu::{WorkspaceMenu, WorkspaceMenuContext};
use crate::utils::output_size;

/// The Settings app's desktop id, which every quick-settings route into Settings resolves
/// through (`js/ui/status/system.js:143`).
pub const SETTINGS_DESKTOP_ID: &str = "org.gnome.Settings.desktop";

/// The side effect a popover click asks the caller (the input handler) to apply.
/// Keeps the content widgets pure — they never touch gsettings or spawn — while
/// still driving real behavior.
#[derive(Debug, Clone, PartialEq)]
pub enum PopoverAction {
    /// The click was consumed but has no side effect (e.g. a calendar day, or a
    /// hit on empty menu space). The popover stays open.
    Consumed,
    /// Activate an app by desktop id, presenting it if it is already running
    /// (`shell_app_activate`). What gnome-shell's own quick-settings items do — its
    /// `SettingsItem` looks `org.gnome.Settings.desktop` up and calls `activate()`
    /// (`js/ui/status/system.js:133-154`), it does not spawn a command.
    ActivateApp(String),
    /// Open Settings on a named panel — gnome-shell's `launchSettingsPanel`
    /// (`js/ui/status/network.js:66-76`): the app's own `launch-panel` action, not a
    /// `gnome-control-center <panel>` spawn.
    ///
    /// **Divergence (2026-08-13):** gnome-shell's plain settings rows use
    /// `addSettingsAction(title, 'gnome-power-panel.desktop')` (`js/ui/popupMenu.js:709-721`),
    /// i.e. launching a panel-specific desktop file and letting GTK's single-instance handoff
    /// raise the running window. That handoff does not raise here: traced on the headless
    /// harness, the primary GTK instance ignores the forwarded `XDG_ACTIVATION_TOKEN` and mints
    /// its own with `set_serial(0, seat)`, which our activation gate refuses — as mutter's
    /// `token_can_activate` would too (`src/wayland/meta-wayland-activation.c:288-312`). So we
    /// take GNOME's *other* mechanism for the panel choice and do the raise ourselves.
    LaunchSettingsPanel {
        panel: String,
        args: Vec<String>,
    },
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
    /// Activate an output-device-picker row: select its card port and/or make its node the
    /// default. The menu stays open (gnome-shell keeps the device list up after picking); the
    /// check moves when the write echoes back.
    SetOutputDevice(crate::audio::AudioDeviceKey),
    /// Set the default source's perceptual volume `0..=1` (the QS mic slider).
    SetInputVolume(f64),
    /// Set the global brightness scale `0..=1` (the QS brightness slider). Resolved in
    /// `apply_popover_action`, since the scale algebra and the hardware both live on the
    /// compositor; the menu stays open.
    SetBrightness(f64),
    /// Set ONE output's brightness scale `0..=1` (a row of the per-monitor brightness card).
    /// Resolved in `apply_popover_action` like [`SetBrightness`](Self::SetBrightness); the menu
    /// stays open.
    SetMonitorBrightness(String, f64),
    /// Toggle the default source's mute (clicking the mic slider's icon).
    ToggleInputMute,
    /// Activate an input-device-picker row; the input mirror of
    /// [`SetOutputDevice`](Self::SetOutputDevice).
    SetInputDevice(crate::audio::AudioDeviceKey),
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
    /// Toggle Bluetooth (the Bluetooth tile body): gnome-shell's `toggleActive`
    /// (`bluetooth.js:120-141`) — write gsd-rfkill's `BluetoothAirplaneMode` and, when turning
    /// on, also power the adapter. Resolved in `apply_popover_action` (the writes need live
    /// compositor state); menu stays open; echo-driven apart from the predicted tile icon.
    ToggleBluetooth,
    /// Connect or disconnect a Bluetooth device (`Device1.Connect`/`Disconnect`, a device-list
    /// row). The menu stays open; the row shows a busy mark until the call finishes.
    ConnectBluetoothDevice {
        path: String,
        connect: bool,
    },
    /// Activate a row of an app indicator's remote menu — `Event(id, "clicked")` on the client.
    /// The popover closes, as a menu activation does everywhere else.
    IndicatorMenuActivate {
        item_id: String,
        node_id: i32,
    },
    /// Expand a submenu of an app indicator's remote menu. The menu stays open, and the client is
    /// told (`AboutToShow`) so it can fill the submenu in before it is drawn.
    IndicatorMenuExpand {
        item_id: String,
        node_id: i32,
    },
    /// Open the interactive screenshot UI (the screenshot system button); the
    /// popover closes.
    Screenshot,
    /// Spawn a command (a system-row button / the battery pill); popover closes.
    Spawn(Vec<String>),
    /// Ask gnome-session to start a logout / power-off / restart (the quick-settings system
    /// rows). Popover closes.
    ///
    /// gnome-shell calls `org.gnome.SessionManager` directly for these —
    /// `this._session.LogoutAsync(0)` / `ShutdownAsync(0)` / `RebootAsync()`
    /// (`systemActions.js:483-501`) — rather than running the `gnome-session-quit` helper, which
    /// is what we used to do: a whole GTK process start on the logout path.
    SessionRequest(crate::end_session::SessionRequest),
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
    ActivateNotification {
        id: u32,
        has_default: bool,
    },
    /// An expanded message-list card's action button: emit
    /// ActivationToken+ActionInvoked for `key` and destroy unless resident
    /// (`js/ui/notificationDaemon.js:224-227`, `js/ui/messageTray.js:430-442`).
    InvokeNotificationAction {
        id: u32,
        key: String,
    },
    /// The message list's Clear pill: close every notification.
    ClearNotifications,
    /// A media card's transport button (`js/ui/messageList.js:778-791`). The popover stays open —
    /// GNOME's buttons are plain `St.Button`s inside the message, not menu items.
    MediaControl {
        bus_name: String,
        control: crate::ui::media_card::MediaControl,
    },
    /// A media card's body: raise the player and close the popover
    /// (`MediaMessage.vfunc_clicked`, `js/ui/messageList.js:799-804`).
    RaiseMediaPlayer(String),
    /// Switch to this input source (a layout row in the keyboard menu): set the
    /// active xkb group and record it in `mru-sources`. The menu closes, like
    /// gnome-shell's popup menu closing on item activation.
    SetInputSource(usize),
    /// The app menu's "New Window": launch a fresh window of this app and leave the
    /// overview (`appMenu.js:57-61`).
    AppNewWindow(String),
    /// An app menu `.desktop` action row (`appMenu.js:235-241`): launch it and leave
    /// the overview.
    AppLaunchAction {
        id: String,
        action: String,
    },
    /// The app menu's "Pin to Dash" / "Unpin" (`appMenu.js:74-80`). Unlike the launch
    /// rows this does *not* leave the overview — gnome-shell only hides it for the
    /// items that raise a window.
    AppToggleFavorite(String),
    /// An "Open Windows" row (`appMenu.js:284-286`): raise that window and leave
    /// the overview, which is what `Main.activateWindow` does.
    AppActivateWindow(crate::window::mapped::MappedId),
    /// "App Details" (`appMenu.js:84-95`): ask `org.gnome.Software` to show the app.
    AppDetails(String),
    /// "Quit" (`appMenu.js:99-100`) — `shell_app_request_quit`. Unlike the launch
    /// rows this does *not* leave the overview: gnome-shell's handler is bare.
    AppQuit(String),
    /// The window menu's Take Screenshot (`windowMenu.js:26-36`): the window's own pixels,
    /// saved and put on the clipboard with a notification, and without the pointer — the null
    /// cursor `captureScreenshot(texture, null, 1, null)` passes.
    WindowTakeScreenshot(crate::window::mapped::MappedId),
    /// The window menu's Hide — `window.minimize()` (`windowMenu.js:38-42`).
    WindowMinimize(crate::window::mapped::MappedId),
    /// The window menu's Maximize / Restore (`windowMenu.js:44-55`).
    WindowSetMaximized {
        window: crate::window::mapped::MappedId,
        maximized: bool,
    },
    /// The window menu's Move and Resize — the keyboard grabs (`windowMenu.js:58-84`).
    WindowBeginMove(crate::window::mapped::MappedId),
    WindowBeginResize(crate::window::mapped::MappedId),
    /// The window menu's Always on Top (`windowMenu.js:86-98`).
    WindowSetAlwaysOnTop {
        window: crate::window::mapped::MappedId,
        above: bool,
    },
    /// The window menu's Always on Visible Workspace (`windowMenu.js:105-114`).
    WindowSetSticky {
        window: crate::window::mapped::MappedId,
        sticky: bool,
    },
    /// The window menu's "Move to Workspace Left" / "Right" (`windowMenu.js:110-135`).
    WindowMoveToWorkspace {
        window: crate::window::mapped::MappedId,
        dir: crate::ui::window_menu::WorkspaceDirection,
    },
    /// The window menu's "Move to Monitor *" (`windowMenu.js:143-181`).
    WindowMoveToMonitor {
        window: crate::window::mapped::MappedId,
        dir: crate::ui::window_menu::MonitorDirection,
    },
    /// The window menu's Close — `window.delete()` (`windowMenu.js:185-189`).
    WindowClose(crate::window::mapped::MappedId),
    /// The workspace menu's Rename — open the strip's inline name entry on this workspace.
    WorkspaceRename(crate::layout::workspace::WorkspaceId),
    /// The workspace menu's Close — `Layout::close_workspace`, the same call the strip's own
    /// close button makes.
    WorkspaceClose(crate::layout::workspace::WorkspaceId),
    /// The workspace menu's Send to <display> — the keyboard's way to say what the cross-display
    /// thumbnail drag says, through the same move (`docs/fork/multi-display.md` §6).
    WorkspaceSendToDisplay {
        workspace: crate::layout::workspace::WorkspaceId,
        output: Output,
    },
    /// Flip one accessibility menu row: write the backing gsettings key and close the
    /// menu (`PopupSwitchMenuItem.activate`, `js/ui/popupMenu.js:539-550`).
    SetA11yToggle {
        toggle: crate::gnome::A11yToggle,
        on: bool,
    },
}

impl PopoverAction {
    /// Whether applying this action dismisses the menu (GNOME closes quick
    /// settings when a system button is used, but keeps it open for a toggle).
    pub(crate) fn closes_menu(&self) -> bool {
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
                // Same as `Spawn`: gnome-shell's `SettingsItem` calls
                // `Main.panel.closeQuickSettings()` before `activate()`
                // (`js/ui/status/system.js:151-154`), and the raised window would otherwise
                // come up under our modal grab.
                | PopoverAction::ActivateApp(_)
                | PopoverAction::LaunchSettingsPanel { .. }
                | PopoverAction::SessionRequest(_)
                | PopoverAction::ActivateNotification { .. }
                | PopoverAction::InvokeNotificationAction { .. }
                // Picking a layout closes the popup, like gnome-shell's popup menu.
                | PopoverAction::SetInputSource(_)
                // Every app-menu row closes it: activating *any* `PopupMenuItem` runs
                // `menu.itemActivated()`, which closes the menu it belongs to.
                | PopoverAction::AppNewWindow(_)
                | PopoverAction::AppLaunchAction { .. }
                | PopoverAction::AppToggleFavorite(_)
                | PopoverAction::AppActivateWindow(_)
                | PopoverAction::AppDetails(_)
                | PopoverAction::AppQuit(_)
                // A switch row toggles and then falls through to `super.activate`,
                // which closes the menu — only Space keeps it open.
                | PopoverAction::SetA11yToggle { .. }
                // `vfunc_clicked` raises the player and calls `Main.panel.closeCalendar()`.
                | PopoverAction::RaiseMediaPlayer(_)
                // Activating a remote row closes the menu too — but *expanding* one does not,
                // which is why the two are separate actions.
                | PopoverAction::IndicatorMenuActivate { .. }
                // Every window-menu row is a plain `PopupMenuItem`, so activating any of them
                // runs `menu.itemActivated()` and the menu goes.
                | PopoverAction::WindowTakeScreenshot(_)
                | PopoverAction::WindowMinimize(_)
                | PopoverAction::WindowSetMaximized { .. }
                | PopoverAction::WindowBeginMove(_)
                | PopoverAction::WindowBeginResize(_)
                | PopoverAction::WindowSetAlwaysOnTop { .. }
                | PopoverAction::WindowSetSticky { .. }
                | PopoverAction::WindowMoveToWorkspace { .. }
                | PopoverAction::WindowMoveToMonitor { .. }
                | PopoverAction::WindowClose(_)
                | PopoverAction::WorkspaceRename(_)
                | PopoverAction::WorkspaceClose(_)
                | PopoverAction::WorkspaceSendToDisplay { .. }
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
    A11y(Box<A11yMenu>),
    App(Box<AppMenu>),
    /// An app indicator's remote menu, which opens empty and fills in when the client answers.
    Indicator(Box<IndicatorMenu>),
    /// A window's own menu, summoned on its titlebar.
    Window(Box<WindowMenu>),
    /// A workspace thumbnail's menu, summoned in the overview strip.
    Workspace(Box<WorkspaceMenu>),
}

impl PopoverContent {
    fn logical_size(&self) -> Size<f64, Logical> {
        match self {
            PopoverContent::Calendar(dm) => dm.logical_size(),
            PopoverContent::QuickSettings(qs) => qs.logical_size(),
            PopoverContent::InputSources(m) => m.logical_size(),
            PopoverContent::A11y(m) => m.logical_size(),
            PopoverContent::App(m) => m.logical_size(),
            PopoverContent::Indicator(m) => m.logical_size(),
            PopoverContent::Window(m) => m.logical_size(),
            PopoverContent::Workspace(m) => m.logical_size(),
        }
    }

    /// The content box's corner radius, for the `.popup-menu-content` drop shadow behind it.
    fn corner_radius(&self) -> f64 {
        match self {
            PopoverContent::Calendar(dm) => dm.corner_radius(),
            PopoverContent::QuickSettings(qs) => qs.corner_radius(),
            PopoverContent::InputSources(m) => m.corner_radius(),
            PopoverContent::A11y(m) => m.corner_radius(),
            PopoverContent::App(m) => m.corner_radius(),
            PopoverContent::Indicator(m) => m.corner_radius(),
            PopoverContent::Window(m) => m.corner_radius(),
            PopoverContent::Workspace(m) => m.corner_radius(),
        }
    }
}

/// Which side of the anchor a popover's arrow is on — so the box sits on the
/// opposite one. gnome-shell's `BoxPointer` arrow side, the third argument of
/// `PopupMenu(sourceActor, arrowAlignment, side)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopoverSide {
    /// Arrow on top → the box hangs *below* the anchor. Every panel menu.
    Top,
    /// Arrow underneath → the box sits *above* the anchor. What a dash icon's menu
    /// uses, since the dash is at the bottom of the screen (`popupMenuSide:
    /// St.Side.BOTTOM`, `dash.js:27`).
    Bottom,
    /// Arrow on the left → the box sits to the anchor's *right*. `AppIcon`'s default
    /// (`popupMenuSide ?? St.Side.LEFT`, `appDisplay.js:2928`), which is what an
    /// app-grid or search-result icon gets.
    Left,
    /// Arrow on top with alignment 0 → the box hangs *below* the anchor and is aligned to its
    /// left edge, rather than centred on it. gnome-shell's window menu, whose source actor is a
    /// zero-sized widget parked at the point the client asked for
    /// (`PopupMenu(sourceActor, 0, St.Side.TOP)`, `windowMenu.js:10-11`, over the rect
    /// `_shell_wm_show_window_menu` builds with `width = height = 0`, `shell-wm.c:336-350`).
    Point,
}

/// A single panel popover, owned on `Synoik` alongside the other overlays.
pub struct PanelPopover {
    open: bool,
    /// The output the popover is anchored on (drawn/hit-tested only there).
    output: Option<Output>,
    /// The rect it hangs from, output-local logical: a panel button, or the icon a
    /// context menu was summoned on.
    anchor: Rectangle<f64, Logical>,
    /// Which side of `anchor` the arrow is on, i.e. where the box goes.
    side: PopoverSide,
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
    /// An action a keyboard activation produced, waiting for the caller to drain it once it is
    /// out of the keyboard filter. See [`Self::handle_key`].
    pending_action: Option<PopoverAction>,
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
            side: PopoverSide::Top,
            content: None,
            clock,
            config,
            anim: None,
            closing: false,
            pending_action: None,
            shadow_cache: RefCell::new(widget::BakeCache::new()),
            border_cache: RefCell::new(widget::BakeCache::new()),
            fill_cache: RefCell::new(widget::BakeCache::new()),
        }
    }

    /// Whether the popover is showing (including while it fades out on close).
    ///
    /// This is the *rendering* question. For "does it hold the modal grab", ask
    /// [`grabs_input`](Self::grabs_input) instead — the two diverge for the length of the
    /// close fade.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Whether the popover holds the modal input grab: keyboard focus, pointer focus,
    /// clicks, scrolls and the cursor image.
    ///
    /// A *closing* popover no longer grabs, even though it is still on screen fading out.
    /// gnome-shell releases the grab synchronously at the top of the close: `close()` emits
    /// `open-state-changed, false` (`js/ui/popupMenu.js:1095`) right after merely *starting*
    /// the box-pointer ease, and `PopupMenuManager` calls `Main.popModal` from that signal
    /// (`js/ui/popupMenu.js:1487`) — which dismisses the `Clutter.Grab`, so mutter's
    /// `notify::is-grabbed` handler re-runs `get_focus_surface` and sends `wl_keyboard.enter`
    /// back to the window before the menu has faded at all. Gating on [`is_open`](Self::is_open)
    /// here instead would leave the client with no focus, no keys and no clicks for the whole
    /// 150 ms fade.
    pub fn grabs_input(&self) -> bool {
        self.open && !self.closing
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
            PopoverContent::A11y(_) => Some(crate::ui::panel::ROLE_A11Y),
            // Not a panel menu: nothing in the panel should light up for it.
            PopoverContent::App(_) => None,
            // Not a panel menu either: it hangs off a window, not off the bar.
            PopoverContent::Window(_) => None,
            // Nor this one: it hangs off a thumbnail in the overview strip.
            PopoverContent::Workspace(_) => None,
            // The indicator cluster is one panel item per icon, so the pressed-role highlight
            // would have to name *which* icon; it does not, and lighting the whole cluster would
            // be worse than lighting none of it.
            PopoverContent::Indicator(_) => None,
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
    ///
    /// Also steps the quick-settings detail view's own grow/fade, which lives in the content but
    /// needs the clock and animation config the popover holds.
    pub fn advance_animations(&mut self) {
        let (detail, dim) = {
            let c = self.config.borrow();
            (
                c.animations.quick_settings_detail_open_close.0,
                // The dim spans both phases on its own clock — quickSettings' *whole*
                // `POPUP_ANIMATION_TIME`, which is not the boxpointer one the popover opens with.
                c.animations.quick_settings_dim.0,
            )
        };
        if let Some(PopoverContent::QuickSettings(qs)) = &mut self.content {
            qs.advance_expand(&self.clock, detail, dim);
        }
        if self.closing && self.anim.as_ref().is_none_or(|a| a.is_done()) {
            self.open = false;
            self.closing = false;
            self.output = None;
            self.content = None;
            self.anim = None;
        }
    }

    /// Whether an open/close fade — the popover's own, or the quick-settings detail view's —
    /// is still running (keeps the redraw loop ticking).
    pub fn are_animations_ongoing(&self) -> bool {
        if self.anim.as_ref().is_some_and(|a| !a.is_done()) {
            return true;
        }
        matches!(&self.content, Some(PopoverContent::QuickSettings(qs)) if qs.are_animations_ongoing())
    }

    /// The output the popover is anchored on, while open.
    pub fn output(&self) -> Option<&Output> {
        self.output.as_ref()
    }

    /// The quick-settings menu, if that is what is up. A seam so a test can click a real control
    /// at the coordinates the menu itself lays out, rather than reimplementing that arithmetic.
    #[cfg(test)]
    pub fn quick_settings(&self) -> Option<&crate::ui::quick_settings::QuickSettings> {
        match self.content.as_ref() {
            Some(PopoverContent::QuickSettings(qs)) => Some(qs),
            _ => None,
        }
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
        self.side = PopoverSide::Top;
        let mut date_menu = DateMenu::new(week_start, show_week_numbers, accent, groups);
        // Grow to fit the content but stay within the work area, leaving the
        // same margin at the bottom as the top (`js/ui/panelMenu.js:177-185`,
        // `js/ui/boxpointer.js:117-137`): output height minus the panel and both
        // margins. Past this the message list scrolls.
        let available_h =
            (output_size(&output).h - panel_height() - 2. * POPOVER_MARGIN).max(POPOVER_MARGIN);
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

    /// Push a fresh MPRIS player snapshot to an open calendar popover — the media cards above the
    /// notification groups. Returns whether it changed anything.
    pub fn set_media_players(
        &mut self,
        players: Vec<crate::ui::media_card::MediaCardContent>,
    ) -> bool {
        match &mut self.content {
            Some(PopoverContent::Calendar(dm)) if self.open && !self.closing => {
                dm.set_media_players(players)
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

    /// Push a freshly-formatted World Clocks section model into the open dateMenu.
    /// Returns whether it changed anything.
    pub fn set_world_clocks(&mut self, model: crate::world_clocks::WorldClocksModel) -> bool {
        match &mut self.content {
            Some(PopoverContent::Calendar(dm)) if self.open && !self.closing => {
                dm.set_world_clocks(model)
            }
            _ => false,
        }
    }

    /// Introspection/test hook: the open dateMenu content.
    /// An album-art decode landed: tell the open message list, so a media card showing the themed
    /// fallback re-bakes with the art. Returns whether anything changed.
    pub fn note_art_decoded(&mut self, source: &ImageSource) -> bool {
        match &mut self.content {
            Some(PopoverContent::Calendar(dm)) if self.open && !self.closing => {
                dm.note_art_decoded(source)
            }
            _ => false,
        }
    }

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
        self.side = PopoverSide::Top;
        self.content = Some(PopoverContent::InputSources(InputSourceMenu::new(
            items, active,
        )));
        self.anim = Some(self.make_anim(0., 1.));
    }

    /// Toggle the accessibility menu, anchored at `anchor` on `output`
    /// (gnome-shell's `ATIndicator` menu). `settings` is the snapshot the rows show;
    /// `accent` is straight RGB, the switch's on-state fill.
    pub fn toggle_a11y(
        &mut self,
        output: Output,
        anchor: Rectangle<f64, Logical>,
        settings: crate::gnome::A11ySettings,
        accent: [u8; 3],
    ) {
        if self.is_showing::<A11yTag>() {
            self.close();
            return;
        }
        self.open = true;
        self.closing = false;
        self.output = Some(output);
        self.anchor = anchor;
        self.side = PopoverSide::Top;
        self.content = Some(PopoverContent::A11y(Box::new(A11yMenu::new(
            settings, accent,
        ))));
        self.anim = Some(self.make_anim(0., 1.));
    }

    /// The popover's content origin on `output` (its resting top-left),
    /// output-local logical — for tests that click inside the content.
    pub fn content_location(&self, output: &Output) -> Point<f64, Logical> {
        self.location(output)
    }

    /// Pop up the context menu for `entry`, anchored on the icon at `anchor` with the
    /// arrow on `side` (`AppIcon.popupMenu`, `appDisplay.js:3027-3052`).
    ///
    /// Unconditionally *opens*, where the panel menus toggle: a right-click is not a
    /// button press that latches, and a second right-click on the same icon in
    /// gnome-shell re-opens the menu rather than dismissing it (the outside-click
    /// dismissal is what closes it).
    pub fn open_app_menu(
        &mut self,
        output: Output,
        anchor: Rectangle<f64, Logical>,
        side: PopoverSide,
        ctx: &crate::ui::app_menu::AppMenuContext<'_>,
    ) {
        self.open = true;
        self.closing = false;
        self.output = Some(output);
        self.anchor = anchor;
        self.side = side;
        let mut menu = AppMenu::new(ctx);
        // GNOME caps a menu at the monitor's work area and lets the content scroll rather than
        // running off the screen (`js/ui/panelMenu.js:168-186`). An app with many open windows is
        // how this menu gets there.
        menu.set_max_height(Some(self.available_menu_height()));
        self.content = Some(PopoverContent::App(Box::new(menu)));
        self.anim = Some(self.make_anim(0., 1.));
    }

    /// Pop up a window's own menu, anchored at `anchor` — the point the client asked for, or the
    /// window's top-left for the keyboard binding. `WindowMenuManager.showWindowMenuForWindow`
    /// (`windowMenu.js:213-250`).
    ///
    /// Unconditionally opens, like the app context menu: a right-click is not a latching button.
    pub fn open_window_menu(
        &mut self,
        output: Output,
        anchor: Rectangle<f64, Logical>,
        ctx: &WindowMenuContext,
    ) {
        self.open = true;
        self.closing = false;
        self.output = Some(output);
        self.anchor = anchor;
        self.side = PopoverSide::Point;
        let mut menu = WindowMenu::new(ctx);
        menu.set_max_height(Some(self.available_menu_height()));
        // `menu.actor.navigate_focus(null, TAB_FORWARD, false)` (`windowMenu.js:247`): the menu
        // comes up with its first row focused, so Enter acts without an arrow key first.
        menu.focus_step(1);
        self.content = Some(PopoverContent::Window(Box::new(menu)));
        self.anim = Some(self.make_anim(0., 1.));
    }

    /// Open the workspace menu, anchored at a point in the overview strip like the window menu
    /// is anchored at the point a client asked for.
    pub fn open_workspace_menu(
        &mut self,
        output: Output,
        anchor: Rectangle<f64, Logical>,
        ctx: &WorkspaceMenuContext,
    ) {
        self.open = true;
        self.closing = false;
        self.output = Some(output);
        self.anchor = anchor;
        self.side = PopoverSide::Point;
        let mut menu = WorkspaceMenu::new(ctx);
        menu.set_max_height(Some(self.available_menu_height()));
        // Opened with the first row focused, like the window menu, so Enter acts without an arrow
        // key first — which is the whole point of this menu existing.
        menu.focus_step(1);
        self.content = Some(PopoverContent::Workspace(Box::new(menu)));
        self.anim = Some(self.make_anim(0., 1.));
    }

    /// The open workspace menu, if that is what is up — for the corpus, and for the paths that
    /// have to take the menu down with the workspace it names.
    pub fn workspace_menu(&self) -> Option<&WorkspaceMenu> {
        match &self.content {
            Some(PopoverContent::Workspace(m)) if self.open && !self.closing => Some(m),
            _ => None,
        }
    }

    /// The open window menu, if that is what is up — for the corpus, and for the unmap path that
    /// has to take the menu down with its window.
    pub fn window_menu(&self) -> Option<&WindowMenu> {
        match &self.content {
            Some(PopoverContent::Window(m)) if self.open && !self.closing => Some(m),
            _ => None,
        }
    }

    /// Close the menu if it belongs to `window`. gnome-shell wires the same thing to the window's
    /// `unmanaged` signal (`windowMenu.js:235-237`): a menu whose window is gone acts on nothing,
    /// and would keep the modal grab over whatever the focus fell back to.
    pub fn close_window_menu_for(&mut self, window: crate::window::mapped::MappedId) -> bool {
        if self.window_menu().map(|m| m.window()) != Some(window) {
            return false;
        }
        self.close();
        true
    }

    /// Pop up an app indicator's menu, anchored on its panel icon.
    ///
    /// Opens **empty**: the rows are a client's to send, and asking for them is a round trip. The
    /// menu is a box with nothing in it until [`Self::set_indicator_layout`] lands, which is what
    /// the extension's menus do as well — its `RemoteMenu` populates asynchronously
    /// (`dbusMenu.js:880-900`).
    pub fn toggle_indicator_menu(
        &mut self,
        output: Output,
        anchor: Rectangle<f64, Logical>,
        item_id: String,
    ) {
        // A second click on the same icon dismisses, like every other panel button. A click on a
        // *different* indicator swaps the menu rather than closing.
        if self.indicator_menu().map(|m| m.item_id()) == Some(item_id.as_str()) {
            self.close();
            return;
        }
        self.open = true;
        self.closing = false;
        self.output = Some(output);
        self.anchor = anchor;
        self.side = PopoverSide::Top;
        let mut menu = IndicatorMenu::new(item_id);
        menu.set_max_height(Some(self.available_menu_height()));
        self.content = Some(PopoverContent::Indicator(Box::new(menu)));
        self.anim = Some(self.make_anim(0., 1.));
    }

    /// How tall a panel menu may be before it must scroll: the work area under the panel, less the
    /// margin the box keeps from the screen edge (`js/ui/panelMenu.js:168-186`).
    fn available_menu_height(&self) -> f64 {
        let Some(output) = self.output.as_ref() else {
            return 0.;
        };
        (output_size(output).h - crate::ui::panel::panel_height() - 2. * POPOVER_MARGIN).max(0.)
    }

    /// The open indicator menu, if that is what is up.
    pub fn indicator_menu(&self) -> Option<&IndicatorMenu> {
        match &self.content {
            Some(PopoverContent::Indicator(m)) if self.open && !self.closing => Some(m),
            _ => None,
        }
    }

    /// The item whose menu is up — what the watcher is asked to follow.
    pub fn indicator_menu_item(&self) -> Option<&str> {
        self.indicator_menu().map(|m| m.item_id())
    }

    /// A client's menu layout arrived. Ignored unless it is for the menu that is actually up: a
    /// layout that lost the race with a dismissal has nowhere to go. Returns whether it changed
    /// anything drawn.
    pub fn set_indicator_layout(
        &mut self,
        item_id: &str,
        root: &crate::dbusmenu::MenuNode,
    ) -> bool {
        match &mut self.content {
            Some(PopoverContent::Indicator(m)) if self.open && m.item_id() == item_id => {
                m.set_layout(root)
            }
            _ => false,
        }
    }

    /// Whether an app context menu is the content that is up — the overview closes it
    /// on the way out (`Main.overview.connectObject('hiding', …)`, `appDisplay.js:3040`).
    pub fn is_app_menu(&self) -> bool {
        self.open && matches!(self.content, Some(PopoverContent::App(_)))
    }

    /// The open app context menu, for the corpus (which reads its rows back).
    pub fn app_menu(&self) -> Option<&AppMenu> {
        match &self.content {
            Some(PopoverContent::App(m)) if self.open => Some(m),
            _ => None,
        }
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
        bluetooth: crate::system_status::BluetoothStatus,
        bluetooth_rfkill: crate::system_status::BluetoothRfkill,
        battery: Option<crate::system_status::BatteryStatus>,
        audio: Option<crate::audio::AudioStatus>,
        sink_list: crate::audio::SinkList,
        cards: crate::audio::AudioCards,
        headphones: bool,
        mic: crate::audio::MicStatus,
        source_list: crate::audio::SourceList,
        brightness: crate::brightness::BrightnessView,
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
        self.side = PopoverSide::Top;
        self.content = Some(PopoverContent::QuickSettings(Box::new(QuickSettings::new(
            toggles,
            network,
            airplane,
            power,
            bluetooth,
            bluetooth_rfkill,
            battery,
            audio,
            sink_list,
            cards,
            headphones,
            mic,
            source_list,
            brightness,
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

    /// Push a fresh card/route model to an open quick-settings popover, so the device pickers track
    /// ports appearing/disappearing (a headphone plug is exactly that). Returns whether it changed.
    pub fn set_audio_cards(&mut self, cards: crate::audio::AudioCards) -> bool {
        match &mut self.content {
            Some(PopoverContent::QuickSettings(qs)) if self.open && !self.closing => {
                qs.set_audio_cards(cards)
            }
            _ => false,
        }
    }

    /// Push the headphone state to an open quick-settings popover, so its volume-slider icon swaps
    /// to `audio-headphones-symbolic`. Returns whether it changed anything.
    pub fn set_headphones(&mut self, headphones: bool) -> bool {
        match &mut self.content {
            Some(PopoverContent::QuickSettings(qs)) if self.open && !self.closing => {
                qs.set_headphones(headphones)
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

    /// Push fresh accessibility state to an open a11y menu. GNOME's rows are
    /// `settings.bind`-ed (`accessibility.js:110`), so a key written by anyone else — a
    /// Settings window, another shell, `gsettings set` — moves the switch under the open
    /// menu. Returns whether it changed.
    ///
    /// Deliberately NOT gated on `!self.closing`: the row the user just clicked flips its
    /// own key *and* starts the close fade, and GNOME's `settings.bind` moves that switch
    /// synchronously — so the user watches it travel as the menu fades. A closing menu is
    /// still rendered, and [`A11yMenu::set_settings`] bumps the revision, so the bake
    /// follows.
    pub fn set_a11y(&mut self, a11y: crate::gnome::A11ySettings) -> bool {
        match &mut self.content {
            Some(PopoverContent::A11y(m)) if self.open => m.set_settings(a11y),
            _ => false,
        }
    }

    /// The menu-local center of a11y menu row `k`, so a conformance test can click a row
    /// without duplicating the menu's padding and row height. Test-only.
    #[cfg(test)]
    pub fn a11y_row_center(&self, k: usize) -> Option<Point<f64, Logical>> {
        match self.content.as_ref()? {
            PopoverContent::A11y(m) => Some(m.row_center(k)),
            _ => None,
        }
    }

    /// Whether a11y menu row `k`'s switch currently reads as on. Test-only.
    #[cfg(test)]
    pub fn a11y_row_state(&self, k: usize) -> Option<bool> {
        match self.content.as_ref()? {
            PopoverContent::A11y(m) => m.row_state(k),
            _ => None,
        }
    }

    /// Open the brightness card on an open quick-settings popover. Test-only: the real path is a
    /// click on the slider's picker arrow, whose position the render tests would have to
    /// re-derive.
    #[cfg(test)]
    pub fn open_brightness_card_for_test(&mut self) {
        if let Some(PopoverContent::QuickSettings(qs)) = &mut self.content {
            qs.open_brightness_card_for_test();
        }
    }

    /// Push a fresh brightness snapshot to an open quick-settings popover, so the brightness
    /// slider tracks the hardware and appears/disappears with the backlight. Returns whether it
    /// changed.
    pub fn set_brightness(&mut self, brightness: crate::brightness::BrightnessView) -> bool {
        match &mut self.content {
            Some(PopoverContent::QuickSettings(qs)) if self.open && !self.closing => {
                qs.set_brightness(brightness)
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

    /// Push a fresh Bluetooth adapter/device snapshot to an open quick-settings popover, so the
    /// tile and an open device list track live changes. Returns whether it changed.
    pub fn set_bluetooth(&mut self, bluetooth: crate::system_status::BluetoothStatus) -> bool {
        match &mut self.content {
            Some(PopoverContent::QuickSettings(qs)) if self.open && !self.closing => {
                qs.set_bluetooth(bluetooth)
            }
            _ => false,
        }
    }

    /// Push a fresh Bluetooth rfkill snapshot to an open quick-settings popover, so the Bluetooth
    /// tile appears/vanishes with its kill switch. Returns whether it changed.
    pub fn set_bluetooth_rfkill(&mut self, rfkill: crate::system_status::BluetoothRfkill) -> bool {
        match &mut self.content {
            Some(PopoverContent::QuickSettings(qs)) if self.open && !self.closing => {
                qs.set_bluetooth_rfkill(rfkill)
            }
            _ => false,
        }
    }

    /// A Bluetooth `Connect`/`Disconnect` we issued finished: clear that row's busy mark in an
    /// open quick-settings popover. Returns whether anything changed (→ redraw).
    pub fn bluetooth_connect_done(&mut self, path: &str) -> bool {
        match &mut self.content {
            Some(PopoverContent::QuickSettings(qs)) if self.open && !self.closing => {
                qs.bluetooth_connect_done(path)
            }
            _ => false,
        }
    }

    /// The 30 s failsafe on the Bluetooth tile's predicted state (see
    /// [`QuickSettings::clear_bluetooth_prediction`]). Returns whether anything changed.
    pub fn clear_bluetooth_prediction(&mut self) -> bool {
        match &mut self.content {
            Some(PopoverContent::QuickSettings(qs)) if self.open && !self.closing => {
                qs.clear_bluetooth_prediction()
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
            Some(PopoverContent::A11y(m)) => m.pointer_hover(local),
            Some(PopoverContent::App(m)) => m.pointer_hover(local),
            Some(PopoverContent::Indicator(m)) => m.pointer_hover(local),
            Some(PopoverContent::Window(m)) => m.pointer_hover(local),
            Some(PopoverContent::Workspace(m)) => m.pointer_hover(local),
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

    /// Close with no fade — GNOME's `close(PopupAnimation.NONE)`.
    ///
    /// For the one caller that must not merely *start* the menu going away: the screenshot button
    /// freezes the screen, and a fading popover is still on the screen it freezes. GNOME closes
    /// this menu without animation and defers the open to a `BEFORE_REDRAW` later
    /// (`js/ui/status/system.js:121-128`) for exactly that reason.
    pub fn close_immediately(&mut self) {
        if !self.open {
            return;
        }
        self.open = false;
        self.closing = false;
        self.output = None;
        self.content = None;
        self.anim = None;
    }

    /// Feed a key while the popover is open. Escape closes it; a window menu also takes the
    /// arrows and Enter/Space, since it is the one popover with a keyboard way in
    /// (`activate-window-menu`). Every other key is swallowed (a modal grab, like GNOME popup
    /// menus). Returns whether the key was consumed. A closing (fading-out) popover no longer
    /// grabs input.
    ///
    /// An activated row's action is *parked* on [`Self::take_pending_action`] rather than
    /// returned: this runs inside smithay's keyboard filter, which holds the keyboard borrowed,
    /// and applying an action there would re-enter it. The caller drains it once `input()` is
    /// done.
    ///
    /// The pointer-summoned menus (app, indicator) have no key navigation because they have no
    /// keyboard trigger to arrive from; they get it when they get one.
    pub fn handle_key(&mut self, raw: Option<Keysym>, pressed: bool) -> bool {
        if !self.open || self.closing {
            return false;
        }
        if !pressed {
            return true;
        }
        if raw == Some(Keysym::Escape) {
            self.close();
            return true;
        }
        let Some(PopoverContent::Window(menu)) = self.content.as_mut() else {
            return true;
        };
        match raw {
            Some(Keysym::Down | Keysym::Tab) => {
                menu.focus_step(1);
            }
            Some(Keysym::Up | Keysym::ISO_Left_Tab) => {
                menu.focus_step(-1);
            }
            Some(Keysym::Return | Keysym::KP_Enter | Keysym::space) => {
                let action = menu.activate_focused();
                if action.closes_menu() {
                    self.close();
                }
                self.pending_action = Some(action);
            }
            _ => (),
        }
        true
    }

    /// Take the action a keyboard activation parked, if any. See [`Self::handle_key`].
    pub fn take_pending_action(&mut self) -> Option<PopoverAction> {
        self.pending_action.take()
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
                Some(PopoverContent::A11y(m)) => m.pointer_click(local),
                Some(PopoverContent::App(m)) => m.pointer_click(local),
                Some(PopoverContent::Indicator(m)) => m.pointer_click(local),
                Some(PopoverContent::Window(m)) => m.pointer_click(local),
                Some(PopoverContent::Workspace(m)) => m.pointer_click(local),
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
            // A menu only takes the wheel when it has somewhere to go; a short one leaves the
            // event for whatever is behind it.
            Some(PopoverContent::App(m)) => m.scroll(delta),
            Some(PopoverContent::Indicator(m)) => m.scroll(delta),
            Some(PopoverContent::Window(m)) => m.scroll(delta),
            _ => false,
        }
    }

    /// The open popover's content size, or `None` when closed. With [`Self::location`],
    /// enough to pin where the surface actually lands on an output — which is all this is
    /// for, hence the gate; the render path takes the size from the content directly.
    #[cfg(test)]
    pub(crate) fn content_size(&self) -> Option<Size<f64, Logical>> {
        self.content.as_ref().map(|c| c.logical_size())
    }

    /// The popover's resting top-left, output-local logical: centered under the anchor,
    /// clamped into the output, and sitting `POPOVER_MARGIN` below the panel (not flush);
    /// snapped to the pixel grid.
    ///
    /// A **panel** menu's horizontal clamp insets by [`PANEL_EDGE_INSET`], not
    /// `POPOVER_MARGIN`: an edge-most panel button's own pill already stops that far in, so
    /// this is what lines the menu's edge up with the button's instead of leaving it 2px
    /// short. It bites on the roles that live at the ends of the bar — with the clock now
    /// in the right corner ([`crate::ui::panel`]), the calendar is clamped every time.
    pub(crate) fn location(&self, output: &Output) -> Point<f64, Logical> {
        let scale = output.current_scale().fractional_scale();
        let os = output_size(output);
        let size = self
            .content
            .as_ref()
            .map(|c| c.logical_size())
            .unwrap_or_default();
        // Keep a margin from the screen edges on both axes (each upper bound falls back
        // to the lower one when the popover is larger than the margined area).
        let x_inset = match self.side {
            PopoverSide::Top => PANEL_EDGE_INSET,
            _ => POPOVER_MARGIN,
        };
        let max_x = (os.w - size.w - x_inset).max(x_inset);
        let max_y = (os.h - size.h - POPOVER_MARGIN).max(POPOVER_MARGIN);
        let centered_x =
            (self.anchor.loc.x + self.anchor.size.w / 2. - size.w / 2.).clamp(x_inset, max_x);

        let loc = match self.side {
            // Panel menus: centered under the panel, ignoring the anchor's own y.
            // gnome-shell would place this off the anchor like the others; ours predates
            // the anchored path and the panel is the only Top user, so it stays literal
            // until there is a second one to generalize against.
            PopoverSide::Top => Point::from((centered_x, panel_height() + POPOVER_MARGIN)),
            PopoverSide::Bottom => Point::from((
                centered_x,
                (self.anchor.loc.y - POPOVER_MARGIN - size.h).clamp(POPOVER_MARGIN, max_y),
            )),
            PopoverSide::Left => Point::from((
                (self.anchor.loc.x + self.anchor.size.w + POPOVER_MARGIN)
                    .clamp(POPOVER_MARGIN, max_x),
                // Vertically centred on the anchor, the `0.5` arrow alignment every
                // `AppIcon` menu is built with (`appDisplay.js:3031`).
                (self.anchor.loc.y + self.anchor.size.h / 2. - size.h / 2.)
                    .clamp(POPOVER_MARGIN, max_y),
            )),
            // Straight off the anchor point, not centred on it: the window menu's arrow
            // alignment is 0, so the box's left edge lines up with where the click was.
            PopoverSide::Point => Point::from((
                (self.anchor.loc.x).clamp(POPOVER_MARGIN, max_x),
                (self.anchor.loc.y + self.anchor.size.h).clamp(POPOVER_MARGIN, max_y),
            )),
        };
        loc.to_physical_precise_round(scale).to_logical(scale)
    }

    /// The popover render elements for `output`, or empty when closed / on another
    /// output. `icons` supplies the symbolic icons the quick-settings menu needs.
    pub fn render(
        &self,
        renderer: &mut VulkanRenderer,
        icons: &IconCache,
        app_icons: &AppIconCache,
        images: &ImageCache,
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
            Some(PopoverContent::Calendar(dm)) => {
                dm.render(renderer, icons, app_icons, images, scale, origin)
            }
            Some(PopoverContent::QuickSettings(qs)) => qs.render(renderer, icons, scale, origin),
            Some(PopoverContent::InputSources(m)) => m.render(renderer, icons, scale, origin),
            Some(PopoverContent::A11y(m)) => m.render(renderer, scale, origin),
            Some(PopoverContent::App(m)) => m.render(renderer, scale, origin),
            Some(PopoverContent::Indicator(m)) => m.render(renderer, scale, origin),
            Some(PopoverContent::Window(m)) => m.render(renderer, scale, origin),
            Some(PopoverContent::Workspace(m)) => m.render(renderer, scale, origin),
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
                    let opaque = widget::rounded_opaque_regions(
                        tex.texture().size(),
                        (radius * scale).round() as i32,
                    );
                    let mut buffer = tex;
                    buffer.set_opaque_regions(opaque);
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
                Ok((buffer, off)) => {
                    let loc = origin - off.to_f64().to_logical(scale);
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
                Ok(buffer) => {
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
struct A11yTag;
impl ContentTag for A11yTag {
    fn matches(content: &PopoverContent) -> bool {
        matches!(content, PopoverContent::A11y(_))
    }
}
