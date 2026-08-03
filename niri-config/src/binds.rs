use bitflags::bitflags;
use niri_ipc::{
    ColumnDisplay, LayoutSwitchTarget, PositionChange, SizeChange, WorkspaceReferenceArg,
};
use smithay::input::keyboard::Keysym;

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
    DebugToggleDamage,
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
    Screenshot(
        bool,
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
    FocusWorkspaceDownUnderMouse,
    FocusWorkspaceUp,
    FocusWorkspaceUpUnderMouse,
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

impl From<niri_ipc::Action> for Action {
    fn from(value: niri_ipc::Action) -> Self {
        match value {
            niri_ipc::Action::Quit { skip_confirmation } => Self::Quit(skip_confirmation),
            niri_ipc::Action::PowerOffMonitors {} => Self::PowerOffMonitors,
            niri_ipc::Action::Logout {} => Self::Logout,
            niri_ipc::Action::PowerOff {} => Self::PowerOff,
            niri_ipc::Action::Reboot {} => Self::Reboot,
            niri_ipc::Action::PowerOnMonitors {} => Self::PowerOnMonitors,
            niri_ipc::Action::Spawn { command } => Self::Spawn(command),
            niri_ipc::Action::SpawnSh { command } => Self::SpawnSh(command),
            niri_ipc::Action::DoScreenTransition { delay_ms } => Self::DoScreenTransition(delay_ms),
            niri_ipc::Action::Screenshot { show_pointer, path } => {
                Self::Screenshot(show_pointer, path)
            }
            niri_ipc::Action::ScreenshotScreen {
                write_to_disk,
                show_pointer,
                path,
            } => Self::ScreenshotScreen(write_to_disk, show_pointer, path),
            niri_ipc::Action::ScreenshotWindow {
                id: None,
                write_to_disk,
                show_pointer,
                path,
            } => Self::ScreenshotWindow(write_to_disk, show_pointer, path),
            niri_ipc::Action::ScreenshotWindow {
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
            niri_ipc::Action::ToggleKeyboardShortcutsInhibit {} => {
                Self::ToggleKeyboardShortcutsInhibit
            }
            niri_ipc::Action::CloseWindow { id: None } => Self::CloseWindow,
            niri_ipc::Action::CloseWindow { id: Some(id) } => Self::CloseWindowById(id),
            niri_ipc::Action::FullscreenWindow { id: None } => Self::FullscreenWindow,
            niri_ipc::Action::FullscreenWindow { id: Some(id) } => Self::FullscreenWindowById(id),
            niri_ipc::Action::ToggleWindowedFullscreen { id: None } => {
                Self::ToggleWindowedFullscreen
            }
            niri_ipc::Action::ToggleWindowedFullscreen { id: Some(id) } => {
                Self::ToggleWindowedFullscreenById(id)
            }
            niri_ipc::Action::FocusWindow { id } => Self::FocusWindow(id),
            niri_ipc::Action::FocusWindowInColumn { index } => Self::FocusWindowInColumn(index),
            niri_ipc::Action::FocusWindowPrevious {} => Self::FocusWindowPrevious,
            niri_ipc::Action::FocusColumnLeft {} => Self::FocusColumnLeft,
            niri_ipc::Action::FocusColumnRight {} => Self::FocusColumnRight,
            niri_ipc::Action::FocusColumnFirst {} => Self::FocusColumnFirst,
            niri_ipc::Action::FocusColumnLast {} => Self::FocusColumnLast,
            niri_ipc::Action::FocusColumnRightOrFirst {} => Self::FocusColumnRightOrFirst,
            niri_ipc::Action::FocusColumnLeftOrLast {} => Self::FocusColumnLeftOrLast,
            niri_ipc::Action::FocusColumn { index } => Self::FocusColumn(index),
            niri_ipc::Action::FocusWindowOrMonitorUp {} => Self::FocusWindowOrMonitorUp,
            niri_ipc::Action::FocusWindowOrMonitorDown {} => Self::FocusWindowOrMonitorDown,
            niri_ipc::Action::FocusColumnOrMonitorLeft {} => Self::FocusColumnOrMonitorLeft,
            niri_ipc::Action::FocusColumnOrMonitorRight {} => Self::FocusColumnOrMonitorRight,
            niri_ipc::Action::FocusWindowDown {} => Self::FocusWindowDown,
            niri_ipc::Action::FocusWindowUp {} => Self::FocusWindowUp,
            niri_ipc::Action::FocusWindowDownOrColumnLeft {} => Self::FocusWindowDownOrColumnLeft,
            niri_ipc::Action::FocusWindowDownOrColumnRight {} => Self::FocusWindowDownOrColumnRight,
            niri_ipc::Action::FocusWindowUpOrColumnLeft {} => Self::FocusWindowUpOrColumnLeft,
            niri_ipc::Action::FocusWindowUpOrColumnRight {} => Self::FocusWindowUpOrColumnRight,
            niri_ipc::Action::FocusWindowOrWorkspaceDown {} => Self::FocusWindowOrWorkspaceDown,
            niri_ipc::Action::FocusWindowOrWorkspaceUp {} => Self::FocusWindowOrWorkspaceUp,
            niri_ipc::Action::FocusWindowTop {} => Self::FocusWindowTop,
            niri_ipc::Action::FocusWindowBottom {} => Self::FocusWindowBottom,
            niri_ipc::Action::FocusWindowDownOrTop {} => Self::FocusWindowDownOrTop,
            niri_ipc::Action::FocusWindowUpOrBottom {} => Self::FocusWindowUpOrBottom,
            niri_ipc::Action::MoveColumnLeft {} => Self::MoveColumnLeft,
            niri_ipc::Action::MoveColumnRight {} => Self::MoveColumnRight,
            niri_ipc::Action::MoveColumnToFirst {} => Self::MoveColumnToFirst,
            niri_ipc::Action::MoveColumnToLast {} => Self::MoveColumnToLast,
            niri_ipc::Action::MoveColumnToIndex { index } => Self::MoveColumnToIndex(index),
            niri_ipc::Action::MoveColumnLeftOrToMonitorLeft {} => {
                Self::MoveColumnLeftOrToMonitorLeft
            }
            niri_ipc::Action::MoveColumnRightOrToMonitorRight {} => {
                Self::MoveColumnRightOrToMonitorRight
            }
            niri_ipc::Action::MoveWindowDown {} => Self::MoveWindowDown,
            niri_ipc::Action::MoveWindowUp {} => Self::MoveWindowUp,
            niri_ipc::Action::MoveWindowDownOrToWorkspaceDown {} => {
                Self::MoveWindowDownOrToWorkspaceDown
            }
            niri_ipc::Action::MoveWindowUpOrToWorkspaceUp {} => Self::MoveWindowUpOrToWorkspaceUp,
            niri_ipc::Action::ConsumeOrExpelWindowLeft { id: None } => {
                Self::ConsumeOrExpelWindowLeft
            }
            niri_ipc::Action::ConsumeOrExpelWindowLeft { id: Some(id) } => {
                Self::ConsumeOrExpelWindowLeftById(id)
            }
            niri_ipc::Action::ConsumeOrExpelWindowRight { id: None } => {
                Self::ConsumeOrExpelWindowRight
            }
            niri_ipc::Action::ConsumeOrExpelWindowRight { id: Some(id) } => {
                Self::ConsumeOrExpelWindowRightById(id)
            }
            niri_ipc::Action::ConsumeWindowIntoColumn {} => Self::ConsumeWindowIntoColumn,
            niri_ipc::Action::ExpelWindowFromColumn {} => Self::ExpelWindowFromColumn,
            niri_ipc::Action::SwapWindowRight {} => Self::SwapWindowRight,
            niri_ipc::Action::SwapWindowLeft {} => Self::SwapWindowLeft,
            niri_ipc::Action::ToggleColumnTabbedDisplay {} => Self::ToggleColumnTabbedDisplay,
            niri_ipc::Action::SetColumnDisplay { display } => Self::SetColumnDisplay(display),
            niri_ipc::Action::CenterColumn {} => Self::CenterColumn,
            niri_ipc::Action::CenterWindow { id: None } => Self::CenterWindow,
            niri_ipc::Action::CenterWindow { id: Some(id) } => Self::CenterWindowById(id),
            niri_ipc::Action::CenterVisibleColumns {} => Self::CenterVisibleColumns,
            niri_ipc::Action::FocusWorkspaceDown {} => Self::FocusWorkspaceDown,
            niri_ipc::Action::FocusWorkspaceUp {} => Self::FocusWorkspaceUp,
            niri_ipc::Action::FocusWorkspace { reference } => {
                Self::FocusWorkspace(WorkspaceReference::from(reference))
            }
            niri_ipc::Action::FocusWorkspacePrevious {} => Self::FocusWorkspacePrevious,
            niri_ipc::Action::MoveWindowToWorkspaceDown { focus } => {
                Self::MoveWindowToWorkspaceDown(focus)
            }
            niri_ipc::Action::MoveWindowToWorkspaceUp { focus } => {
                Self::MoveWindowToWorkspaceUp(focus)
            }
            niri_ipc::Action::MoveWindowToWorkspace {
                window_id: None,
                reference,
                focus,
            } => Self::MoveWindowToWorkspace(WorkspaceReference::from(reference), focus),
            niri_ipc::Action::MoveWindowToWorkspace {
                window_id: Some(window_id),
                reference,
                focus,
            } => Self::MoveWindowToWorkspaceById {
                window_id,
                reference: WorkspaceReference::from(reference),
                focus,
            },
            niri_ipc::Action::MoveColumnToWorkspaceDown { focus } => {
                Self::MoveColumnToWorkspaceDown(focus)
            }
            niri_ipc::Action::MoveColumnToWorkspaceUp { focus } => {
                Self::MoveColumnToWorkspaceUp(focus)
            }
            niri_ipc::Action::MoveColumnToWorkspace { reference, focus } => {
                Self::MoveColumnToWorkspace(WorkspaceReference::from(reference), focus)
            }
            niri_ipc::Action::MoveWorkspaceDown {} => Self::MoveWorkspaceDown,
            niri_ipc::Action::MoveWorkspaceUp {} => Self::MoveWorkspaceUp,
            niri_ipc::Action::SetWorkspaceName {
                name,
                workspace: None,
            } => Self::SetWorkspaceName(name),
            niri_ipc::Action::SetWorkspaceName {
                name,
                workspace: Some(reference),
            } => Self::SetWorkspaceNameByRef {
                name,
                reference: WorkspaceReference::from(reference),
            },
            niri_ipc::Action::UnsetWorkspaceName { reference: None } => Self::UnsetWorkspaceName,
            niri_ipc::Action::UnsetWorkspaceName {
                reference: Some(reference),
            } => Self::UnsetWorkSpaceNameByRef(WorkspaceReference::from(reference)),
            niri_ipc::Action::FocusMonitorLeft {} => Self::FocusMonitorLeft,
            niri_ipc::Action::FocusMonitorRight {} => Self::FocusMonitorRight,
            niri_ipc::Action::FocusMonitorDown {} => Self::FocusMonitorDown,
            niri_ipc::Action::FocusMonitorUp {} => Self::FocusMonitorUp,
            niri_ipc::Action::FocusMonitorPrevious {} => Self::FocusMonitorPrevious,
            niri_ipc::Action::FocusMonitorNext {} => Self::FocusMonitorNext,
            niri_ipc::Action::FocusMonitor { output } => Self::FocusMonitor(output),
            niri_ipc::Action::MoveWindowToMonitorLeft {} => Self::MoveWindowToMonitorLeft,
            niri_ipc::Action::MoveWindowToMonitorRight {} => Self::MoveWindowToMonitorRight,
            niri_ipc::Action::MoveWindowToMonitorDown {} => Self::MoveWindowToMonitorDown,
            niri_ipc::Action::MoveWindowToMonitorUp {} => Self::MoveWindowToMonitorUp,
            niri_ipc::Action::MoveWindowToMonitorPrevious {} => Self::MoveWindowToMonitorPrevious,
            niri_ipc::Action::MoveWindowToMonitorNext {} => Self::MoveWindowToMonitorNext,
            niri_ipc::Action::MoveWindowToMonitor { id: None, output } => {
                Self::MoveWindowToMonitor(output)
            }
            niri_ipc::Action::MoveWindowToMonitor {
                id: Some(id),
                output,
            } => Self::MoveWindowToMonitorById { id, output },
            niri_ipc::Action::MoveColumnToMonitorLeft {} => Self::MoveColumnToMonitorLeft,
            niri_ipc::Action::MoveColumnToMonitorRight {} => Self::MoveColumnToMonitorRight,
            niri_ipc::Action::MoveColumnToMonitorDown {} => Self::MoveColumnToMonitorDown,
            niri_ipc::Action::MoveColumnToMonitorUp {} => Self::MoveColumnToMonitorUp,
            niri_ipc::Action::MoveColumnToMonitorPrevious {} => Self::MoveColumnToMonitorPrevious,
            niri_ipc::Action::MoveColumnToMonitorNext {} => Self::MoveColumnToMonitorNext,
            niri_ipc::Action::MoveColumnToMonitor { output } => Self::MoveColumnToMonitor(output),
            niri_ipc::Action::SetWindowWidth { id: None, change } => Self::SetWindowWidth(change),
            niri_ipc::Action::SetWindowWidth {
                id: Some(id),
                change,
            } => Self::SetWindowWidthById { id, change },
            niri_ipc::Action::SetWindowHeight { id: None, change } => Self::SetWindowHeight(change),
            niri_ipc::Action::SetWindowHeight {
                id: Some(id),
                change,
            } => Self::SetWindowHeightById { id, change },
            niri_ipc::Action::ResetWindowHeight { id: None } => Self::ResetWindowHeight,
            niri_ipc::Action::ResetWindowHeight { id: Some(id) } => Self::ResetWindowHeightById(id),
            niri_ipc::Action::SwitchPresetColumnWidth {} => Self::SwitchPresetColumnWidth,
            niri_ipc::Action::SwitchPresetColumnWidthBack {} => Self::SwitchPresetColumnWidthBack,
            niri_ipc::Action::SwitchPresetWindowWidth { id: None } => Self::SwitchPresetWindowWidth,
            niri_ipc::Action::SwitchPresetWindowWidthBack { id: None } => {
                Self::SwitchPresetWindowWidthBack
            }
            niri_ipc::Action::SwitchPresetWindowWidth { id: Some(id) } => {
                Self::SwitchPresetWindowWidthById(id)
            }
            niri_ipc::Action::SwitchPresetWindowWidthBack { id: Some(id) } => {
                Self::SwitchPresetWindowWidthBackById(id)
            }
            niri_ipc::Action::SwitchPresetWindowHeight { id: None } => {
                Self::SwitchPresetWindowHeight
            }
            niri_ipc::Action::SwitchPresetWindowHeightBack { id: None } => {
                Self::SwitchPresetWindowHeightBack
            }
            niri_ipc::Action::SwitchPresetWindowHeight { id: Some(id) } => {
                Self::SwitchPresetWindowHeightById(id)
            }
            niri_ipc::Action::SwitchPresetWindowHeightBack { id: Some(id) } => {
                Self::SwitchPresetWindowHeightBackById(id)
            }
            niri_ipc::Action::MaximizeColumn {} => Self::MaximizeColumn,
            niri_ipc::Action::MaximizeWindowToEdges { id: None } => Self::MaximizeWindowToEdges,
            niri_ipc::Action::MaximizeWindowToEdges { id: Some(id) } => {
                Self::MaximizeWindowToEdgesById(id)
            }
            niri_ipc::Action::SetColumnWidth { change } => Self::SetColumnWidth(change),
            niri_ipc::Action::ExpandColumnToAvailableWidth {} => Self::ExpandColumnToAvailableWidth,
            niri_ipc::Action::SwitchLayout { layout } => Self::SwitchLayout(layout),
            niri_ipc::Action::ShowHotkeyOverlay {} => Self::ShowHotkeyOverlay,
            niri_ipc::Action::MoveWorkspaceToMonitorLeft {} => Self::MoveWorkspaceToMonitorLeft,
            niri_ipc::Action::MoveWorkspaceToMonitorRight {} => Self::MoveWorkspaceToMonitorRight,
            niri_ipc::Action::MoveWorkspaceToMonitorDown {} => Self::MoveWorkspaceToMonitorDown,
            niri_ipc::Action::MoveWorkspaceToMonitorUp {} => Self::MoveWorkspaceToMonitorUp,
            niri_ipc::Action::MoveWorkspaceToMonitorPrevious {} => {
                Self::MoveWorkspaceToMonitorPrevious
            }
            niri_ipc::Action::MoveWorkspaceToIndex {
                index,
                reference: Some(reference),
            } => Self::MoveWorkspaceToIndexByRef {
                new_idx: index,
                reference: WorkspaceReference::from(reference),
            },
            niri_ipc::Action::MoveWorkspaceToIndex {
                index,
                reference: None,
            } => Self::MoveWorkspaceToIndex(index),
            niri_ipc::Action::MoveWorkspaceToMonitor {
                output,
                reference: Some(reference),
            } => Self::MoveWorkspaceToMonitorByRef {
                output_name: output,
                reference: WorkspaceReference::from(reference),
            },
            niri_ipc::Action::MoveWorkspaceToMonitor {
                output,
                reference: None,
            } => Self::MoveWorkspaceToMonitor(output),
            niri_ipc::Action::MoveWorkspaceToMonitorNext {} => Self::MoveWorkspaceToMonitorNext,
            niri_ipc::Action::ToggleDebugTint {} => Self::ToggleDebugTint,
            niri_ipc::Action::DebugToggleOpaqueRegions {} => Self::DebugToggleOpaqueRegions,
            niri_ipc::Action::DebugToggleDamage {} => Self::DebugToggleDamage,
            niri_ipc::Action::ToggleWindowFloating { id: None } => Self::ToggleWindowFloating,
            niri_ipc::Action::ToggleWindowFloating { id: Some(id) } => {
                Self::ToggleWindowFloatingById(id)
            }
            niri_ipc::Action::MoveWindowToFloating { id: None } => Self::MoveWindowToFloating,
            niri_ipc::Action::MoveWindowToFloating { id: Some(id) } => {
                Self::MoveWindowToFloatingById(id)
            }
            niri_ipc::Action::MoveWindowToTiling { id: None } => Self::MoveWindowToTiling,
            niri_ipc::Action::MoveWindowToTiling { id: Some(id) } => {
                Self::MoveWindowToTilingById(id)
            }
            niri_ipc::Action::FocusFloating {} => Self::FocusFloating,
            niri_ipc::Action::FocusTiling {} => Self::FocusTiling,
            niri_ipc::Action::SwitchFocusBetweenFloatingAndTiling {} => {
                Self::SwitchFocusBetweenFloatingAndTiling
            }
            niri_ipc::Action::MoveFloatingWindow { id, x, y } => {
                Self::MoveFloatingWindowById { id, x, y }
            }
            niri_ipc::Action::ToggleWindowRuleOpacity { id: None } => Self::ToggleWindowRuleOpacity,
            niri_ipc::Action::ToggleWindowRuleOpacity { id: Some(id) } => {
                Self::ToggleWindowRuleOpacityById(id)
            }
            niri_ipc::Action::SetDynamicCastWindow { id: None } => Self::SetDynamicCastWindow,
            niri_ipc::Action::SetDynamicCastWindow { id: Some(id) } => {
                Self::SetDynamicCastWindowById(id)
            }
            niri_ipc::Action::SetDynamicCastMonitor { output } => {
                Self::SetDynamicCastMonitor(output)
            }
            niri_ipc::Action::ClearDynamicCastTarget {} => Self::ClearDynamicCastTarget,
            niri_ipc::Action::StopCast { session_id } => Self::StopCast(session_id),
            niri_ipc::Action::ToggleOverview {} => Self::ToggleOverview,
            niri_ipc::Action::ToggleScreenRecord {} => Self::ToggleScreenRecord,
            niri_ipc::Action::OpenOverview {} => Self::OpenOverview,
            niri_ipc::Action::CloseOverview {} => Self::CloseOverview,
            niri_ipc::Action::ShowRunDialog {} => Self::ShowRunDialog,
            niri_ipc::Action::Maximize {} => Self::Maximize,
            niri_ipc::Action::Unmaximize {} => Self::Unmaximize,
            niri_ipc::Action::ToggleTiledLeft {} => Self::ToggleTiledLeft,
            niri_ipc::Action::ToggleTiledRight {} => Self::ToggleTiledRight,
            niri_ipc::Action::ScreenBrightnessUp { current_monitor } => {
                Self::ScreenBrightnessUp(current_monitor)
            }
            niri_ipc::Action::ScreenBrightnessDown { current_monitor } => {
                Self::ScreenBrightnessDown(current_monitor)
            }
            niri_ipc::Action::ScreenBrightnessCycle { current_monitor } => {
                Self::ScreenBrightnessCycle(current_monitor)
            }
            niri_ipc::Action::ToggleWindowUrgent { id } => Self::ToggleWindowUrgent(id),
            niri_ipc::Action::SetWindowUrgent { id } => Self::SetWindowUrgent(id),
            niri_ipc::Action::UnsetWindowUrgent { id } => Self::UnsetWindowUrgent(id),
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
