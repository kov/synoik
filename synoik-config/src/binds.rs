// SPDX-License-Identifier: GPL-3.0-only
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

use bitflags::bitflags;
use smithay::input::keyboard::Keysym;
use synoik_ipc::{
    ColumnDisplay, LayoutSwitchTarget, PositionChange, SizeChange, WorkspaceReferenceArg,
};

use crate::utils::MergeWith;

/// A binding's trigger and the modifiers held with it.
///
/// There is no longer a `Bind` to go with it: keybindings come from GSettings, and
/// this is what an accelerator parses into. See `docs/fork/keybindings-port.md`.
#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub struct Key {
    pub trigger: Trigger,
    pub modifiers: Modifiers,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub enum Trigger {
    Keysym(Keysym),
    MouseLeft,
    MouseRight,
    MouseMiddle,
    MouseBack,
    MouseForward,
    WheelScrollDown,
    WheelScrollUp,
    WheelScrollLeft,
    WheelScrollRight,
    TouchpadScrollDown,
    TouchpadScrollUp,
    TouchpadScrollLeft,
    TouchpadScrollRight,
    TabletStylusButton1,
    TabletStylusButton2,
    TabletStylusButton3,
}

impl Trigger {
    /// The non-keyboard triggers by name, case-insensitively — everything a
    /// binding can be on that is not a keysym.
    ///
    /// Shared by the KDL key syntax and the accelerator parser, which both spell
    /// these the same way, so `<Super>WheelScrollDown` in our GSettings schema and
    /// `Mod+WheelScrollDown` in a config file cannot drift apart.
    ///
    /// Never returns [`Trigger::Keysym`]: a name that is not one of these is a
    /// keysym, and resolving it is the caller's business.
    pub fn from_name(name: &str) -> Option<Self> {
        let trigger = match () {
            _ if name.eq_ignore_ascii_case("MouseLeft") => Self::MouseLeft,
            _ if name.eq_ignore_ascii_case("MouseRight") => Self::MouseRight,
            _ if name.eq_ignore_ascii_case("MouseMiddle") => Self::MouseMiddle,
            _ if name.eq_ignore_ascii_case("MouseBack") => Self::MouseBack,
            _ if name.eq_ignore_ascii_case("MouseForward") => Self::MouseForward,
            _ if name.eq_ignore_ascii_case("WheelScrollDown") => Self::WheelScrollDown,
            _ if name.eq_ignore_ascii_case("WheelScrollUp") => Self::WheelScrollUp,
            _ if name.eq_ignore_ascii_case("WheelScrollLeft") => Self::WheelScrollLeft,
            _ if name.eq_ignore_ascii_case("WheelScrollRight") => Self::WheelScrollRight,
            _ if name.eq_ignore_ascii_case("TouchpadScrollDown") => Self::TouchpadScrollDown,
            _ if name.eq_ignore_ascii_case("TouchpadScrollUp") => Self::TouchpadScrollUp,
            _ if name.eq_ignore_ascii_case("TouchpadScrollLeft") => Self::TouchpadScrollLeft,
            _ if name.eq_ignore_ascii_case("TouchpadScrollRight") => Self::TouchpadScrollRight,
            _ if name.eq_ignore_ascii_case("TabletStylusButton1") => Self::TabletStylusButton1,
            _ if name.eq_ignore_ascii_case("TabletStylusButton2") => Self::TabletStylusButton2,
            _ if name.eq_ignore_ascii_case("TabletStylusButton3") => Self::TabletStylusButton3,
            _ => return None,
        };
        Some(trigger)
    }
}

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct Modifiers : u8 {
        const CTRL = 1;
        const SHIFT = 1 << 1;
        const ALT = 1 << 2;
        const SUPER = 1 << 3;
        const ISO_LEVEL3_SHIFT = 1 << 4;
        const ISO_LEVEL5_SHIFT = 1 << 5;
        const COMPOSITOR = 1 << 6;
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
pub struct SwitchBinds {
    pub lid_open: Option<SwitchAction>,
    pub lid_close: Option<SwitchAction>,
    pub tablet_mode_on: Option<SwitchAction>,
    pub tablet_mode_off: Option<SwitchAction>,
}

impl MergeWith<SwitchBinds> for SwitchBinds {
    fn merge_with(&mut self, part: &SwitchBinds) {
        merge_clone_opt!(
            (self, part),
            lid_open,
            lid_close,
            tablet_mode_on,
            tablet_mode_off,
        );
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SwitchAction {
    pub spawn: Vec<String>,
}

// Remember to add new actions to the CLI enum too.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Quit(bool),
    ChangeVt(i32),
    Suspend,
    PowerOffMonitors,
    PowerOnMonitors,
    Logout,
    PowerOff,
    Reboot,
    ToggleDebugTint,
    DebugToggleOpaqueRegions,
    DebugToggleDamage(Option<u8>),
    DebugDumpScanout,
    DebugToggleDeadlineDispatch,
    DebugSetRenderTimeMargin(f64),
    /// Percentage, UPower state spelling, UPower warning-level spelling.
    DebugSetBattery(f64, String, String),
    DebugToggleOutput(String),
    DebugInsertText(String),
    Spawn(Vec<String>),
    SpawnSh(String),
    DoScreenTransition(Option<u16>),
    ConfirmScreenshot {
        write_to_disk: bool,
    },
    CancelScreenshot,
    ScreenshotTogglePointer,
    /// Pick the area type from the screenshot UI's type row (its `s`/`c`/`w` keys).
    ScreenshotTypeSelection,
    ScreenshotTypeScreen,
    ScreenshotTypeWindow,
    /// Flip the screenshot UI between its shot and cast modes (its `v` key).
    ScreenshotToggleCast,
    /// Open the screenshot UI. The pointer toggle is the UI's own, remembered across opens.
    Screenshot(
        // Path; not settable from knuffel
        Option<String>,
    ),
    ScreenshotScreen(
        bool,
        bool,
        // Path; not settable from knuffel
        Option<String>,
    ),
    ScreenshotWindow(
        bool,
        bool,
        // Path; not settable from knuffel
        Option<String>,
    ),
    ScreenshotWindowById {
        id: u64,
        write_to_disk: bool,
        show_pointer: bool,
        path: Option<String>,
    },
    ToggleKeyboardShortcutsInhibit,
    /// Give the keyboard shortcuts back unconditionally, the way GNOME's
    /// `restore-shortcuts` does (mutter `handle_restore_shortcuts`,
    /// `keybindings.c:2503` → `meta_wayland_compositor_restore_shortcuts`,
    /// `meta-wayland.c:1155`): a no-op when nothing is inhibiting, and never
    /// the reverse. That asymmetry is the point — this is a recovery key, so
    /// it must not be able to *start* inhibiting the way a toggle would.
    RestoreKeyboardShortcuts,
    CloseWindow,
    CloseWindowById(u64),
    /// `activate-window-menu`: pop the focused window's own menu
    /// (`handle_activate_window_menu`, `keybindings.c:1999-2021`).
    ShowWindowMenu,
    /// `minimize`: hide the focused window (`meta_window_minimize`, `window.c:2734-2771`).
    MinimizeWindow,
    /// `always-on-top` / `toggle-above`: keep the focused window over the others
    /// (`meta_window_make_above` / `unmake_above`, `window.c:3622-3639`).
    ToggleWindowAlwaysOnTop,
    /// `raise`: bring the focused window to the top of its stacking band
    /// (`meta_window_raise`, `window.c:5404-5442`).
    RaiseWindow,
    /// `lower`: send the focused window to the bottom of its stacking band
    /// (`meta_window_lower`, `window.c:5467-5475`).
    LowerWindow,
    /// `raise-or-lower`: raise the focused window if something covers it, else lower it
    /// (`handle_raise_or_lower`, `keybindings.c:2359-2402`).
    RaiseOrLowerWindow,
    /// `begin-move`: start the keyboard move grab on the focused window
    /// (`handle_begin_move`, `keybindings.c:2194-2218`).
    BeginWindowMove,
    /// `begin-resize`: start the keyboard resize grab on the focused window
    /// (`handle_begin_resize`, `keybindings.c:2220-2244`).
    BeginWindowResize,
    /// `toggle-on-all-workspaces`: stick or unstick the focused window
    /// (`handle_toggle_on_all_workspaces`, `keybindings.c:2245-2255`).
    ToggleWindowOnAllWorkspaces,
    FullscreenWindow,
    FullscreenWindowById(u64),
    ToggleWindowedFullscreen,
    ToggleWindowedFullscreenById(u64),
    FocusWindow(u64),
    FocusWindowInColumn(u8),
    FocusWindowPrevious,
    FocusColumnLeft,
    FocusColumnLeftUnderMouse,
    FocusColumnRight,
    FocusColumnRightUnderMouse,
    FocusColumnFirst,
    FocusColumnLast,
    FocusColumnRightOrFirst,
    FocusColumnLeftOrLast,
    FocusColumn(usize),
    FocusWindowOrMonitorUp,
    FocusWindowOrMonitorDown,
    FocusColumnOrMonitorLeft,
    FocusColumnOrMonitorRight,
    FocusWindowDown,
    FocusWindowUp,
    FocusWindowDownOrColumnLeft,
    FocusWindowDownOrColumnRight,
    FocusWindowUpOrColumnLeft,
    FocusWindowUpOrColumnRight,
    FocusWindowOrWorkspaceDown,
    FocusWindowOrWorkspaceUp,
    FocusWindowTop,
    FocusWindowBottom,
    FocusWindowDownOrTop,
    FocusWindowUpOrBottom,
    MoveColumnLeft,
    MoveColumnRight,
    MoveColumnToFirst,
    MoveColumnToLast,
    MoveColumnLeftOrToMonitorLeft,
    MoveColumnRightOrToMonitorRight,
    MoveColumnToIndex(usize),
    MoveWindowDown,
    MoveWindowUp,
    MoveWindowDownOrToWorkspaceDown,
    MoveWindowUpOrToWorkspaceUp,
    ConsumeOrExpelWindowLeft,
    ConsumeOrExpelWindowLeftById(u64),
    ConsumeOrExpelWindowRight,
    ConsumeOrExpelWindowRightById(u64),
    ConsumeWindowIntoColumn,
    ExpelWindowFromColumn,
    SwapWindowLeft,
    SwapWindowRight,
    ToggleColumnTabbedDisplay,
    SetColumnDisplay(ColumnDisplay),
    CenterColumn,
    CenterWindow,
    CenterWindowById(u64),
    CenterVisibleColumns,
    FocusWorkspaceDown,
    FocusWorkspaceUp,
    FocusWorkspace(WorkspaceReference),
    FocusWorkspacePrevious,
    MoveWindowToWorkspaceDown(bool),
    MoveWindowToWorkspaceUp(bool),
    MoveWindowToWorkspace(WorkspaceReference, bool),
    MoveWindowToWorkspaceById {
        window_id: u64,
        reference: WorkspaceReference,
        focus: bool,
    },
    MoveColumnToWorkspaceDown(bool),
    MoveColumnToWorkspaceUp(bool),
    MoveColumnToWorkspace(WorkspaceReference, bool),
    MoveWorkspaceDown,
    MoveWorkspaceUp,
    MoveWorkspaceToIndex(usize),
    MoveWorkspaceToIndexByRef {
        new_idx: usize,
        reference: WorkspaceReference,
    },
    MoveWorkspaceToMonitorByRef {
        output_name: String,
        reference: WorkspaceReference,
    },
    MoveWorkspaceToMonitor(String),
    /// Pop the active workspace's own menu — rename, close, send to another display. The
    /// keyboard's way to reach what a right-click on its thumbnail reaches
    /// (`docs/fork/multi-display.md` §6); it opens the overview if it is not already up, since
    /// the menu hangs off a thumbnail in the strip.
    ShowWorkspaceMenu,
    SetWorkspaceName(String),
    SetWorkspaceNameByRef {
        name: String,
        reference: WorkspaceReference,
    },
    UnsetWorkspaceName,
    UnsetWorkSpaceNameByRef(WorkspaceReference),
    FocusMonitorLeft,
    FocusMonitorRight,
    FocusMonitorDown,
    FocusMonitorUp,
    FocusMonitorPrevious,
    FocusMonitorNext,
    FocusMonitor(String),
    MoveWindowToMonitorLeft,
    MoveWindowToMonitorRight,
    MoveWindowToMonitorDown,
    MoveWindowToMonitorUp,
    MoveWindowToMonitorPrevious,
    MoveWindowToMonitorNext,
    MoveWindowToMonitor(String),
    MoveWindowToMonitorById {
        id: u64,
        output: String,
    },
    MoveColumnToMonitorLeft,
    MoveColumnToMonitorRight,
    MoveColumnToMonitorDown,
    MoveColumnToMonitorUp,
    MoveColumnToMonitorPrevious,
    MoveColumnToMonitorNext,
    MoveColumnToMonitor(String),
    SetWindowWidth(SizeChange),
    SetWindowWidthById {
        id: u64,
        change: SizeChange,
    },
    SetWindowHeight(SizeChange),
    SetWindowHeightById {
        id: u64,
        change: SizeChange,
    },
    ResetWindowHeight,
    ResetWindowHeightById(u64),
    SwitchPresetColumnWidth,
    SwitchPresetColumnWidthBack,
    SwitchPresetWindowWidth,
    SwitchPresetWindowWidthBack,
    SwitchPresetWindowWidthById(u64),
    SwitchPresetWindowWidthBackById(u64),
    SwitchPresetWindowHeight,
    SwitchPresetWindowHeightBack,
    SwitchPresetWindowHeightById(u64),
    SwitchPresetWindowHeightBackById(u64),
    MaximizeColumn,
    MaximizeWindowToEdges,
    MaximizeWindowToEdgesById(u64),
    SetColumnWidth(SizeChange),
    ExpandColumnToAvailableWidth,
    SwitchLayout(LayoutSwitchTarget),
    ShowHotkeyOverlay,
    MoveWorkspaceToMonitorLeft,
    MoveWorkspaceToMonitorRight,
    MoveWorkspaceToMonitorDown,
    MoveWorkspaceToMonitorUp,
    MoveWorkspaceToMonitorPrevious,
    MoveWorkspaceToMonitorNext,
    ToggleWindowFloating,
    ToggleWindowFloatingById(u64),
    MoveWindowToFloating,
    MoveWindowToFloatingById(u64),
    MoveWindowToTiling,
    MoveWindowToTilingById(u64),
    FocusFloating,
    FocusTiling,
    SwitchFocusBetweenFloatingAndTiling,
    MoveFloatingWindowById {
        id: Option<u64>,
        x: PositionChange,
        y: PositionChange,
    },
    ToggleWindowRuleOpacity,
    ToggleWindowRuleOpacityById(u64),
    SetDynamicCastWindow,
    SetDynamicCastWindowById(u64),
    SetDynamicCastMonitor(Option<String>),
    ClearDynamicCastTarget,
    StopCast(u64),
    /// GNOME's `switch-to-workspace-last` (`<Super>End`): the last workspace on
    /// the active monitor, `get_workspace_by_index(n_workspaces - 1)`
    /// (`windowManager.js`, `_showWorkspaceSwitcher`) — the trailing empty one
    /// included, since it is a workspace like any other under dynamic workspaces.
    FocusWorkspaceLast,
    /// GNOME's `move-to-workspace-last` (`<Super><Shift>End`). The flag is
    /// whether to follow the window, matching [`MoveWindowToWorkspace`].
    MoveWindowToWorkspaceLast(bool),
    /// GNOME's `switch-to-application-N` (`<Super>1..9`): activate the Nth dash
    /// favourite — launch it if stopped, else raise its most recently used
    /// window (`_switchToApplication`, `windowManager.js:1725`).
    SwitchToApplication(u8),
    /// GNOME's `open-new-window-application-N` (`<Super><Ctrl>1..9`): ask the Nth
    /// favourite for another window rather than raising the one it has.
    OpenNewWindowApplication(u8),
    ToggleOverview,
    /// GNOME's `toggle-application-view` (`<Super>a`): from the window picker it
    /// flips to the app grid and back, and from a closed overview it opens
    /// straight into the grid (`overviewControls.js:660-667`).
    ToggleApplicationView,
    /// GNOME's `toggle-message-tray` (`<Super>v` / `<Super>m`): the date menu —
    /// calendar and message list (`Panel.toggleCalendar`, `js/ui/panel.js:603`).
    ToggleMessageTray,
    /// GNOME's `toggle-quick-settings` (`<Super>s`): the quick settings menu
    /// (`Panel.toggleQuickSettings`, `js/ui/panel.js:607`).
    ToggleQuickSettings,
    /// The emoji picker (`<Control><Alt>space`). An addition, not a port: GNOME has no
    /// shell-level picker. See `docs/fork/emoji-picker.md`.
    ToggleEmojiPicker,
    ToggleScreenRecord,
    OpenOverview,
    CloseOverview,
    ShowRunDialog,
    Maximize,
    Unmaximize,
    ToggleTiledLeft,
    ToggleTiledRight,
    /// Step the brightness scales up / down / cyclically. The bool is GNOME's `-monitor`
    /// variant: act on the monitor under the pointer instead of the global scale.
    ScreenBrightnessUp(bool),
    ScreenBrightnessDown(bool),
    ScreenBrightnessCycle(bool),
    /// Notify a grabbed accelerator (org.gnome.Shell GrabAccelerator); the
    /// argument is the grab's action id. Internal, not bindable from config.
    ActivateAcceleratorGrab(u32),
    ToggleWindowUrgent(u64),
    SetWindowUrgent(u64),
    UnsetWindowUrgent(u64),
    /// GNOME's `switch-applications` — raise the app switcher, or advance it if it is already
    /// up. `backward` is the binding's `-backward` half (`windowManager.js:1705`).
    SwitchApplications {
        backward: bool,
    },
    /// GNOME's `switch-windows` — the per-*window* Alt-Tab switcher, a different popup class
    /// from the app switcher above (`windowManager.js:1670-1694`).
    SwitchWindows {
        backward: bool,
    },
    /// GNOME's `switch-group` — the *same* popup as `switch-applications`, opened inside the
    /// current app: the app row is pinned to item 0 and the window sub-list comes up with it
    /// (`AppSwitcherPopup._initialSelection`, `altTab.js:117-137`).
    SwitchGroup {
        backward: bool,
    },
    /// GNOME's `cycle-windows` (`<Alt>Escape`) — `WindowCyclerPopup` (`altTab.js:638-667`). Same
    /// window list as the window switcher, but **no popup**: the selected window is raised and
    /// framed in place.
    CycleWindows {
        backward: bool,
    },
    /// GNOME's `cycle-group` (`<Alt>F6`) — `GroupCyclerPopup` (`altTab.js:541-580`), the same
    /// listless cycler restricted to the focused app's windows.
    CycleGroup {
        backward: bool,
    },
}

impl From<synoik_ipc::Action> for Action {
    fn from(value: synoik_ipc::Action) -> Self {
        match value {
            synoik_ipc::Action::Quit { skip_confirmation } => Self::Quit(skip_confirmation),
            synoik_ipc::Action::PowerOffMonitors {} => Self::PowerOffMonitors,
            synoik_ipc::Action::Logout {} => Self::Logout,
            synoik_ipc::Action::PowerOff {} => Self::PowerOff,
            synoik_ipc::Action::Reboot {} => Self::Reboot,
            synoik_ipc::Action::PowerOnMonitors {} => Self::PowerOnMonitors,
            synoik_ipc::Action::Spawn { command } => Self::Spawn(command),
            synoik_ipc::Action::SpawnSh { command } => Self::SpawnSh(command),
            synoik_ipc::Action::DoScreenTransition { delay_ms } => {
                Self::DoScreenTransition(delay_ms)
            }
            synoik_ipc::Action::Screenshot { path } => Self::Screenshot(path),
            synoik_ipc::Action::ScreenshotScreen {
                write_to_disk,
                show_pointer,
                path,
            } => Self::ScreenshotScreen(write_to_disk, show_pointer, path),
            synoik_ipc::Action::ScreenshotWindow {
                id: None,
                write_to_disk,
                show_pointer,
                path,
            } => Self::ScreenshotWindow(write_to_disk, show_pointer, path),
            synoik_ipc::Action::ScreenshotWindow {
                id: Some(id),
                write_to_disk,
                show_pointer,
                path,
            } => Self::ScreenshotWindowById {
                id,
                write_to_disk,
                show_pointer,
                path,
            },
            synoik_ipc::Action::ToggleKeyboardShortcutsInhibit {} => {
                Self::ToggleKeyboardShortcutsInhibit
            }
            synoik_ipc::Action::CloseWindow { id: None } => Self::CloseWindow,
            synoik_ipc::Action::CloseWindow { id: Some(id) } => Self::CloseWindowById(id),
            synoik_ipc::Action::FullscreenWindow { id: None } => Self::FullscreenWindow,
            synoik_ipc::Action::FullscreenWindow { id: Some(id) } => Self::FullscreenWindowById(id),
            synoik_ipc::Action::ToggleWindowedFullscreen { id: None } => {
                Self::ToggleWindowedFullscreen
            }
            synoik_ipc::Action::ToggleWindowedFullscreen { id: Some(id) } => {
                Self::ToggleWindowedFullscreenById(id)
            }
            synoik_ipc::Action::FocusWindow { id } => Self::FocusWindow(id),
            synoik_ipc::Action::FocusWindowInColumn { index } => Self::FocusWindowInColumn(index),
            synoik_ipc::Action::FocusWindowPrevious {} => Self::FocusWindowPrevious,
            synoik_ipc::Action::FocusColumnLeft {} => Self::FocusColumnLeft,
            synoik_ipc::Action::FocusColumnRight {} => Self::FocusColumnRight,
            synoik_ipc::Action::FocusColumnFirst {} => Self::FocusColumnFirst,
            synoik_ipc::Action::FocusColumnLast {} => Self::FocusColumnLast,
            synoik_ipc::Action::FocusColumnRightOrFirst {} => Self::FocusColumnRightOrFirst,
            synoik_ipc::Action::FocusColumnLeftOrLast {} => Self::FocusColumnLeftOrLast,
            synoik_ipc::Action::FocusColumn { index } => Self::FocusColumn(index),
            synoik_ipc::Action::FocusWindowOrMonitorUp {} => Self::FocusWindowOrMonitorUp,
            synoik_ipc::Action::FocusWindowOrMonitorDown {} => Self::FocusWindowOrMonitorDown,
            synoik_ipc::Action::FocusColumnOrMonitorLeft {} => Self::FocusColumnOrMonitorLeft,
            synoik_ipc::Action::FocusColumnOrMonitorRight {} => Self::FocusColumnOrMonitorRight,
            synoik_ipc::Action::FocusWindowDown {} => Self::FocusWindowDown,
            synoik_ipc::Action::FocusWindowUp {} => Self::FocusWindowUp,
            synoik_ipc::Action::FocusWindowDownOrColumnLeft {} => Self::FocusWindowDownOrColumnLeft,
            synoik_ipc::Action::FocusWindowDownOrColumnRight {} => {
                Self::FocusWindowDownOrColumnRight
            }
            synoik_ipc::Action::FocusWindowUpOrColumnLeft {} => Self::FocusWindowUpOrColumnLeft,
            synoik_ipc::Action::FocusWindowUpOrColumnRight {} => Self::FocusWindowUpOrColumnRight,
            synoik_ipc::Action::FocusWindowOrWorkspaceDown {} => Self::FocusWindowOrWorkspaceDown,
            synoik_ipc::Action::FocusWindowOrWorkspaceUp {} => Self::FocusWindowOrWorkspaceUp,
            synoik_ipc::Action::FocusWindowTop {} => Self::FocusWindowTop,
            synoik_ipc::Action::FocusWindowBottom {} => Self::FocusWindowBottom,
            synoik_ipc::Action::FocusWindowDownOrTop {} => Self::FocusWindowDownOrTop,
            synoik_ipc::Action::FocusWindowUpOrBottom {} => Self::FocusWindowUpOrBottom,
            synoik_ipc::Action::MoveColumnLeft {} => Self::MoveColumnLeft,
            synoik_ipc::Action::MoveColumnRight {} => Self::MoveColumnRight,
            synoik_ipc::Action::MoveColumnToFirst {} => Self::MoveColumnToFirst,
            synoik_ipc::Action::MoveColumnToLast {} => Self::MoveColumnToLast,
            synoik_ipc::Action::MoveColumnToIndex { index } => Self::MoveColumnToIndex(index),
            synoik_ipc::Action::MoveColumnLeftOrToMonitorLeft {} => {
                Self::MoveColumnLeftOrToMonitorLeft
            }
            synoik_ipc::Action::MoveColumnRightOrToMonitorRight {} => {
                Self::MoveColumnRightOrToMonitorRight
            }
            synoik_ipc::Action::MoveWindowDown {} => Self::MoveWindowDown,
            synoik_ipc::Action::MoveWindowUp {} => Self::MoveWindowUp,
            synoik_ipc::Action::MoveWindowDownOrToWorkspaceDown {} => {
                Self::MoveWindowDownOrToWorkspaceDown
            }
            synoik_ipc::Action::MoveWindowUpOrToWorkspaceUp {} => Self::MoveWindowUpOrToWorkspaceUp,
            synoik_ipc::Action::ConsumeOrExpelWindowLeft { id: None } => {
                Self::ConsumeOrExpelWindowLeft
            }
            synoik_ipc::Action::ConsumeOrExpelWindowLeft { id: Some(id) } => {
                Self::ConsumeOrExpelWindowLeftById(id)
            }
            synoik_ipc::Action::ConsumeOrExpelWindowRight { id: None } => {
                Self::ConsumeOrExpelWindowRight
            }
            synoik_ipc::Action::ConsumeOrExpelWindowRight { id: Some(id) } => {
                Self::ConsumeOrExpelWindowRightById(id)
            }
            synoik_ipc::Action::ConsumeWindowIntoColumn {} => Self::ConsumeWindowIntoColumn,
            synoik_ipc::Action::ExpelWindowFromColumn {} => Self::ExpelWindowFromColumn,
            synoik_ipc::Action::SwapWindowRight {} => Self::SwapWindowRight,
            synoik_ipc::Action::SwapWindowLeft {} => Self::SwapWindowLeft,
            synoik_ipc::Action::ToggleColumnTabbedDisplay {} => Self::ToggleColumnTabbedDisplay,
            synoik_ipc::Action::SetColumnDisplay { display } => Self::SetColumnDisplay(display),
            synoik_ipc::Action::CenterColumn {} => Self::CenterColumn,
            synoik_ipc::Action::CenterWindow { id: None } => Self::CenterWindow,
            synoik_ipc::Action::CenterWindow { id: Some(id) } => Self::CenterWindowById(id),
            synoik_ipc::Action::CenterVisibleColumns {} => Self::CenterVisibleColumns,
            synoik_ipc::Action::FocusWorkspaceDown {} => Self::FocusWorkspaceDown,
            synoik_ipc::Action::FocusWorkspaceUp {} => Self::FocusWorkspaceUp,
            synoik_ipc::Action::FocusWorkspace { reference } => {
                Self::FocusWorkspace(WorkspaceReference::from(reference))
            }
            synoik_ipc::Action::FocusWorkspacePrevious {} => Self::FocusWorkspacePrevious,
            synoik_ipc::Action::MoveWindowToWorkspaceDown { focus } => {
                Self::MoveWindowToWorkspaceDown(focus)
            }
            synoik_ipc::Action::MoveWindowToWorkspaceUp { focus } => {
                Self::MoveWindowToWorkspaceUp(focus)
            }
            synoik_ipc::Action::MoveWindowToWorkspace {
                window_id: None,
                reference,
                focus,
            } => Self::MoveWindowToWorkspace(WorkspaceReference::from(reference), focus),
            synoik_ipc::Action::MoveWindowToWorkspace {
                window_id: Some(window_id),
                reference,
                focus,
            } => Self::MoveWindowToWorkspaceById {
                window_id,
                reference: WorkspaceReference::from(reference),
                focus,
            },
            synoik_ipc::Action::MoveColumnToWorkspaceDown { focus } => {
                Self::MoveColumnToWorkspaceDown(focus)
            }
            synoik_ipc::Action::MoveColumnToWorkspaceUp { focus } => {
                Self::MoveColumnToWorkspaceUp(focus)
            }
            synoik_ipc::Action::MoveColumnToWorkspace { reference, focus } => {
                Self::MoveColumnToWorkspace(WorkspaceReference::from(reference), focus)
            }
            synoik_ipc::Action::MoveWorkspaceDown {} => Self::MoveWorkspaceDown,
            synoik_ipc::Action::MoveWorkspaceUp {} => Self::MoveWorkspaceUp,
            synoik_ipc::Action::SetWorkspaceName {
                name,
                workspace: None,
            } => Self::SetWorkspaceName(name),
            synoik_ipc::Action::SetWorkspaceName {
                name,
                workspace: Some(reference),
            } => Self::SetWorkspaceNameByRef {
                name,
                reference: WorkspaceReference::from(reference),
            },
            synoik_ipc::Action::UnsetWorkspaceName { reference: None } => Self::UnsetWorkspaceName,
            synoik_ipc::Action::UnsetWorkspaceName {
                reference: Some(reference),
            } => Self::UnsetWorkSpaceNameByRef(WorkspaceReference::from(reference)),
            synoik_ipc::Action::FocusMonitorLeft {} => Self::FocusMonitorLeft,
            synoik_ipc::Action::FocusMonitorRight {} => Self::FocusMonitorRight,
            synoik_ipc::Action::FocusMonitorDown {} => Self::FocusMonitorDown,
            synoik_ipc::Action::FocusMonitorUp {} => Self::FocusMonitorUp,
            synoik_ipc::Action::FocusMonitorPrevious {} => Self::FocusMonitorPrevious,
            synoik_ipc::Action::FocusMonitorNext {} => Self::FocusMonitorNext,
            synoik_ipc::Action::FocusMonitor { output } => Self::FocusMonitor(output),
            synoik_ipc::Action::MoveWindowToMonitorLeft {} => Self::MoveWindowToMonitorLeft,
            synoik_ipc::Action::MoveWindowToMonitorRight {} => Self::MoveWindowToMonitorRight,
            synoik_ipc::Action::MoveWindowToMonitorDown {} => Self::MoveWindowToMonitorDown,
            synoik_ipc::Action::MoveWindowToMonitorUp {} => Self::MoveWindowToMonitorUp,
            synoik_ipc::Action::MoveWindowToMonitorPrevious {} => Self::MoveWindowToMonitorPrevious,
            synoik_ipc::Action::MoveWindowToMonitorNext {} => Self::MoveWindowToMonitorNext,
            synoik_ipc::Action::MoveWindowToMonitor { id: None, output } => {
                Self::MoveWindowToMonitor(output)
            }
            synoik_ipc::Action::MoveWindowToMonitor {
                id: Some(id),
                output,
            } => Self::MoveWindowToMonitorById { id, output },
            synoik_ipc::Action::MoveColumnToMonitorLeft {} => Self::MoveColumnToMonitorLeft,
            synoik_ipc::Action::MoveColumnToMonitorRight {} => Self::MoveColumnToMonitorRight,
            synoik_ipc::Action::MoveColumnToMonitorDown {} => Self::MoveColumnToMonitorDown,
            synoik_ipc::Action::MoveColumnToMonitorUp {} => Self::MoveColumnToMonitorUp,
            synoik_ipc::Action::MoveColumnToMonitorPrevious {} => Self::MoveColumnToMonitorPrevious,
            synoik_ipc::Action::MoveColumnToMonitorNext {} => Self::MoveColumnToMonitorNext,
            synoik_ipc::Action::MoveColumnToMonitor { output } => Self::MoveColumnToMonitor(output),
            synoik_ipc::Action::SetWindowWidth { id: None, change } => Self::SetWindowWidth(change),
            synoik_ipc::Action::SetWindowWidth {
                id: Some(id),
                change,
            } => Self::SetWindowWidthById { id, change },
            synoik_ipc::Action::SetWindowHeight { id: None, change } => {
                Self::SetWindowHeight(change)
            }
            synoik_ipc::Action::SetWindowHeight {
                id: Some(id),
                change,
            } => Self::SetWindowHeightById { id, change },
            synoik_ipc::Action::ResetWindowHeight { id: None } => Self::ResetWindowHeight,
            synoik_ipc::Action::ResetWindowHeight { id: Some(id) } => {
                Self::ResetWindowHeightById(id)
            }
            synoik_ipc::Action::SwitchPresetColumnWidth {} => Self::SwitchPresetColumnWidth,
            synoik_ipc::Action::SwitchPresetColumnWidthBack {} => Self::SwitchPresetColumnWidthBack,
            synoik_ipc::Action::SwitchPresetWindowWidth { id: None } => {
                Self::SwitchPresetWindowWidth
            }
            synoik_ipc::Action::SwitchPresetWindowWidthBack { id: None } => {
                Self::SwitchPresetWindowWidthBack
            }
            synoik_ipc::Action::SwitchPresetWindowWidth { id: Some(id) } => {
                Self::SwitchPresetWindowWidthById(id)
            }
            synoik_ipc::Action::SwitchPresetWindowWidthBack { id: Some(id) } => {
                Self::SwitchPresetWindowWidthBackById(id)
            }
            synoik_ipc::Action::SwitchPresetWindowHeight { id: None } => {
                Self::SwitchPresetWindowHeight
            }
            synoik_ipc::Action::SwitchPresetWindowHeightBack { id: None } => {
                Self::SwitchPresetWindowHeightBack
            }
            synoik_ipc::Action::SwitchPresetWindowHeight { id: Some(id) } => {
                Self::SwitchPresetWindowHeightById(id)
            }
            synoik_ipc::Action::SwitchPresetWindowHeightBack { id: Some(id) } => {
                Self::SwitchPresetWindowHeightBackById(id)
            }
            synoik_ipc::Action::MaximizeColumn {} => Self::MaximizeColumn,
            synoik_ipc::Action::MaximizeWindowToEdges { id: None } => Self::MaximizeWindowToEdges,
            synoik_ipc::Action::MaximizeWindowToEdges { id: Some(id) } => {
                Self::MaximizeWindowToEdgesById(id)
            }
            synoik_ipc::Action::SetColumnWidth { change } => Self::SetColumnWidth(change),
            synoik_ipc::Action::ExpandColumnToAvailableWidth {} => {
                Self::ExpandColumnToAvailableWidth
            }
            synoik_ipc::Action::SwitchLayout { layout } => Self::SwitchLayout(layout),
            synoik_ipc::Action::ShowHotkeyOverlay {} => Self::ShowHotkeyOverlay,
            synoik_ipc::Action::MoveWorkspaceToMonitorLeft {} => Self::MoveWorkspaceToMonitorLeft,
            synoik_ipc::Action::MoveWorkspaceToMonitorRight {} => Self::MoveWorkspaceToMonitorRight,
            synoik_ipc::Action::MoveWorkspaceToMonitorDown {} => Self::MoveWorkspaceToMonitorDown,
            synoik_ipc::Action::MoveWorkspaceToMonitorUp {} => Self::MoveWorkspaceToMonitorUp,
            synoik_ipc::Action::MoveWorkspaceToMonitorPrevious {} => {
                Self::MoveWorkspaceToMonitorPrevious
            }
            synoik_ipc::Action::MoveWorkspaceToIndex {
                index,
                reference: Some(reference),
            } => Self::MoveWorkspaceToIndexByRef {
                new_idx: index,
                reference: WorkspaceReference::from(reference),
            },
            synoik_ipc::Action::MoveWorkspaceToIndex {
                index,
                reference: None,
            } => Self::MoveWorkspaceToIndex(index),
            synoik_ipc::Action::MoveWorkspaceToMonitor {
                output,
                reference: Some(reference),
            } => Self::MoveWorkspaceToMonitorByRef {
                output_name: output,
                reference: WorkspaceReference::from(reference),
            },
            synoik_ipc::Action::MoveWorkspaceToMonitor {
                output,
                reference: None,
            } => Self::MoveWorkspaceToMonitor(output),
            synoik_ipc::Action::MoveWorkspaceToMonitorNext {} => Self::MoveWorkspaceToMonitorNext,
            synoik_ipc::Action::ToggleDebugTint {} => Self::ToggleDebugTint,
            synoik_ipc::Action::DebugToggleOpaqueRegions {} => Self::DebugToggleOpaqueRegions,
            synoik_ipc::Action::DebugToggleDamage { age } => Self::DebugToggleDamage(age),
            synoik_ipc::Action::DebugDumpScanout {} => Self::DebugDumpScanout,
            synoik_ipc::Action::DebugToggleDeadlineDispatch {} => Self::DebugToggleDeadlineDispatch,
            synoik_ipc::Action::DebugSetRenderTimeMargin { millis } => {
                Self::DebugSetRenderTimeMargin(millis)
            }
            synoik_ipc::Action::DebugSetBattery {
                percentage,
                state,
                warning,
            } => Self::DebugSetBattery(percentage, state, warning),
            synoik_ipc::Action::DebugToggleOutput { connector } => {
                Self::DebugToggleOutput(connector)
            }
            synoik_ipc::Action::DebugInsertText { text } => Self::DebugInsertText(text),
            synoik_ipc::Action::ToggleWindowFloating { id: None } => Self::ToggleWindowFloating,
            synoik_ipc::Action::ToggleWindowFloating { id: Some(id) } => {
                Self::ToggleWindowFloatingById(id)
            }
            synoik_ipc::Action::MoveWindowToFloating { id: None } => Self::MoveWindowToFloating,
            synoik_ipc::Action::MoveWindowToFloating { id: Some(id) } => {
                Self::MoveWindowToFloatingById(id)
            }
            synoik_ipc::Action::MoveWindowToTiling { id: None } => Self::MoveWindowToTiling,
            synoik_ipc::Action::MoveWindowToTiling { id: Some(id) } => {
                Self::MoveWindowToTilingById(id)
            }
            synoik_ipc::Action::FocusFloating {} => Self::FocusFloating,
            synoik_ipc::Action::FocusTiling {} => Self::FocusTiling,
            synoik_ipc::Action::SwitchFocusBetweenFloatingAndTiling {} => {
                Self::SwitchFocusBetweenFloatingAndTiling
            }
            synoik_ipc::Action::MoveFloatingWindow { id, x, y } => {
                Self::MoveFloatingWindowById { id, x, y }
            }
            synoik_ipc::Action::ToggleWindowRuleOpacity { id: None } => {
                Self::ToggleWindowRuleOpacity
            }
            synoik_ipc::Action::ToggleWindowRuleOpacity { id: Some(id) } => {
                Self::ToggleWindowRuleOpacityById(id)
            }
            synoik_ipc::Action::SetDynamicCastWindow { id: None } => Self::SetDynamicCastWindow,
            synoik_ipc::Action::SetDynamicCastWindow { id: Some(id) } => {
                Self::SetDynamicCastWindowById(id)
            }
            synoik_ipc::Action::SetDynamicCastMonitor { output } => {
                Self::SetDynamicCastMonitor(output)
            }
            synoik_ipc::Action::ClearDynamicCastTarget {} => Self::ClearDynamicCastTarget,
            synoik_ipc::Action::StopCast { session_id } => Self::StopCast(session_id),
            synoik_ipc::Action::ToggleOverview {} => Self::ToggleOverview,
            synoik_ipc::Action::ToggleScreenRecord {} => Self::ToggleScreenRecord,
            synoik_ipc::Action::OpenOverview {} => Self::OpenOverview,
            synoik_ipc::Action::CloseOverview {} => Self::CloseOverview,
            synoik_ipc::Action::ShowRunDialog {} => Self::ShowRunDialog,
            synoik_ipc::Action::Maximize {} => Self::Maximize,
            synoik_ipc::Action::Unmaximize {} => Self::Unmaximize,
            synoik_ipc::Action::ToggleTiledLeft {} => Self::ToggleTiledLeft,
            synoik_ipc::Action::ToggleTiledRight {} => Self::ToggleTiledRight,
            synoik_ipc::Action::ScreenBrightnessUp { current_monitor } => {
                Self::ScreenBrightnessUp(current_monitor)
            }
            synoik_ipc::Action::ScreenBrightnessDown { current_monitor } => {
                Self::ScreenBrightnessDown(current_monitor)
            }
            synoik_ipc::Action::ScreenBrightnessCycle { current_monitor } => {
                Self::ScreenBrightnessCycle(current_monitor)
            }
            synoik_ipc::Action::ToggleWindowUrgent { id } => Self::ToggleWindowUrgent(id),
            synoik_ipc::Action::SetWindowUrgent { id } => Self::SetWindowUrgent(id),
            synoik_ipc::Action::UnsetWindowUrgent { id } => Self::UnsetWindowUrgent(id),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum WorkspaceReference {
    Id(u64),
    Index(u8),
    Name(String),
}

impl From<WorkspaceReferenceArg> for WorkspaceReference {
    fn from(reference: WorkspaceReferenceArg) -> WorkspaceReference {
        match reference {
            WorkspaceReferenceArg::Id(id) => Self::Id(id),
            WorkspaceReferenceArg::Index(i) => Self::Index(i),
            WorkspaceReferenceArg::Name(n) => Self::Name(n),
        }
    }
}
