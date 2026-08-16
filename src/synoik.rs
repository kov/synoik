// SPDX-License-Identifier: GPL-3.0-only
//
// Copyright (C) 2026 Gustavo Noronha Silva <gustavo@noronha.dev.br>
//
// Based on niri, copyright Ivan Molodetskikh and the niri contributors,
// distributed under the GNU General Public License version 3 or later.
// Modified for synoik in 2026.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{env, mem, thread};

use _server_decoration::server::org_kde_kwin_server_decoration_manager::Mode as KdeDecorationsMode;
use anyhow::{bail, ensure, Context};
use calloop::futures::Scheduler;
use smithay::backend::allocator::Fourcc;
use smithay::backend::input::Keycode;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::memory::MemoryRenderBufferRenderElement;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::utils::{
    select_dmabuf_feedback, CropRenderElement, Relocate, RelocateRenderElement,
    RescaleRenderElement,
};
use smithay::backend::renderer::element::{
    default_primary_scanout_output_compare, Element, Id, Kind, PrimaryScanoutOutput, RenderElement,
    RenderElementStates,
};
use smithay::backend::renderer::sync::SyncPoint;
use smithay::backend::renderer::Color32F;
use smithay::desktop::utils::{
    bbox_from_surface_tree, output_update, send_dmabuf_feedback_surface_tree,
    send_frames_surface_tree, surface_presentation_feedback_flags_from_states,
    surface_primary_scanout_output, take_presentation_feedback_surface_tree,
    under_from_surface_tree, update_surface_primary_scanout_output, with_surfaces_surface_tree,
    OutputPresentationFeedback,
};
use smithay::desktop::{
    find_popup_root_surface, layer_map_for_output, LayerMap, LayerSurface, PopupGrab, PopupManager,
    PopupUngrabStrategy, Space, Window, WindowSurfaceType,
};
use smithay::input::keyboard::{Layout as KeyboardLayout, XkbConfig};
use smithay::input::pointer::{
    CursorIcon, CursorImageStatus, CursorImageSurfaceData, Focus,
    GrabStartData as PointerGrabStartData, MotionEvent,
};
use smithay::input::{Seat, SeatState};
use smithay::output::{self, Output, OutputModeSource, PhysicalProperties, Subpixel, WeakOutput};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{
    Interest, LoopHandle, LoopSignal, Mode, PostAction, RegistrationToken,
};
use smithay::reexports::wayland_protocols::ext::session_lock::v1::server::ext_session_lock_v1::ExtSessionLockV1;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::WmCapabilities;
use smithay::reexports::wayland_protocols_misc::server_decoration as _server_decoration;
use smithay::reexports::wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;
use smithay::reexports::wayland_server::backend::{
    ClientData, ClientId, DisconnectReason, GlobalId,
};
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Display, DisplayHandle, Resource};
use smithay::utils::{
    ClockSource, IsAlive as _, Logical, Monotonic, Physical, Point, Rectangle, Scale, Size,
    Transform, SERIAL_COUNTER,
};
use smithay::wayland::background_effect::BackgroundEffectState;
use smithay::wayland::compositor::{
    with_states, with_surface_tree_downward, CompositorClientState, CompositorHandler,
    CompositorState, HookId, SurfaceData, TraversalAction,
};
use smithay::wayland::cursor_shape::CursorShapeManagerState;
use smithay::wayland::dmabuf::DmabufState;
use smithay::wayland::drm_syncobj::DrmSyncobjState;
use smithay::wayland::fractional_scale::FractionalScaleManagerState;
use smithay::wayland::idle_inhibit::IdleInhibitManagerState;
use smithay::wayland::idle_notify::IdleNotifierState;
use smithay::wayland::input_method::InputMethodManagerState;
use smithay::wayland::keyboard_shortcuts_inhibit::{
    KeyboardShortcutsInhibitState, KeyboardShortcutsInhibitor,
};
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::pointer_constraints::{with_pointer_constraint, PointerConstraintsState};
use smithay::wayland::pointer_gestures::PointerGesturesState;
use smithay::wayland::presentation::PresentationState;
use smithay::wayland::relative_pointer::RelativePointerManagerState;
use smithay::wayland::security_context::SecurityContextState;
use smithay::wayland::selection::data_device::{
    clear_data_device_selection, set_data_device_selection, DataDeviceState,
};
use smithay::wayland::selection::ext_data_control::DataControlState as ExtDataControlState;
use smithay::wayland::selection::primary_selection::{
    clear_primary_selection, PrimarySelectionState,
};
use smithay::wayland::selection::wlr_data_control::DataControlState as WlrDataControlState;
use smithay::wayland::session_lock::{LockSurface, SessionLockManagerState, SessionLocker};
use smithay::wayland::shell::kde::decoration::KdeDecorationState;
use smithay::wayland::shell::wlr_layer::{self, Layer, WlrLayerShellState};
use smithay::wayland::shell::xdg::decoration::XdgDecorationState;
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shm::ShmState;
#[cfg(test)]
use smithay::wayland::single_pixel_buffer::SinglePixelBufferState;
use smithay::wayland::socket::ListeningSocketSource;
use smithay::wayland::tablet_manager::TabletManagerState;
use smithay::wayland::text_input::TextInputManagerState;
use smithay::wayland::viewporter::ViewporterState;
use smithay::wayland::virtual_keyboard::VirtualKeyboardManagerState;
use smithay::wayland::xdg_activation::XdgActivationState;
use smithay::wayland::xdg_foreign::XdgForeignState;
use synoik_config::debug::PreviewRender;
use synoik_config::output::MaxBpc;
use synoik_config::{
    Config, FloatOrInt, Key, Modifiers, OutputName, TrackLayout, WarpMouseToFocusMode,
    WindowingMode, WorkspaceReference, Xkb,
};
use wayland_server::protocol::wl_output::WlOutput;

use crate::a11y::A11y;
use crate::animation::{Animation, Clock};
use crate::app_system::AppIconRef;
use crate::backend::tty::SurfaceDmabufFeedback;
use crate::backend::{Backend, BackendMode, Headless, RenderResult, Tty};
use crate::cursor::{CursorManager, CursorTextureCache, RenderCursor, XCursor};
use crate::dbus::freedesktop_locale1::Locale1ToSynoik;
use crate::dbus::freedesktop_login1::Login1ToSynoik;
use crate::dbus::gnome_shell_introspect::{self, IntrospectToSynoik, SynoikToIntrospect};
use crate::dbus::gnome_shell_screenshot::{ScreenshotToSynoik, SynoikToScreenshot};
use crate::dbus::system_status::SystemStatusToSynoik;
use crate::frame_clock::{Dispatch, FrameClock};
use crate::frame_log::{AnimCauses, FrameContext, FrameLog, Phase};
use crate::gnome::{AccelGrab, GnomeSettings, GnomeSettingsWriter};
use crate::handlers::{configure_lock_surface, XDG_ACTIVATION_TOKEN_TIMEOUT};
use crate::input::pick_color_grab::PickColorGrab;
use crate::input::pressure::{Barrier, Edge, Segment};
use crate::input::scroll_swipe_gesture::ScrollSwipeGesture;
use crate::input::scroll_tracker::ScrollTracker;
use crate::input::{
    apply_libinput_settings, mods_with_finger_scroll_binds, mods_with_mouse_binds,
    mods_with_tablet_stylus_binds, mods_with_wheel_binds, OverviewHit, TabletData,
};
use crate::ipc::server::IpcServer;
use crate::layer::mapped::LayerSurfaceRenderElement;
use crate::layer::MappedLayer;
use crate::layout::tile::TileRenderElement;
use crate::layout::workspace::{Workspace, WorkspaceId};
use crate::layout::{
    HitType, Layout, LayoutElement as _, LayoutElementRenderElement, MonitorRenderElement,
    SizingMode,
};
use crate::notifications::{bounded_pixels, PixelIcon};
use crate::protocols::ext_workspace::{self, ExtWorkspaceManagerState};
use crate::protocols::foreign_toplevel::{self, ForeignToplevelManagerState};
use crate::protocols::gamma_control::GammaControlManagerState;
use crate::protocols::mutter_x11_interop::MutterX11InteropManagerState;
use crate::protocols::output_management::OutputManagementManagerState;
use crate::protocols::screencopy::{Screencopy, ScreencopyBuffer, ScreencopyManagerState};
use crate::protocols::session_management::{SessionManagerHandler as _, SessionManagerState};
use crate::protocols::virtual_pointer::VirtualPointerManagerState;
use crate::render_helpers::blur::BlurOptions;
use crate::render_helpers::captured_texture::CapturedTextureRenderElement;
use crate::render_helpers::debug::push_opaque_regions;
use crate::render_helpers::icon::{AppIconCache, IconCache, ImageCache};
use crate::render_helpers::memory::MemoryBuffer;
use crate::render_helpers::offscreen::{OffscreenBuffer, OffscreenRenderElement};
use crate::render_helpers::rounded_texture::RoundedTextureRenderElement;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::surface::push_elements_from_surface_tree;
use crate::render_helpers::texture::TextureRenderElement;
use crate::render_helpers::vulkan::{VkTexture, VulkanRenderer};
use crate::render_helpers::xray::{Xray, XrayPos};
use crate::render_helpers::{
    encompassing_geo, render_to_dmabuf, render_to_shm, render_to_vec, RenderCtx, RenderTarget,
};
#[cfg(feature = "xdp-gnome-screencast")]
use crate::screencasting::Screencasting;
use crate::session_state::{ToplevelRecord, WindowState};
use crate::synoik_render_elements;
use crate::system_status::SystemStatus;
use crate::ui::app_grid::{AppGrid, AppGridEntry};
use crate::ui::dash::{Dash, DashEntry};
use crate::ui::end_session_dialog::{EndSessionDialog, EndSessionDialogRenderElement};
use crate::ui::exit_confirm_dialog::{ExitConfirmDialog, ExitConfirmDialogRenderElement};
use crate::ui::hotkey_overlay::HotkeyOverlay;
use crate::ui::overview_search::{OverviewSearch, SearchResultEntry};
use crate::ui::panel::{Panel, PanelElement};
use crate::ui::popover::PanelPopover;
use crate::ui::run_dialog::{RunDialog, RunDialogRenderElement};
use crate::ui::screen_transition::{self, ScreenTransition};
use crate::ui::screenshot_ui::{
    CaptureMode, CaptureType, CastAreaIndicator, OutputScreenshot, PendingTarget, PointerDown,
    PointerUp, ScreenshotNeutral, ScreenshotUi, ScreenshotUiRenderElement,
};
use crate::ui::switcher::app_switcher::app_items;
use crate::ui::switcher::ui::{Items, OpenRequest};
use crate::ui::switcher::SwitcherKey;
use crate::ui::thumbnail_chrome::{ThumbnailChrome, ThumbnailClose};
use crate::ui::window_preview::{PreviewChrome, PreviewOverlay};
use crate::utils::scale::{closest_representable_scale, guess_monitor_scale};
use crate::utils::spawning::{CHILD_DISPLAY, CHILD_ENV};
use crate::utils::vblank_throttle::VBlankThrottle;
use crate::utils::xwayland::satellite::Satellite;
use crate::utils::{
    center, center_f64, crop_rgba8, expand_home, get_monotonic_time, ipc_transform_to_smithay,
    is_laptop_panel, is_mapped, logical_output, make_display_name, make_screenshot_path,
    output_matches_name, output_size, panel_orientation, send_scale_transform, write_png_rgba8,
    xwayland,
};
use crate::wallpaper::Wallpaper;
use crate::window::mapped::MappedId;
use crate::window::{InitialConfigureState, Mapped, ResolvedWindowRules, Unmapped, WindowRef};

/// How often the live device-memory census is logged (DEBUG only).
///
/// Long enough that a whole session's worth of lines stays readable in the journal, short enough to
/// see a leak's slope inside one sitting.
const DEVICE_MEMORY_CENSUS_PERIOD: Duration = Duration::from_secs(30);

/// Log target for that census, kept off `synoik`'s own tree so it can be enabled by itself.
const DEVICE_MEMORY_CENSUS_TARGET: &str = "devmem";

const CLEAR_COLOR_LOCKED: [f32; 4] = [0.3, 0.1, 0.1, 1.];

/// The screen shield's wash over the wallpaper: black at `1 - BLUR_BRIGHTNESS`, which multiplies
/// what shows through by GNOME's `BLUR_BRIGHTNESS` (`js/ui/unlockDialog.js:34`). Premultiplied,
/// like every other [`SolidColorBuffer`] color here.
const SHIELD_DIM_COLOR: [f32; 4] = {
    let a = 1. - crate::ui::lock_screen::BLUR_BRIGHTNESS as f32;
    [0., 0., 0., a]
};

/// The overview backdrop's blur radius, in **stage pixels** (output physical pixels) like
/// gnome-shell's own blur constants — [`crate::wallpaper::Wallpaper::render_blurred`] converts it
/// into the wallpaper texture's resolution.
///
/// **Divergence (deliberate).** gnome-shell fills `#overviewGroup` with a flat
/// `$system_base_color` (`_overview.scss:7-9`); we show the wallpaper blurred behind the shrunk
/// workspaces instead, the way the widely-used Blur my Shell extension does. The workspace
/// previews keep their own unblurred wallpaper, so the backdrop reads as the same picture pushed
/// out of focus rather than as a slab of grey — which is the whole point of the effect.
const OVERVIEW_BLUR_RADIUS: f64 = 90.;

/// What the overview backdrop's blur multiplies the wallpaper by — the dim that keeps a bright
/// picture from competing with the window previews and the white dash/search chrome over it. Rides
/// *inside* the blur pass, like the lock screen's [`crate::ui::lock_screen::BLUR_BRIGHTNESS`].
const OVERVIEW_BLUR_BRIGHTNESS: f32 = 0.45;

/// Every render target, in `RenderTarget as usize` order — so a `[T; RenderTarget::COUNT]` built
/// by mapping over this can be indexed by target. A frozen screen (screenshot UI, screen
/// transition) must be captured once per target, since block-out rules key off the target.
const ALL_RENDER_TARGETS: [RenderTarget; RenderTarget::COUNT] = [
    RenderTarget::Output,
    RenderTarget::Screencast,
    RenderTarget::ScreenCapture,
];

// We'll try to send frame callbacks at least once a second. We'll make a timer that fires once a
// second, so with the worst timing the maximum interval between two frame callbacks for a surface
// should be ~1.995 seconds.
const FRAME_CALLBACK_THROTTLE: Option<Duration> = Some(Duration::from_millis(995));

/// How long the app catalog waits for `installed-changed` pings to stop before
/// reloading — gnome-shell's `DEFAULT_TIMEOUT_SECONDS` (`src/shell-app-cache.c:28`),
/// which it uses both as the coalescing timeout and as its directory monitors'
/// rate limit. See [`Synoik::queue_app_catalog_reload`].
const APP_CATALOG_RELOAD_DEBOUNCE: Duration = Duration::from_secs(5);

/// Encode RGBA8 to PNG and write it, off the compositor thread.
///
/// `on_done` fires **after** the bytes are on disk. A D-Bus reply that carried the filename before
/// the write finished would hand a portal an empty file to read.
fn write_png_in_thread(
    size: Size<i32, Physical>,
    pixels: Vec<u8>,
    path: PathBuf,
    on_done: impl FnOnce(PathBuf) + Send + 'static,
) {
    debug!("saving screenshot to {path:?}");

    thread::spawn(move || {
        let file = match std::fs::File::create(&path) {
            Ok(file) => file,
            Err(err) => {
                warn!("error creating file: {err:?}");
                return;
            }
        };

        let w = std::io::BufWriter::new(file);
        if let Err(err) = write_png_rgba8(w, size.w as u32, size.h as u32, &pixels) {
            warn!("error encoding screenshot image: {err:?}");
            return;
        }

        on_done(path);
    });
}

/// What the portal's share picker calls the dynamic-cast pseudo-window.
///
/// User-visible, and deliberately says what it *does* rather than who made it — it sits in a list
/// beside the user's real windows in a shell that presents itself as GNOME.
#[cfg(feature = "xdp-gnome-screencast")]
pub const DYNAMIC_CAST_TARGET_LABEL: &str = "Dynamic Target";

/// What the reload timer should do when it fires: `Some(deadline)` to wait again (a ping
/// arrived mid-wait and pushed the deadline out), `None` to reload now.
///
/// Split out of the timer callback because the callback is the only place the debounce can
/// go wrong — a timer that always reloads makes the coalescing a no-op, one that always
/// re-waits never reloads at all — and neither is reachable from a test without sleeping
/// out a real five seconds.
fn app_catalog_reload_wait(
    deadline: Option<std::time::Instant>,
    now: std::time::Instant,
) -> Option<std::time::Instant> {
    deadline.filter(|at| *at > now)
}

/// Size of the icon carried by an app drag, logical px. gnome-shell drags the
/// icon actor itself, which in the dash is 64px (`dash.js:321`) — the app grid's
/// 96px icon shrinks to the same 64 through `dragActorMaxSize` there
/// (`appDisplay.js:1096`), so one size covers both sources.
const APP_DRAG_ICON_PX: f64 = 64.;

/// An app icon being dragged out of the dash or the app grid toward a workspace.
///
/// gnome-shell makes every `AppIcon` a DND source (`AppViewItem`'s draggable) and
/// lets a `Workspace` take the drop: `source.app.open_new_window(workspaceIndex)`
/// (`workspace.js:1429-1434`). The drag actor is the icon itself, carried at the
/// grab point.
#[derive(Debug)]
pub struct AppDrag {
    /// The desktop id being dragged.
    pub id: String,
    /// Its icon — the drag actor.
    pub icon: AppIconRef,
    /// When the dragged item is a *folder*, the member icons its tile composes (up to
    /// four). GNOME's `FolderIcon.getDragActor` builds a `BaseIcon` with the folder's own
    /// `_createIcon` and its `overview-tile app-folder` style class
    /// (`appDisplay.js:2286,2368-2379`), so the drag actor is the composed 2x2 over the
    /// raised folder background — not any one member's icon.
    pub folder: Option<Vec<AppIconRef>>,
    /// Output the pointer is on.
    pub output: Output,
    /// Pointer position within that output.
    pub pos: Point<f64, Logical>,
    /// Where the pointer sat inside the icon when the drag began, so it doesn't
    /// jump to the center as it is picked up.
    pub grab_offset: Point<f64, Logical>,
    /// The folder this icon was dragged *out of*, if it came from an open folder dialog.
    /// GNOME tracks it as `_getViewFromIcon(source) instanceof FolderView`, which is what
    /// makes the drop remove the app from the folder (`AppDisplay.acceptDrop`,
    /// `appDisplay.js:1680-1697`) and what puts a placeholder in the grid to reorder.
    pub from_folder: Option<String>,
    /// Whether the pointer is over the dash's unpin target (the show-apps button,
    /// which offers removal for the duration of the drag — `Dash::unpin_target_at`).
    /// Drives both the button's hover feedback and what the drop does.
    pub unpin: bool,
}

/// A press on the app-grid background that may turn into a page drag.
#[derive(Debug)]
pub struct AppGridPan {
    pub button: u32,
    pub output: Output,
    pub origin: Point<f64, Logical>,
    pub last: Point<f64, Logical>,
    pub dragging: bool,
}

/// A tray item we told to activate, waiting for the window it opens.
///
/// **Why this exists at all.** A Wayland client cannot place its own toplevel, and the spec's
/// `Activate(x, y)` hint — "an hint to the item where to show eventual windows" — is therefore
/// unactionable by the client even when it wants to obey. So a tray window's position is ours to
/// choose or nobody's, and "wherever a floating window happens to land" is what nobody's looks
/// like. This is an invented behavior with no GNOME reference (GNOME has no tray at all); see
/// `docs/fork/status-notifier-port.md`.
///
/// **Why the token and nothing else.** The obvious alternative — match the new window's client
/// PID against the item's D-Bus connection — is dead for sandboxed clients, which are most of the
/// interesting ones: their bus traffic goes through `xdg-dbus-proxy`, so
/// `GetConnectionUnixProcessID` names the *proxy*. Measured on the seat: Nextcloud's item resolved
/// to pid 138293 (`xdg-dbus-proxy`) while its window's client was 138297 (`nextcloud`), not even
/// on the same branch of the process tree. The activation token we hand the item before `Activate`
/// is the only thing that crosses that boundary intact — and only if the client passes it on.
#[derive(Debug)]
pub struct IndicatorActivation {
    /// The token handed to the item via `ProvideXdgActivationToken`.
    pub token: String,
    /// The icon's rect on the panel, output-local logical — where the window should appear.
    pub anchor: Rectangle<f64, Logical>,
    pub output: Output,
    /// When the record expires. A click that opens nothing must not place an unrelated window
    /// minutes later.
    pub expires: Duration,
}

pub struct Synoik {
    pub config: Rc<RefCell<Config>>,

    /// Output config from the config file.
    ///
    /// This does not include transient output config changes done via IPC. It is only used when
    /// reloading the config from disk to determine if the output configuration should be reloaded
    /// (and transient changes dropped).
    pub config_file_output_config: synoik_config::Outputs,

    pub event_loop: LoopHandle<'static, State>,
    pub scheduler: Scheduler<()>,
    pub stop_signal: LoopSignal,
    pub display_handle: DisplayHandle,

    /// Whether synoik was run with `--session`
    pub is_session_instance: bool,

    /// Name of the Wayland socket.
    ///
    /// This is `None` when creating `Synoik` without a Wayland socket.
    pub socket_name: Option<OsString>,

    pub start_time: Instant,

    /// Whether the at-startup=true window rules are active.
    pub is_at_startup: bool,

    /// Clock for driving animations.
    pub clock: Clock,

    // Each workspace corresponds to a Space. Each workspace generally has one Output mapped to it,
    // however it may have none (when there are no outputs connected) or multiple (when mirroring).
    pub layout: Layout<Mapped>,

    // This space does not actually contain any windows, but all outputs are mapped into it
    // according to their global position.
    pub global_space: Space<Window>,

    /// Mapped outputs, sorted by their name and position.
    pub sorted_outputs: Vec<Output>,

    // Windows which don't have a buffer attached yet.
    pub unmapped_windows: HashMap<WlSurface, Unmapped>,

    /// Layer surfaces which don't have a buffer attached yet.
    pub unmapped_layer_surfaces: HashSet<WlSurface>,

    /// Extra data for mapped layer surfaces.
    pub mapped_layer_surfaces: HashMap<LayerSurface, MappedLayer>,

    // Cached root surface for every surface, so that we can access it in destroyed() where the
    // normal get_parent() is cleared out.
    pub root_surface: HashMap<WlSurface, WlSurface>,

    // Dmabuf readiness pre-commit hook for a surface.
    pub dmabuf_pre_commit_hook: HashMap<WlSurface, HookId>,

    /// Clients to notify about their blockers being cleared.
    pub blocker_cleared_tx: Sender<Client>,
    pub blocker_cleared_rx: Receiver<Client>,

    pub output_state: HashMap<Output, OutputState>,

    // When false, we're idling with monitors powered off.
    pub monitors_active: bool,

    /// Whether the laptop lid is closed.
    ///
    /// Libinput guarantees that the lid switch starts in open state, and if it was closed during
    /// startup, libinput will immediately send a closed event.
    pub is_lid_closed: bool,

    pub devices: HashSet<input::Device>,
    pub tablets: HashMap<input::Device, TabletData>,
    pub touch: HashSet<input::Device>,

    // Smithay state.
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub xdg_decoration_state: XdgDecorationState,
    pub kde_decoration_state: KdeDecorationState,
    pub layer_shell_state: WlrLayerShellState,
    pub session_lock_state: SessionLockManagerState,
    pub foreign_toplevel_state: ForeignToplevelManagerState,
    pub ext_workspace_state: ExtWorkspaceManagerState,
    pub screencopy_state: ScreencopyManagerState,
    pub output_management_state: OutputManagementManagerState,
    pub viewporter_state: ViewporterState,
    pub background_effect_state: BackgroundEffectState,
    pub xdg_foreign_state: XdgForeignState,
    pub shm_state: ShmState,
    pub output_manager_state: OutputManagerState,
    pub dmabuf_state: DmabufState,
    // linux-drm-syncobj-v1 explicit-sync global. `Some` on the tty backend while the primary GPU
    // is up and supports syncobj eventfd (created in `Tty::device_added`, torn down and reset to
    // `None` in `Tty::device_removed` so the device fd can be closed cleanly and a re-added
    // primary rebuilds it against the fresh fd); always `None` on winit/headless (no DRM device).
    // Renderer-agnostic: benefits both the GLES and Vulkan paths.
    pub drm_syncobj_state: Option<DrmSyncobjState>,
    pub fractional_scale_manager_state: FractionalScaleManagerState,
    pub seat_state: SeatState<State>,
    pub tablet_state: TabletManagerState,
    pub text_input_state: TextInputManagerState,
    pub input_method_state: InputMethodManagerState,
    pub keyboard_shortcuts_inhibit_state: KeyboardShortcutsInhibitState,
    pub virtual_keyboard_state: VirtualKeyboardManagerState,
    pub virtual_pointer_state: VirtualPointerManagerState,
    pub pointer_gestures_state: PointerGesturesState,
    pub relative_pointer_state: RelativePointerManagerState,
    pub pointer_constraints_state: PointerConstraintsState,
    pub idle_notifier_state: IdleNotifierState<State>,
    pub idle_inhibit_manager_state: IdleInhibitManagerState,
    /// `org.gnome.Mutter.IdleMonitor` watch bookkeeping (gsd-power's dim/blank/suspend). Driven by
    /// `notify_activity` and `idle_monitor_timer`; `WatchFired` goes out via
    /// `emit_idle_watch_fired`.
    pub idle_monitor: crate::idle_monitor::IdleMonitor,
    /// The single timer re-armed to the next idle watch's deadline (see
    /// `IdleMonitor::next_wakeup`).
    pub idle_monitor_timer: Option<RegistrationToken>,
    /// `org.gnome.SessionManager.EndSessionDialog` lifecycle: gnome-session's logout/shutdown/
    /// restart confirmation. The interactive surface is `end_session_dialog`;
    /// `Confirmed*`/`Canceled` go out via `emit_end_session_signal`.
    pub end_session: crate::end_session::EndSession,
    /// The timer armed to the countdown's auto-confirm deadline (see `EndSession::deadline`).
    pub end_session_timer: Option<RegistrationToken>,
    /// Where `org.gnome.Software.OfflineUpdates.GetState` replies come back to. `None` with no
    /// session bus, which simply means the update checkbox is never offered.
    pub offline_update_tx: Option<calloop::channel::Sender<crate::end_session::OfflineUpdateState>>,
    /// 1 s repeating timer that ticks the R1 screen-recording indicator's `M:SS` label while any
    /// recording is live; `None` when nothing is recording.
    pub recording_tick: Option<RegistrationToken>,
    pub data_device_state: DataDeviceState,
    /// Mime types whoever owns the clipboard right now is offering.
    ///
    /// Smithay tracks the selection but exposes no way to ask what it offers, and a paste has
    /// to pick a mime type *before* it can request the data. Kept in step from the two places
    /// the selection changes: `SelectionHandler::new_selection` (a client took it) and
    /// [`Self::set_clipboard`] (we did).
    pub clipboard_mime_types: Vec<String>,
    /// Whether a paste into one of our entries is waiting on the clipboard owner's pipe.
    ///
    /// One at a time: a held `Ctrl-v` would otherwise stack an fd source and a timer per key
    /// repeat, every one of them inserting into the same field when it lands.
    pub clipboard_paste_pending: bool,
    pub primary_selection_state: PrimarySelectionState,
    pub wlr_data_control_state: WlrDataControlState,
    pub ext_data_control_state: ExtDataControlState,
    pub popups: PopupManager,
    pub popup_grab: Option<PopupGrabState>,
    pub presentation_state: PresentationState,
    pub security_context_state: SecurityContextState,
    pub gamma_control_manager_state: GammaControlManagerState,
    pub activation_state: XdgActivationState,
    pub mutter_x11_interop_state: MutterX11InteropManagerState,
    pub session_manager_state: SessionManagerState,
    /// Armed by [`State::schedule_session_save`]; `None` means no write is pending.
    pub session_save_timer: Option<RegistrationToken>,

    // This will not work as is outside of tests, so it is gated with #[cfg(test)] for now. In
    // particular, shaders will need to learn about the single pixel buffer. Also, it must be
    // verified that a black single-pixel-buffer background lets the foreground surface to be
    // unredirected.
    //
    // https://github.com/niri-wm/niri/issues/619
    #[cfg(test)]
    pub single_pixel_buffer_state: SinglePixelBufferState,

    pub seat: Seat<State>,

    /// The compositor-as-input-method model, `None` until an IBus daemon is reachable.
    /// See [`crate::input_method`].
    pub input_method: Option<crate::input_method::InputMethod>,
    /// The focused client's surrounding text and caret byte offset, as last committed.
    ///
    /// Kept because `delete_surrounding_text` arrives in *characters* and goes out in bytes, so
    /// the conversion needs the text the client last told us about.
    pub im_surrounding: Option<(String, u32)>,
    /// Deadline for the oldest keystroke the engine has not answered for, so a wedged daemon
    /// cannot hold the keyboard indefinitely.
    pub im_key_timer: Option<RegistrationToken>,

    /// Inspectable model of the GNOME settings the compositor honors.
    pub gnome_settings: GnomeSettings,
    /// Writes settings back to the GSettings store; `None` when headless.
    pub gnome_settings_writer: Option<GnomeSettingsWriter>,
    /// The application catalog (installed apps, favorites, search, launch) the
    /// dash and overview search resolve through. `disconnected` (empty) when
    /// headless; tests inject fakes.
    pub app_system: crate::app_system::AppSystem,
    /// When a coalesced catalog reload is due, if one is pending — see
    /// [`Synoik::queue_app_catalog_reload`]. `Some` also means a timer is already armed, so
    /// a burst of `installed-changed` pings arms exactly one.
    pub app_catalog_reload_at: Option<std::time::Instant>,
    /// Live network + battery state for the panel status area (from the system-bus
    /// watcher); stays at its `Unknown`/absent default without the `dbus` feature.
    pub system_status: SystemStatus,
    /// A battery forced by `debug-set-battery`, replacing UPower's until cleared.
    ///
    /// Overlaid where the status reaches the panel ([`Self::panel_system_status`]) rather than
    /// written into `system_status`, so the live UPower snapshot underneath stays true and the
    /// next real update does not silently reinstate itself over the override.
    pub battery_override: Option<crate::system_status::BatteryStatus>,
    /// The authoritative last-selected (non-Balanced) power profile the Power Mode tile toggles
    /// back to (gnome-shell's `last-selected-power-profile`). Seeded from gsettings at
    /// startup, updated from each power-profile echo, and write-through-persisted — kept here
    /// (not re-read from the gsettings model, which the watcher rebuilds from defaults on
    /// every unrelated change).
    pub last_power_profile: String,
    /// A clone of the system-status watcher's inbound channel, for
    /// [`crate::dbus::bluez::connect_device`] to report `BluetoothConnectDone` back through the
    /// same path the snapshots take. `None` without the `dbus` feature / before D-Bus starts.
    pub system_status_tx:
        Option<calloop::channel::Sender<crate::dbus::system_status::SystemStatusToSynoik>>,
    /// Every output that has a usable display backlight, with its current brightness. Empty
    /// without the TTY backend or without backlight hardware — which is also how the brightness
    /// slider stays absent, as in GNOME (`brightness.js:59-60`).
    pub backlight: crate::backlight::BacklightSnapshot,
    /// The shell-side brightness algebra over that snapshot: the global scale the quick-settings
    /// slider drives, plus one scale per backlit output. Dormant (no global scale) when nothing is
    /// backlit.
    pub brightness: crate::brightness::BrightnessManager,
    /// The outbound half of `org.gnome.Shell.Brightness`: `HasBrightnessControl` changes and the
    /// `BrightnessChanged` signal. `None` without the `dbus` feature / before D-Bus starts.
    pub brightness_emit:
        Option<async_channel::Sender<crate::dbus::gnome_shell_brightness::SynoikToBrightness>>,
    /// The screen shield — GNOME's session lock (`ScreenShield`, `js/ui/screenShield.js`).
    pub screen_shield: crate::screen_shield::ScreenShield,
    /// What the shield *looks* like: the curtain's clock and hint (`js/ui/unlockDialog.js`). Kept
    /// beside the model rather than inside it so the model stays renderer-free and testable.
    pub lock_screen: crate::ui::lock_screen::LockScreen,
    /// The unlock prompt's state — which page is up, what has been typed, what gdm last said.
    pub unlock_dialog: crate::unlock_dialog::UnlockDialog,
    /// Callers of `org.gnome.ScreenSaver.Lock` still waiting to hear the shield is on screen.
    ///
    /// A list because two can be waiting at once, and each owes its own reply. Every path that
    /// resolves a lock — the curtain landing, a refusal, the shield going away before it lands —
    /// must drain this, or the caller hangs until its D-Bus timeout.
    pub lock_replies: Vec<crate::dbus::gnome_screen_saver::LockReply>,
    /// The session user's AccountsService account, as far as it has been read.
    ///
    /// Defaults are the conservative ones and stay in force until the service answers — most
    /// importantly `PasswordMode::Regular`, so a lock that happens before the reply lands demands
    /// a password rather than being a screensaver anyone can wave away.
    pub user_account: crate::dbus::accounts_service::UserAccount,
    /// Whether this machine has more than one ordinary account, for the "Other User" button.
    pub multiple_users: bool,
    /// What the fprintd probe found, if anything — see [`crate::dbus::fprintd`]. `None` (the
    /// default) is what a machine with no reader keeps forever, and it is what stops
    /// `gdm-fingerprint` from ever being started.
    pub fingerprint_reader: crate::dbus::fprintd::ReaderType,
    /// Whether the card this session was unlocked with is in the reader
    /// (`smartcardDetected`, `js/gdm/util.js:127`). Already reduced by the setting and by the
    /// login-token rule — see [`crate::dbus::smartcard`].
    ///
    /// **Nothing acts on it yet**: GNOME would make `gdm-smartcard` preempt the password
    /// conversation, and that restructuring is deliberately deferred until there is a card to
    /// prove it against.
    pub smartcard_detected: bool,
    /// Whether logind gave us a seat id — libaccountsservice's `can_switch()`, which is a seat
    /// lookup and nothing else now that `sd_seat_can_multi_session` is gone. Resolved once, off
    /// the main loop, by the same task that starts the AccountsService watch.
    pub can_switch_user: bool,
    /// Whether the pointer is on the switch-user button.
    pub switch_user_hovered: bool,

    /// The last `active` we told the session bus about — see [`Self::publish_shield_active`].
    ///
    /// Tracked separately from the model's own `active` because the two deliberately disagree for
    /// the length of the curtain's slide.
    published_active: bool,

    /// Caps lock, sampled from the keyboard after each key the shield saw.
    ///
    /// GNOME reads it from the keymap whenever it needs it (`shellEntry.js:192`); we have no
    /// keymap signal, so it is remembered here. Remembered rather than passed along with the
    /// key because a *question* arriving from gdm also changes whether a warning is owed, and
    /// no key is involved in that at all.
    pub caps_lock: bool,
    /// Drives [`crate::dbus::gdm`]. `None` before D-Bus starts, and on a build without it — in
    /// which case the shield never gets a verifier and so never locks, which is the correct
    /// behaviour rather than a degradation.
    pub gdm_requests: Option<async_channel::Sender<crate::dbus::gdm::VerifierRequest>>,
    /// The pending idle lock (`_lockTimeoutId`). Armed when the session goes idle, dropped when
    /// the user comes back — the grace period is exactly this token's lifetime.
    pub lock_timer: Option<calloop::RegistrationToken>,
    /// The idle fade's completion, which is what actually puts the shield down.
    pub fade_timer: Option<calloop::RegistrationToken>,
    /// Wakes the unlock dialog when the message on screen has had its read time.
    ///
    /// Its own timer rather than the panel's minute tick: a message is owed two seconds, and a
    /// tick that lands up to a minute later is not a queue, it is a stall.
    pub unlock_message_timer: Option<calloop::RegistrationToken>,
    /// logind's `Session.Active`: whether our VT is the one on screen. Assumed true until logind
    /// says otherwise, which is right for the usual case of starting on the active VT.
    pub session_active: bool,
    /// logind's `delay` sleep inhibitor, held while a suspend would still owe a lock. Dropping the
    /// fd is what tells logind to go ahead and suspend, so this field's *lifetime* is the
    /// mechanism, not a handle we happen to keep.
    pub sleep_inhibitor: Option<zbus::zvariant::OwnedFd>,
    /// Outputs that still owe a *presented* frame of the settled curtain before the machine may
    /// suspend. Non-empty only between `PrepareForSleep(true)` and the flip that puts the shield
    /// on screen — see [`crate::screen_shield::ScreenShield::wants_sleep_inhibitor`].
    ///
    /// Held as the outputs themselves, and read through [`Self::shield_frame_owed`], which ignores
    /// any that have gone away: an output unplugged mid-suspend owes nothing, and making the
    /// removal path remember that would be a second place to get it wrong.
    pub shield_frames_owed: HashSet<Output>,
    /// The bound on the above (see [`Self::SHIELD_PRESENT_DEADLINE`]).
    pub shield_present_deadline: Option<calloop::RegistrationToken>,
    /// What `GetActive` / `GetActiveTime` read, mirrored out of [`Self::screen_shield`] on every
    /// change so the bus task can answer without a round trip through the event loop.
    pub shield_snapshot:
        std::sync::Arc<std::sync::Mutex<crate::dbus::gnome_screen_saver::ShieldSnapshot>>,
    /// `ActiveChanged` / `WakeUpScreen`. `None` before D-Bus starts.
    pub screen_saver_emit:
        Option<async_channel::Sender<crate::dbus::gnome_screen_saver::SynoikToScreenSaver>>,
    /// A clone of the login1 watcher's inbound channel, so a brightness write can report its
    /// completion back to the write serializer through the same path.
    pub login1_tx:
        Option<calloop::channel::Sender<crate::dbus::freedesktop_login1::Login1ToSynoik>>,
    /// The notifications model behind the banner/list/indicator surfaces and the
    /// `org.freedesktop.Notifications` server (empty when the server isn't running,
    /// e.g. headless or without the `dbus` feature).
    pub notifications: crate::notifications::NotificationStore,
    /// Emit-command channel back to the notifications server task, which owns the
    /// bus connection and performs the (unicast) signal emission; `None` when the
    /// server isn't running.
    pub notifications_emit:
        Option<async_channel::Sender<crate::notifications::SynoikToNotifications>>,
    /// Emit-command channel to the `org.gtk.Notifications` server task (separate
    /// from [`Self::notifications_emit`] because that interface's `ActionInvoked`
    /// differs in shape and is broadcast); `None` when the server isn't running.
    pub gtk_notifications_emit:
        Option<async_channel::Sender<crate::notifications::GtkToNotifications>>,
    /// The dateMenu Events source (org.gnome.Shell.CalendarServer), empty until
    /// the watcher reports; see [`crate::calendar_events`].
    pub calendar_events: crate::calendar_events::CalendarEventStore,
    /// Range-request channel to the calendar-server watcher task, which owns the
    /// bus connection; `None` when the watcher isn't running.
    pub calendar_range_emit:
        Option<async_channel::Sender<crate::calendar_events::SynoikToCalendar>>,
    /// The media players on the session bus, behind the message list's media cards; see
    /// [`crate::mpris`].
    pub mpris: crate::mpris::MprisStore,
    /// Control channel to the MPRIS watcher task, which owns the bus connection; `None` when the
    /// watcher isn't running.
    pub mpris_emit: Option<async_channel::Sender<crate::mpris::SynoikToMpris>>,
    /// The app indicators registered with our `org.kde.StatusNotifierWatcher`, in registration
    /// order; see [`crate::status_notifier`] and `docs/fork/status-notifier-port.md`.
    pub status_notifier: crate::status_notifier::IndicatorStore,
    /// Menu channel to the app-indicator watcher's task, which owns the bus connection; `None`
    /// when the watcher isn't running.
    pub status_notifier_emit:
        Option<async_channel::Sender<crate::status_notifier::SynoikToStatusNotifier>>,
    /// The item whose remote menu the watcher is currently following, reconciled against the
    /// popover each cycle by `State::reconcile_indicator_menu`.
    pub indicator_menu_open: Option<String>,
    /// Windows we asked a tray item to open, waiting for their toplevel to arrive; see
    /// [`IndicatorActivation`].
    pub indicator_activations: Vec<IndicatorActivation>,
    /// The on-screen notification banner (gnome-shell's MessageTray popup).
    pub notification_banner: crate::ui::notification_banner::NotificationBanner,
    /// The on-screen display (volume/brightness/…), one window per output.
    pub osd: crate::ui::osd::OsdManager,
    /// Wake-up for the OSD's 1500 ms hide timeout: an OSD over a static desktop
    /// produces no frames of its own to expire on.
    pub osd_timer: Option<RegistrationToken>,
    /// The deadline `osd_timer` is actually armed for. The OSD arms its deadline in
    /// `show()`, which can happen from anywhere between frames (a D-Bus call, a
    /// brightness key), so a before/after-`advance_animations` comparison cannot see
    /// it — a replaced OSD would keep the *old* timer and, once that fired,
    /// re-arm nothing and hang on screen until unrelated damage.
    pub osd_timer_at: Option<Duration>,
    /// Wake-up for the dock's auto-hide, and the deadline it is armed for. Same shape as the
    /// OSD's above, and needed for the same reason twice over: a *shown* dock reports no ongoing
    /// animation, so nothing asks for frames, so the grace period after the pointer leaves never
    /// elapses and the dock sits on screen forever. Its deadline is also set between frames (by
    /// the pointer leaving), so a before/after-`advance_animations` diff would never re-arm.
    pub dock_timer: Option<RegistrationToken>,
    pub dock_timer_at: Option<Duration>,
    pub switcher_timer: Option<RegistrationToken>,
    pub switcher_timer_at: Option<Duration>,
    /// An outcome produced by a timer firing inside `advance_animations`, which is `Synoik`-level
    /// and so cannot activate a window itself. Drained by [`State::finish_switcher`].
    pub pending_switcher_outcome: Option<(
        crate::ui::switcher::SwitcherOutcome,
        crate::ui::switcher::ui::Activation,
    )>,
    /// Where a running cycler's `.cycler-highlight` goes, on the switcher's own output. Re-derived
    /// every frame by [`Synoik::sync_switcher_preview`]; `None` whenever no cycler is up.
    pub cycler_highlight: Option<Rectangle<f64, Logical>>,
    /// Monitors a switcher preview took off their workspace, with where to put each one back.
    ///
    /// One entry per output, recorded on first touch: tabbing on across three workspaces still
    /// owes the screen back to where the *session* started, not to the last stop before this one.
    pub switcher_ws_preview: Vec<crate::layout::WorkspacePreviewOrigin>,
    /// Wake-up timer for the shown banner's expiry deadline (the pinned-clock check in
    /// `advance_animations` is the authority; this only wakes an otherwise idle loop).
    pub notification_banner_timer: Option<RegistrationToken>,
    /// Default audio-sink state (volume + mute) for the panel output indicator and
    /// the QS volume slider; `None` until the PipeWire watcher binds a sink.
    pub audio: Option<crate::audio::AudioStatus>,
    /// Microphone activity (recording + mute) for the panel privacy indicator; default = not
    /// recording (also the value where the PipeWire backend is absent, e.g. headless).
    pub mic: crate::audio::MicStatus,
    /// Output sinks + current default for the QS output-device picker; empty until the PipeWire
    /// watcher reports sinks (also where the backend is absent, e.g. headless).
    pub sink_list: crate::audio::SinkList,
    /// Input sources + current default for the QS input-device picker; empty until the PipeWire
    /// watcher reports sources (also where the backend is absent, e.g. headless).
    pub source_list: crate::audio::SourceList,
    /// Sound cards and their ports (PipeWire routes) — GNOME's port-level view, which the device
    /// pickers and the headphone detection will resolve from. Empty where there is no card, or no
    /// backend at all.
    pub audio_cards: crate::audio::AudioCards,
    /// Whether the default sink is headphones, GNOME's `_hasHeadphones`. `None` = no answer yet,
    /// which is what suppresses the very first OSD — see `State::refresh_headphones`.
    pub headphones: Option<bool>,
    /// Whatever the compositor asks audio to *do* — volume, mute, default device. Live sessions
    /// get `PwAudio` (feature `pipewire`), whose loop is driven on the compositor's calloop;
    /// tests get `StubAudio`. `None` when the connection failed, is disabled, or the run is
    /// headless — every caller must therefore tolerate an absent backend.
    pub audio_backend: Option<Box<dyn crate::audio::AudioBackend>>,
    /// The decoded `org.gnome.desktop.background` picture, drawn as the
    /// workspace background in GNOME windowing mode.
    pub wallpaper: Wallpaper,
    /// Live `org.gnome.Shell` accelerator grabs (gsd-media-keys et al.), in
    /// grab order — the input path takes the first match.
    pub accel_grabs: Vec<AccelGrab>,
    /// Keys currently held that fired a grab, so the release can send
    /// `AcceleratorDeactivated` to the grabber.
    pub accel_grab_release_pending: HashMap<Keycode, u32>,
    /// The next grab's action id (mutter hands out ids past its builtin
    /// keybinding actions and never reuses them; the base is arbitrary).
    pub next_accel_grab_action: u32,
    /// Most recent monotonic time of a real user input event (key or button
    /// press): mutter's `display->last_user_time`, the reference clock for
    /// focus-stealing prevention.
    pub last_user_action_time: Option<Duration>,

    /// Scancodes of the keys to suppress.
    pub suppressed_keys: HashSet<Keycode>,
    /// GNOME "overlay key" candidate: set to the keycode of the overlay key (by
    /// default Super) that was pressed on its own. A matching release with no
    /// intervening input toggles the overview; any other key (or pointer
    /// activity) clears it. Mirrors mutter's `overlay_key_only_pressed`.
    pub overlay_key_armed: Option<Keycode>,
    /// Event time of the last overlay-key tap that actually fired, i.e.
    /// gnome-shell's `_lastOverlayKeyTime` (`overviewControls.js:419`). Only
    /// read when animations are off, where the double-tap escalation into the
    /// app grid falls back to a time comparison.
    pub overlay_key_last_fired: Option<Duration>,
    /// Button codes of the mouse buttons to suppress.
    pub suppressed_buttons: HashSet<u32>,
    /// Which of the overview's widgets a pointer press landed on, with the button
    /// code that pressed it. The overview's controls are St.Buttons, which act on
    /// the release and only if it lands on the same widget, so the press records
    /// the target here and the release consumes it.
    pub overview_pressed: Option<(u32, OverviewHit, Point<f64, Logical>)>,
    /// A press on the app grid's background, which may become a page drag: the button,
    /// the output, where it started and where it last was. `SwipeTracker` attaches a
    /// `Clutter.PanGesture` with `min_n_points: 1` to the grid's scroll view and
    /// `allowDrag` defaults to true (`swipeTracker.js:367-404`), so an ordinary
    /// click-drag pages the grid — the only route there is on a machine with no
    /// touchpad. `dragging` is false until the press clears the drag threshold.
    pub app_grid_pan: Option<AppGridPan>,
    pub bind_cooldown_timers: HashMap<Key, RegistrationToken>,
    pub bind_repeat_timer: Option<RegistrationToken>,
    pub keyboard_focus: KeyboardFocus,
    pub layer_shell_on_demand_focus: Option<LayerSurface>,
    pub idle_inhibiting_surfaces: HashSet<WlSurface>,
    pub is_fdo_idle_inhibited: Arc<AtomicBool>,
    pub keyboard_shortcuts_inhibiting_surfaces: HashMap<WlSurface, KeyboardShortcutsInhibitor>,

    /// Most recent XKB settings from org.freedesktop.locale1.
    pub xkb_from_locale1: Option<Xkb>,

    pub cursor_manager: CursorManager,
    pub cursor_texture_cache: CursorTextureCache,
    pub cursor_shape_manager_state: CursorShapeManagerState,
    pub dnd_icon: Option<DndIcon>,
    /// Contents under pointer.
    ///
    /// Periodically updated: on motion and other events and in the loop callback. If you require
    /// the real up-to-date contents somewhere, it's better to recompute on the spot.
    ///
    /// This is not pointer focus. I.e. during a click grab, the pointer focus remains on the
    /// client with the grab, but this field will keep updating to the latest contents as if no
    /// grab was active.
    ///
    /// This is primarily useful for emitting pointer motion events for surfaces that move
    /// underneath the cursor on their own (i.e. when the tiling layout moves). In this case, not
    /// taking grabs into account is expected, because we pass the information to pointer.motion()
    /// which passes it down through grabs, which decide what to do with it as they see fit.
    pub pointer_contents: PointContents,
    pub pointer_visibility: PointerVisibility,
    pub pointer_inactivity_timer: Option<RegistrationToken>,
    /// Whether the pointer inactivity timer got reset this event loop iteration.
    ///
    /// Used for limiting the reset to once per iteration, so that it's not spammed with high
    /// resolution mice.
    pub pointer_inactivity_timer_got_reset: bool,
    /// Whether the (idle notifier) activity was notified this event loop iteration.
    ///
    /// Used for limiting the notify to once per iteration, so that it's not spammed with high
    /// resolution mice.
    pub notified_activity_this_iteration: bool,
    /// Pressure the pointer has built against a hot corner, and the output whose corner it is.
    /// The corner triggers when the pointer pushes into it, not when it merely touches it
    /// (`PressureBarrier`, `layout.js:1267-1408`).
    pub hot_corner_barrier: Barrier,
    pub hot_corner_output: Option<Output>,
    pub tablet_cursor_location: Option<Point<f64, Logical>>,
    pub gesture_swipe_3f_cumulative: Option<(f64, f64)>,
    pub overview_scroll_swipe_gesture: ScrollSwipeGesture,
    /// The same, for the app grid's own 1:1 page swipe — GNOME gives `AppDisplay` its own
    /// `SwipeTracker` on the grid's scroll view (`appDisplay.js:605-614`), independent of
    /// the overview's.
    pub app_grid_scroll_swipe: ScrollSwipeGesture,
    pub vertical_wheel_tracker: ScrollTracker,
    pub horizontal_wheel_tracker: ScrollTracker,
    pub mods_with_mouse_binds: HashSet<Modifiers>,
    pub mods_with_wheel_binds: HashSet<Modifiers>,
    pub mods_with_tablet_stylus_binds: HashSet<Modifiers>,
    pub vertical_finger_scroll_tracker: ScrollTracker,
    pub horizontal_finger_scroll_tracker: ScrollTracker,
    pub mods_with_finger_scroll_binds: HashSet<Modifiers>,

    pub lock_state: LockState,

    // State that we last sent to the logind LockedHint.
    pub locked_hint: Option<bool>,

    pub screenshot_ui: ScreenshotUi,
    pub hotkey_overlay: HotkeyOverlay,
    pub exit_confirm_dialog: ExitConfirmDialog,
    pub run_dialog: RunDialog,
    pub end_session_dialog: EndSessionDialog,
    /// The polkit "Authentication Required" dialog: what it is asking, and how it is drawn.
    /// Set while the picker is up on behalf of `org.gnome.Shell.Screenshot.SelectArea`: the
    /// caller wants coordinates, not a file. Answered on confirm *and* on every close, because a
    /// D-Bus caller left unanswered blocks until its timeout.
    pub select_area_reply: Option<crate::dbus::gnome_shell_screenshot::SelectAreaReply>,

    /// Set while the picker is up on behalf of `InteractiveScreenshot`: the caller wants the URI
    /// of whatever gets saved. Answered when the save completes, and on every close.
    pub interactive_screenshot_reply: Option<crate::dbus::gnome_shell_screenshot::InteractiveReply>,

    /// A capture armed to fire after the picker's delay has run out. **Our divergence** — GNOME's
    /// shell screenshot UI has no delay (`docs/fork/screenshot-ui-port.md`).
    ///
    /// It lives here rather than in `ScreenshotUi::Open` because it *outlives* it: arming
    /// dismisses the picker so the delay can do its job, and the shot is taken from the live
    /// screen afterwards.
    pub pending_capture: Option<PendingCapture>,
    /// The countdown card the pending capture draws — Output target only, never in a capture.
    pub capture_countdown: crate::ui::screenshot_ui::Countdown,
    /// The shade marking the area a picker-started recording is capturing, while it runs.
    pub cast_area_indicator: CastAreaIndicator,

    /// The screenshot flash (`org.gnome.Shell.Screenshot.FlashArea`).
    pub flashspot: crate::ui::flashspot::FlashSpot,
    /// The hot corner's ripple.
    pub ripples: crate::ui::ripples::Ripples,
    /// The dash as a bottom-edge dock (a divergence — see [`crate::ui::dock`]).
    pub dock: crate::ui::dock::Dock,
    pub polkit_dialog: crate::polkit_dialog::PolkitDialog,
    pub polkit_ui: crate::ui::polkit_dialog::PolkitDialogUi,
    /// The agent's side of the conversation.
    pub polkit_requests: Option<async_channel::Sender<crate::dbus::polkit_agent::PolkitRequest>>,
    /// A request that arrived while the screen was locked.
    ///
    /// GNOME does not prompt over a lock screen: it waits for the session mode to change and
    /// re-runs the request then (`polkitAgent.js:439-450`). Anything else would either put a
    /// password box on top of the shield or answer polkitd without asking anyone.
    pub polkit_deferred: Option<Box<crate::dbus::polkit_agent::BeginRequest>>,
    /// The delayed entry reset — see [`crate::polkit_dialog::DELAYED_RESET`].
    pub polkit_reset_timer: Option<calloop::RegistrationToken>,
    pub panel: Panel,
    pub panel_popover: PanelPopover,
    /// Whether the overview was open at the last render-elements update, to
    /// edge-detect the closed→open transition (which dismisses an open panel
    /// popover, like GNOME's overview modal breaking the menu's grab).
    overview_was_open: bool,
    /// The overview dash (favorites bar).
    pub dash: Dash,
    /// The overview search (entry + app results).
    pub overview_search: OverviewSearch,
    /// The overview app grid (installed apps minus favorites).
    pub app_grid: AppGrid,
    /// The app-folder dialog a click on a folder tile opens.
    pub folder_dialog: crate::ui::folder_dialog::FolderDialog,
    /// GPU caches for the window picker's per-preview chrome (the close button).
    pub preview_chrome: PreviewChrome,
    /// The preview whose close button the pointer is on, for its hover fill.
    pub preview_close_hovered: Option<Window>,
    /// GPU caches for the strip's per-thumbnail close button.
    pub thumbnail_chrome: ThumbnailChrome,
    /// The strip thumbnail the pointer is on: an empty workspace shows its close button
    /// while hovered (divergence, `docs/fork/dynamic-workspaces-divergence.md`).
    pub thumbnail_hovered: Option<WorkspaceId>,
    /// …and the workspace whose close button it is *on*, for that button's hover fill.
    pub thumbnail_close_hovered: Option<WorkspaceId>,
    /// An app icon being dragged onto a workspace — see [`AppDrag`].
    pub app_drag: Option<AppDrag>,
    /// The icon whose context menu is open, so it can keep its highlight for as long as
    /// the menu is (`setForcedHighlight`, `appDisplay.js:3028`). Stale once the menu
    /// closes — read only while `panel_popover.is_app_menu()`.
    pub app_menu_source: Option<crate::input::OverviewHit>,
    /// Whether the open app menu was opened off the *dock* rather than the overview's dash.
    ///
    /// gnome-shell closes an app icon's menu when the overview hides (`appDisplay.js:3039-3040`),
    /// which is what the dock — our divergence — walks straight into: it shows the same dash with
    /// the overview shut, so that rule closed every dock menu on the very next frame.
    pub app_menu_from_dock: bool,
    /// The one GPU upload map every app-icon surface draws from — the dash, the grid, the
    /// open folder, the search results and the drag proxy. Held here so the drag proxy,
    /// which has no surface of its own, can reach it.
    pub app_icon_uploads: crate::ui::widget::SharedAppIconUploads,
    /// The raised fill under a dragged *folder*'s composed icon. Held rather than rebuilt
    /// per frame so the element keeps its identity as the drag moves.
    app_drag_bg: RefCell<crate::render_helpers::rounded_solid::RoundedSolidBuffer>,
    /// The grid slot a drag is hovering, waiting out `DELAYED_MOVE_MS` before the grid
    /// reflows around it (`_delayedMoveData`, `appDisplay.js:768-825`), with the page
    /// size the target was resolved against.
    pub grid_pending_move: Option<(crate::ui::app_grid::GridDropTarget, usize)>,
    /// The timer arming that move; dropped and re-armed whenever the target changes.
    pub grid_move_timer: Option<RegistrationToken>,
    /// The tile a drag is resting on that would take the drop *into* it — an absolute
    /// entry index. Onto a folder that is a join and lights up at once; onto an app it is
    /// an offer to make a folder, and `grid_drop_timer` counts out [`FOLDER_PREVIEW_MS`]
    /// before it shows.
    pub grid_drop_target: Option<usize>,
    pub grid_drop_timer: Option<RegistrationToken>,
    /// Counts out `POPDOWN_DIALOG_TIMEOUT` while a drag out of the open folder dialog is
    /// outside its panel; when it fires the dialog pops down and the drag carries on over
    /// the grid (`_setupPopdownTimeout`, `appDisplay.js:2832-2841`).
    pub folder_popdown_timer: Option<RegistrationToken>,
    /// The same delayed move as `grid_pending_move`, but among the *open folder's* members:
    /// `FolderView` inherits `BaseAppView._maybeMoveItem`, so a drag inside the dialog
    /// reorders the folder on exactly the same terms.
    pub folder_pending_move: Option<(crate::ui::app_grid::GridDropTarget, usize)>,
    pub folder_move_timer: Option<RegistrationToken>,
    /// The timer that flips the grid's page while a drag hovers a preview band or leans
    /// on the screen edge (`appDisplay.js:827-921`) — first the initial delay, then the
    /// repeat.
    pub grid_page_switch_timer: Option<RegistrationToken>,
    /// Where the pointer was when an edge bump last fired, so leaning on the edge
    /// switches once rather than continuously (`_lastOvershootCoord`).
    pub grid_page_switch_overshoot: Option<f64>,
    /// When the app grid last flipped a page on a wheel notch, to debounce a fast
    /// spin (`SCROLL_TIMEOUT_TIME`=150ms, `appDisplay.js:696-701`).
    pub app_grid_last_page_flip: Option<Duration>,
    /// Tracks the overview-UI visibility rising edge (closed→open, unlock/screenshot
    /// close while open) so the search resets on a fresh open, matching GNOME's
    /// reset-on-enter — see [`Synoik::refresh_overview_search_state`].
    overview_search_was_visible: bool,
    /// gnome-shell's search cross-fade (`_onSearchChanged`, `overviewControls.js:609-643`):
    /// 0 while the window picker is fully shown, 1 while a search covers it.
    overview_search_fade: Option<Animation>,
    /// The settled target of [`Self::overview_search_fade`], for edge detection.
    overview_search_fade_target: bool,
    /// The search entry's grow/shrink between its resting puck and GNOME's full pill.
    /// Separate from the cross-fade above, which is about the *results* covering the
    /// picker: the entry opens on a click with no query at all.
    overview_search_expand: Option<Animation>,
    /// The settled target of [`Self::overview_search_expand`], for edge detection.
    overview_search_expand_target: bool,
    /// Offscreens for the cross-fade. The picker and the thumbnails are each faded
    /// as a *group*: a per-element alpha would double-darken wherever previews
    /// overlap, which the group composite avoids.
    picker_offscreen: OffscreenBuffer,
    thumbnails_offscreen: OffscreenBuffer,
    /// Shared symbolic-icon cache for the panel and its popovers.
    pub icon_cache: IconCache,
    /// Request sink for the symbolic-icon worker. Held here rather than only inside `icon_cache`
    /// because an icon-theme change replaces that cache and the worker outlives it.
    pub symbolic_icon_tx:
        Option<std::sync::mpsc::Sender<crate::render_helpers::icon::SymbolicRequest>>,
    /// Full-color application-icon loader for the dash / app grid / search.
    pub app_icon_cache: AppIconCache,
    /// Images an app pointed us at (album art). Separate from `app_icon_cache`: its own worker, so
    /// a slow cover fetch can never queue in front of the dash's icons.
    pub image_cache: ImageCache,

    /// The GNOME Alt-Tab / Super-Tab switcher (`ui::switcher`).
    pub switcher: crate::ui::switcher::ui::SwitcherUi,

    pub pick_window: Option<async_channel::Sender<Option<MappedId>>>,
    pub pick_color: Option<async_channel::Sender<Option<synoik_ipc::PickedColor>>>,

    pub debug_draw_opaque_regions: bool,
    pub debug_draw_damage: bool,
    /// One-shot: dump the scanned-out framebuffer to a PNG after the next frame is rendered.
    /// Set by `Action::DebugDumpScanout`, taken by the tty backend.
    pub dump_scanout_next_frame: bool,

    /// Frame-timing instrumentation, off unless `SYNOIK_FRAME_LOG` says otherwise.
    /// See [`crate::frame_log`].
    pub frame_log: FrameLog,

    pub dbus: Option<crate::dbus::DBusServers>,
    pub a11y_keyboard_monitor: Option<crate::dbus::freedesktop_a11y::KeyboardMonitor>,
    pub a11y: A11y,
    pub inhibit_power_key_fd: Option<zbus::zvariant::OwnedFd>,

    pub ipc_server: Option<IpcServer>,
    pub ipc_outputs_changed: bool,

    /// Per-connector display settings applied live this session, keyed by connector.
    ///
    /// This is our equivalent of mutter's "current" monitors config
    /// (`meta_monitor_config_manager_set_current`, meta-monitor-manager.c
    /// `meta_monitor_manager_apply_monitors_config`): a config applied via
    /// `org.gnome.Mutter.DisplayConfig` `ApplyMonitorsConfig` (GNOME Settings), `synoik msg
    /// output`, or wlr-output-management takes effect immediately and outranks the
    /// `monitors.xml` store, which is only *written* for persistence (and read at
    /// startup/hotplug) — never re-read to override a live apply.
    pub applied_display_config: HashMap<String, AppliedDisplayConfig>,

    pub satellite: Option<Satellite>,

    #[cfg(feature = "xdp-gnome-screencast")]
    pub casting: Screencasting,

    /// Where a relative recording template lands, overriding the XDG Videos directory. `None` in
    /// production; the test fixture points it at a scratch directory, because these paths reach
    /// the real filesystem and a test driving the real capture button would otherwise leave a
    /// recording in the developer's own `~/Videos/Screencasts` on every suite run.
    ///
    /// Here rather than on [`Screencasting`], which is `xdp-gnome-screencast`-gated: this governs
    /// the *native* recorder, which is not. It lived there until the no-default-features build
    /// broke on reading it — and gating the reads instead would have been worse than the build
    /// error, since ghost provisions synoik with that exact feature set and runs its suite against
    /// it, which is the litter this field exists to prevent.
    pub recordings_base: Option<std::path::PathBuf>,
}

/// A capture armed to fire once its delay has run out. See [`State::arm_delayed_capture`].
///
/// Deliberately *not* a field of `ScreenshotUi::Open`: arming closes the picker, so this outlives
/// it. The output is held weakly so an unplug during the countdown cancels rather than resurrects.
pub struct PendingCapture {
    output: WeakOutput,
    action: PendingAction,
    /// Monotonic; the countdown reads it, and it — not the tick count — decides when to fire.
    fires_at: Duration,
    token: RegistrationToken,
}

/// What the countdown ends in. The picker's shot/cast control decides which, and the two share
/// every cancellation rule — a lock or a vanished output must stop a recording from starting just
/// as firmly as it stops a photograph being taken.
enum PendingAction {
    Shot {
        target: PendingTarget,
        show_pointer: bool,
        write_to_disk: bool,
        path: Option<String>,
        /// The `InteractiveScreenshot` caller still waiting, lifted out of `Synoik` as this armed
        /// so the close would not answer it with a dismissal.
        reply: Option<crate::dbus::gnome_shell_screenshot::InteractiveReply>,
    },
    Cast {
        /// Global-logical, or `None` for the whole output.
        crop: Option<Rectangle<i32, Logical>>,
        draw_cursor: bool,
    },
}

impl PendingAction {
    /// Answer whatever caller this action was holding with a dismissal. A cast holds none.
    fn dismiss(self) {
        if let Self::Shot {
            reply: Some(tx), ..
        } = self
        {
            let _ = tx.send_blocking(None);
        }
    }
}

impl PendingCapture {
    /// Whole seconds still to wait, rounded up: the number the countdown card shows.
    pub fn seconds_left(&self, now: Duration) -> u64 {
        self.fires_at.saturating_sub(now).as_secs_f64().ceil() as u64
    }
}

/// A scale/transform override applied live this session (see
/// [`Synoik::applied_display_config`]). Only the fields the `monitors.xml` store also covers need
/// overriding; mode/position/off flow through the regular output config and are never overridden
/// by the store.
#[derive(Debug, Clone, Copy, Default)]
pub struct AppliedDisplayConfig {
    pub scale: Option<f64>,
    pub transform: Option<Transform>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PointerVisibility {
    /// The pointer is visible.
    Visible,
    /// The pointer is invisible, but retains its focus.
    ///
    /// This state is set temporarily after auto-hiding the pointer to keep tooltips open and grabs
    /// ongoing.
    Hidden,
    /// The pointer is invisible and cannot focus.
    ///
    /// Corresponds to a fully disabled pointer, for example after a touchscreen input, or after
    /// the pointer contents changed in a Hidden state.
    Disabled,
}

impl PointerVisibility {
    pub fn is_visible(&self) -> bool {
        matches!(self, Self::Visible)
    }
}

#[derive(Debug)]
pub struct DndIcon {
    pub surface: WlSurface,
    pub offset: Point<i32, Logical>,
}

pub struct OutputState {
    pub global: GlobalId,
    pub frame_clock: FrameClock,
    pub redraw_state: RedrawState,
    /// Set by a fired dispatch deadline, taken by the redraw it was armed for.
    ///
    /// The aim belongs to the *decision*, not to the moment the redraw happens: recomputing the
    /// target after waiting until `boundary − estimate` would land one microsecond past the point
    /// where [`FrameClock::next_presentation_time`] gives up on that vblank, so every
    /// deadline-dispatched frame would aim a cycle late.
    pub pending_aim: Option<FrameAim>,
    pub on_demand_vrr_enabled: bool,
    // After the last redraw, some ongoing animations still remain.
    pub unfinished_animations_remain: bool,
    /// Last sequence received in a vblank event.
    pub last_drm_sequence: Option<u32>,
    pub vblank_throttle: VBlankThrottle,
    /// Sequence for frame callback throttling.
    ///
    /// We want to send frame callbacks for each surface at most once per monitor refresh cycle.
    ///
    /// Even if a surface commit resulted in empty damage to the monitor, we want to delay the next
    /// frame callback until roughly when a VBlank would occur, had the monitor been damaged. This
    /// is necessary to prevent clients busy-looping with frame callbacks that result in empty
    /// damage.
    ///
    /// This counter wrapping-increments by 1 every time we move into the next refresh cycle, as
    /// far as frame callback throttling is concerned. Specifically, it happens:
    ///
    /// 1. Upon a successful DRM frame submission. Notably, we don't wait for the VBlank here,
    ///    because the client buffers are already "latched" at the point of submission. Even if a
    ///    client submits a new buffer right away, we will wait for a VBlank to draw it, which
    ///    means that busy looping is avoided.
    /// 2. If a frame resulted in empty damage, a timer is queued to fire roughly when a VBlank
    ///    would occur, based on the last presentation time and output refresh interval. Sequence
    ///    is incremented in that timer, before attempting a redraw or sending frame callbacks.
    pub frame_callback_sequence: u32,
    /// Solid color buffer for the backdrop that we use instead of clearing to avoid damage
    /// tracking issues and make screenshots easier.
    pub backdrop_buffer: SolidColorBuffer,
    pub xray: Xray,
    pub lock_render_state: LockRenderState,
    pub lock_surface: Option<LockSurface>,
    pub lock_color_buffer: SolidColorBuffer,
    /// The shield's dim, laid over the wallpaper — `BLUR_BRIGHTNESS` as a black wash.
    pub shield_dim_buffer: SolidColorBuffer,
    /// Opaque black under it, for the case where there is no wallpaper to dim: without it the
    /// wash is translucent black over the desktop, which is the one way the shield could fail
    /// open.
    pub shield_backstop_buffer: SolidColorBuffer,
    pub screen_transition: Option<ScreenTransition>,
    /// Damage tracker used for the debug damage visualization.
    pub debug_damage_tracker: OutputDamageTracker,
    /// How many render elements the last frame carried, and whether it had to
    /// redraw the whole output. Recorded by the render path for
    /// [`crate::frame_log`], which reports them alongside a slow frame.
    pub last_frame_elements: usize,
    pub last_frame_full_damage: bool,
    /// How the last frame's elements were presented, from smithay's `RenderElementStates`.
    ///
    /// This is the **authoritative** answer to "did direct scan-out engage", and the reason it is
    /// recorded rather than inferred: the DRM debugfs `imported=` field does not mean what it
    /// looks like (our own PRIME-imported scanout buffers report `imported=no`), and reading
    /// scan-out off it produced a confidently wrong answer on 2026-08-15.
    pub last_frame_scanout: ScanoutTally,
    /// Which animations were running when the last frame was built. The set the
    /// redraw loop derives `unfinished_animations_remain` from, kept so the frame
    /// log can name what a slow frame was doing.
    pub last_frame_anim_causes: AnimCauses,
    /// The frame this output last handed to its backend shows the settled shield curtain, and its
    /// presentation is what a pending suspend is waiting on. Taken by whichever moment the backend
    /// calls "presented" — a vblank on the TTY, the render call itself headless.
    pub shield_frame_queued: bool,
}

/// Which Wayland socket, if any, the compositor should listen on.
#[derive(Debug, Clone)]
pub enum WaylandSocket {
    /// Do not listen at all — the in-process test fixture, which speaks to its clients over a
    /// socketpair and must not appear in `XDG_RUNTIME_DIR` for anything else to find.
    None,
    /// The first free `wayland-N`.
    Auto,
    /// Exactly this name, and fail if it is taken (`--wayland-display`). A rig that names its
    /// socket knows where to point its clients without scraping the log for the name we picked.
    Named(String),
}

#[derive(Debug, Default)]
pub enum RedrawState {
    /// The compositor is idle.
    #[default]
    Idle,
    /// A redraw is queued.
    Queued,
    /// We submitted a frame to the KMS and waiting for it to be presented.
    WaitingForVBlank { redraw_needed: bool },
    /// A redraw is due, and deliberately held until its dispatch deadline (see
    /// [`FrameClock::next_dispatch`]). The timer fires at `aim.target − estimated cost`.
    ScheduledDispatch {
        token: RegistrationToken,
        aim: FrameAim,
    },
    /// We did not submit anything to KMS and made a timer to fire at the estimated VBlank.
    WaitingForEstimatedVBlank(RegistrationToken),
    /// A redraw is queued on top of the above.
    WaitingForEstimatedVBlankAndQueued(RegistrationToken),
}

impl RedrawState {
    /// The variant's name, for `synoik msg debug-focus-state`.
    ///
    /// A frozen screen with a live event loop is one of these stuck: `WaitingForVBlank` means a
    /// page flip was accepted and its completion never came back, while an estimated-vblank
    /// variant means we are on a timer instead. They are indistinguishable from outside the
    /// process and need opposite fixes, so name which one it is.
    pub fn debug_name(&self) -> &'static str {
        match self {
            RedrawState::Idle => "Idle",
            RedrawState::Queued => "Queued",
            RedrawState::WaitingForVBlank {
                redraw_needed: false,
            } => "WaitingForVBlank",
            RedrawState::WaitingForVBlank {
                redraw_needed: true,
            } => "WaitingForVBlank(redraw needed)",
            RedrawState::ScheduledDispatch { .. } => "ScheduledDispatch",
            RedrawState::WaitingForEstimatedVBlank(_) => "WaitingForEstimatedVBlank",
            RedrawState::WaitingForEstimatedVBlankAndQueued(_) => {
                "WaitingForEstimatedVBlankAndQueued"
            }
        }
    }
}

/// How one frame's elements were presented — zero-copy scan-out, or rendered and why.
#[derive(Debug, Default, Clone, Copy)]
pub struct ScanoutTally {
    /// Elements handed to a plane directly, no compositing.
    pub zero_copy: usize,
    /// Rendered because the buffer's format cannot be scanned out.
    pub format_unsupported: usize,
    /// Rendered because scan-out was *attempted and failed* — the interesting one.
    pub scanout_failed: usize,
    /// Rendered with no reason recorded (never a scan-out candidate).
    pub rendered: usize,
    /// Not visible this frame.
    pub skipped: usize,
}

/// What a redraw is aiming at, decided before the redraw starts.
#[derive(Debug, Clone, Copy)]
pub struct FrameAim {
    /// The presentation time this frame's content is sampled for, and the vblank it is racing.
    pub target: Duration,
    /// When this frame was *supposed* to start, if it was deadline-dispatched.
    ///
    /// Everything from here to the redraw actually starting is charged to the frame's measured
    /// cost, so a loop that gets descheduled — or merely spends a long turn on input and reconcile
    /// between the timer firing and the drain — raises the render-time estimate and dispatches the
    /// next frames earlier, instead of silently eating the margin and missing vblanks forever.
    /// Mutter folds dispatch lateness in the same way
    /// (`clutter/clutter/clutter-frame-clock.c:600-607`).
    ///
    /// The invariant is that the recorded cost spans `scheduled_at` to handed-to-KMS, because that
    /// is exactly the span `at + estimate + constant == vblank` was solved for.
    pub scheduled_at: Option<Duration>,
}

impl FrameAim {
    /// Aim at `target` with nothing owed — a redraw that was not deadline-dispatched.
    fn immediate(target: Duration) -> Self {
        Self {
            target,
            scheduled_at: None,
        }
    }
}

pub struct PopupGrabState {
    pub root: WlSurface,
    pub grab: PopupGrab<State>,
    pub has_keyboard_grab: bool,
}

// The surfaces here are always toplevel surfaces focused as far as synoik's logic is concerned,
// even when popup grabs are active (which means the real keyboard focus is on a popup descending
// from that toplevel surface).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyboardFocus {
    // Layout is focused by default if there's nothing else to focus.
    Layout {
        surface: Option<WlSurface>,
    },
    LayerShell {
        surface: WlSurface,
    },
    LockScreen {
        surface: Option<WlSurface>,
    },
    ScreenshotUi,
    ExitConfirmDialog,
    RunDialog,
    EndSessionDialog,
    /// The polkit authentication dialog, which is modal like the three above it.
    PolkitDialog,
    Popover,
    Overview,
    /// The Alt-Tab / Super-Tab switcher holds a modal grab while it is up.
    Switcher,
}

#[derive(Default, Clone, PartialEq)]
pub struct PointContents {
    // Output under point.
    pub output: Option<Output>,
    // Surface under point and its location in the global coordinate space.
    //
    // Can be `None` even when `window` is set, for example when the pointer is over the niri
    // border around the window.
    pub surface: Option<(WlSurface, Point<f64, Logical>)>,
    // If surface belongs to a window, this is that window.
    pub window: Option<(Window, HitType)>,
    // If surface belongs to a layer surface, this is that layer surface.
    pub layer: Option<LayerSurface>,
}

#[derive(Debug, Default)]
pub enum LockState {
    #[default]
    Unlocked,
    WaitingForSurfaces {
        confirmation: SessionLocker,
        deadline_token: RegistrationToken,
    },
    Locking(SessionLocker),
    Locked(ExtSessionLockV1),
}

#[derive(PartialEq, Eq)]
pub enum LockRenderState {
    /// The output displays a normal session frame.
    Unlocked,
    /// The output displays a locked frame.
    Locked,
}

// Not related to the one in Smithay.
//
// This state keeps track of when a surface last received a frame callback.
struct SurfaceFrameThrottlingState {
    /// Output and sequence that the frame callback was last sent at.
    last_sent_at: RefCell<Option<(Output, u32)>>,
}

pub enum CenterCoords {
    Separately,
    Both,
    // Force centering even if the cursor is already in the rectangle.
    BothAlways,
}

#[derive(Clone, PartialEq, Eq)]
pub enum CastTarget {
    // Dynamic cast before selecting anything.
    Nothing,
    Output {
        output: WeakOutput,
        /// Cached name of the output.
        name: String,
    },
    Window {
        id: u64,
    },
    /// A rectangular sub-region of the stage.
    ///
    /// Resolved at cast start to the single output with the largest intersection with `rect`
    /// (mutter composites all intersecting views; cross-output area casts are not yet supported).
    Area {
        output: WeakOutput,
        /// Cached name of the resolved output.
        name: String,
        /// Recorded region, in global logical coordinates.
        rect: Rectangle<i32, Logical>,
    },
}

impl CastTarget {
    pub fn output(output: &Output) -> Self {
        Self::Output {
            output: output.downgrade(),
            name: output.name(),
        }
    }

    pub fn matches_output(&self, weak: &WeakOutput) -> bool {
        matches!(
            self,
            CastTarget::Output { output, .. } | CastTarget::Area { output, .. } if output == weak
        )
    }

    pub fn matches(&self, ipc: &synoik_ipc::CastTarget) -> bool {
        use CastTarget::*;
        match (self, ipc) {
            (Nothing, synoik_ipc::CastTarget::Nothing {}) => true,
            (Output { name, .. }, synoik_ipc::CastTarget::Output { name: ipc_name }) => {
                name == ipc_name
            }
            (Window { id }, synoik_ipc::CastTarget::Window { id: ipc_id }) => id == ipc_id,
            _ => false,
        }
    }

    pub fn make_ipc(&self) -> synoik_ipc::CastTarget {
        use CastTarget::*;
        match self {
            Nothing => synoik_ipc::CastTarget::Nothing {},
            Output { name, .. } => synoik_ipc::CastTarget::Output { name: name.clone() },
            Window { id } => synoik_ipc::CastTarget::Window { id: *id },
            // Area casts are never dynamic-target, so this is never queried over IPC; report the
            // resolved output (there is no IPC area variant) rather than inventing one.
            Area { name, .. } => synoik_ipc::CastTarget::Output { name: name.clone() },
        }
    }
}

impl RedrawState {
    fn queue_redraw(self) -> Self {
        match self {
            RedrawState::Idle => RedrawState::Queued,
            RedrawState::WaitingForEstimatedVBlank(token) => {
                RedrawState::WaitingForEstimatedVBlankAndQueued(token)
            }

            // A redraw is already queued. `ScheduledDispatch` included: the damage that just
            // arrived is covered by the frame that deadline is holding, and re-arming the timer
            // on every commit would keep pushing the dispatch out.
            value @ (RedrawState::Queued
            | RedrawState::ScheduledDispatch { .. }
            | RedrawState::WaitingForEstimatedVBlankAndQueued(_)) => value,

            // We're waiting for VBlank, request a redraw afterwards.
            RedrawState::WaitingForVBlank { .. } => RedrawState::WaitingForVBlank {
                redraw_needed: true,
            },
        }
    }
}

impl Default for SurfaceFrameThrottlingState {
    fn default() -> Self {
        Self {
            last_sent_at: RefCell::new(None),
        }
    }
}

impl KeyboardFocus {
    /// The variant's name, plus whether it carries a surface, for debugging.
    ///
    /// `Layout(none)` is the interesting one: it is what focus falls back to when nothing can
    /// take it, and it is indistinguishable from a focused window in every other readout.
    pub fn debug_name(&self) -> String {
        let name = match self {
            KeyboardFocus::Layout { .. } => "Layout",
            KeyboardFocus::LayerShell { .. } => "LayerShell",
            KeyboardFocus::LockScreen { .. } => "LockScreen",
            KeyboardFocus::ScreenshotUi => "ScreenshotUi",
            KeyboardFocus::ExitConfirmDialog => "ExitConfirmDialog",
            KeyboardFocus::RunDialog => "RunDialog",
            KeyboardFocus::EndSessionDialog => "EndSessionDialog",
            KeyboardFocus::PolkitDialog => "PolkitDialog",
            KeyboardFocus::Popover => "Popover",
            KeyboardFocus::Overview => "Overview",
            KeyboardFocus::Switcher => "Switcher",
        };
        match self {
            KeyboardFocus::Layout { .. }
            | KeyboardFocus::LayerShell { .. }
            | KeyboardFocus::LockScreen { .. } => {
                let surface = if self.surface().is_some() {
                    "surface"
                } else {
                    "none"
                };
                format!("{name}({surface})")
            }
            _ => name.to_owned(),
        }
    }

    pub fn surface(&self) -> Option<&WlSurface> {
        match self {
            KeyboardFocus::Layout { surface } => surface.as_ref(),
            KeyboardFocus::LayerShell { surface } => Some(surface),
            KeyboardFocus::LockScreen { surface } => surface.as_ref(),
            KeyboardFocus::ScreenshotUi => None,
            KeyboardFocus::ExitConfirmDialog => None,
            KeyboardFocus::RunDialog => None,
            KeyboardFocus::EndSessionDialog => None,
            KeyboardFocus::PolkitDialog => None,
            KeyboardFocus::Popover => None,
            KeyboardFocus::Overview => None,
            KeyboardFocus::Switcher => None,
        }
    }

    pub fn into_surface(self) -> Option<WlSurface> {
        match self {
            KeyboardFocus::Layout { surface } => surface,
            KeyboardFocus::LayerShell { surface } => Some(surface),
            KeyboardFocus::LockScreen { surface } => surface,
            KeyboardFocus::ScreenshotUi => None,
            KeyboardFocus::ExitConfirmDialog => None,
            KeyboardFocus::RunDialog => None,
            KeyboardFocus::EndSessionDialog => None,
            KeyboardFocus::PolkitDialog => None,
            KeyboardFocus::Popover => None,
            KeyboardFocus::Overview => None,
            KeyboardFocus::Switcher => None,
        }
    }

    pub fn is_layout(&self) -> bool {
        matches!(self, KeyboardFocus::Layout { .. })
    }

    pub fn is_overview(&self) -> bool {
        matches!(self, KeyboardFocus::Overview)
    }
}

pub struct State {
    pub backend: Backend,
    pub synoik: Synoik,
}

impl State {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: Config,
        event_loop: LoopHandle<'static, State>,
        stop_signal: LoopSignal,
        display: Display<State>,
        mode: BackendMode,
        wayland_socket: WaylandSocket,
        is_session_instance: bool,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let _span = tracy_client::span!("State::new");

        let config = Rc::new(RefCell::new(config));

        let mut backend = match mode {
            BackendMode::Headless | BackendMode::HeadlessTest => Backend::Headless(Headless::new()),
            // `Auto` used to pick the winit backend when nested inside a session. Winit was
            // EGL/GLES to the bone (Smithay's `WinitGraphicsBackend` needs
            // `Bind<EGLSurface>`), so it went with GLES; nested mode returns as a
            // Wayland-client backend. Auto is TTY-only for now, and running inside a
            // session fails in TTY init rather than silently nesting.
            BackendMode::Auto => {
                let tty = Tty::new(config.clone(), event_loop.clone())
                    .context("error initializing the TTY backend")?;
                Backend::Tty(tty)
            }
        };

        let mut synoik = Synoik::new(
            config.clone(),
            event_loop,
            stop_signal,
            display,
            &backend,
            wayland_socket,
            is_session_instance,
        );
        backend.init(&mut synoik);

        let mut state = Self { backend, synoik };

        // Pull in the GNOME settings we honor (overlay-key, …) from the live
        // GSettings/dconf backend — the same store gnome-shell uses — and keep the
        // model current as keys change. Headless test instances keep the defaults
        // and drive the model directly instead.
        // A headless test instance keeps the in-memory session store it was built with: the suite
        // must neither read nor clobber the real session file.
        if mode != BackendMode::HeadlessTest {
            match crate::session_state::default_path() {
                Some(path) => {
                    let (store, err) = crate::session_state::SessionStore::load(path);
                    if let Some(err) = err {
                        warn!("error loading the session store, starting empty: {err}");
                    }
                    state.synoik.session_manager_state.store = store;
                }
                None => warn!("no data directory; sessions will not persist"),
            }
        }

        if mode != BackendMode::HeadlessTest {
            let (initial, rx, writer) = crate::gnome::load_and_watch_gsettings();
            state.synoik.gnome_settings = initial;
            state.synoik.refresh_keybinding_state();
            // Before anything can animate: `enable-animations` decides whether the clock completes
            // instantly, and the clock was built from the config alone (no settings existed yet).
            state.refresh_animation_clock();
            // The libinput device settings and the key-repeat parameters (see
            // `crate::input::peripherals`). Devices come and go later, and each one is
            // configured as it appears through the same `apply_libinput_settings`.
            state.apply_peripherals();
            state
                .synoik
                .screen_shield
                .set_settings(state.synoik.gnome_settings.shield);
            state
                .synoik
                .unlock_dialog
                .set_peek_locked_down(state.synoik.gnome_settings.shield.disable_show_password);
            // Publish the realized base font before anything measures text: every point
            // size in the UI is a ratio against it (`crate::ui::base_font_pt`), and every
            // shaped advance comes from the family.
            crate::ui::set_base_font_pt(state.synoik.gnome_settings.base_font_pt);
            synoik_vk::text::set_sans_family(&state.synoik.gnome_settings.base_font_family);
            state.synoik.last_power_profile =
                state.synoik.gnome_settings.last_power_profile.clone();
            state.synoik.gnome_settings_writer = Some(writer);
            // Seed both icon caches from the configured icon theme (they default
            // to Adwaita pre-settings).
            let icon_theme = state.synoik.gnome_settings.icon_theme.clone();
            state.synoik.replace_icon_cache(icon_theme.as_str());
            state.synoik.app_icon_cache.set_theme(&icon_theme);
            // GNOME's input-sources own the keymap when present, overriding the
            // synoik-config keymap the seat keyboard was created with.
            if state.synoik.gnome_settings.input_sources.present {
                state.apply_effective_xkb();
            }
            state
                .synoik
                .layout
                .set_gnome_edge_tiling(state.synoik.gnome_settings.edge_tiling);
            state
                .synoik
                .layout
                .set_gnome_center_new_windows(state.synoik.gnome_settings.center_new_windows);
            state
                .synoik
                .layout
                .set_gnome_auto_maximize(state.synoik.gnome_settings.auto_maximize);
            // The application catalog (dash favorites, overview search, launch),
            // seeded from the current `favorite-apps` and refreshed on
            // `installed-changed`.
            let (app_system, app_db_rx) = crate::app_system::AppSystem::new_gio();
            state.synoik.app_system = app_system;
            state
                .synoik
                .app_system
                .set_favorites(state.synoik.gnome_settings.favorite_apps.clone());
            state.synoik.sync_dash_favorites();
            // Seed the default app folders if this profile has never had any —
            // gnome-shell does it once from `AppDisplay._init` (`appDisplay.js:1349`),
            // and it needs the catalog because the default lists are filtered to what
            // is installed. The write comes back through the settings watcher, which
            // re-syncs the grid; `sync_app_grid` below just shows what we have now.
            if let Some(writer) = &state.synoik.gnome_settings_writer {
                let app_system = &state.synoik.app_system;
                writer.ensure_default_folders(crate::gnome::default_folders(|id| {
                    app_system.lookup(id).is_some()
                }));
            }
            state.synoik.sync_app_grid();
            state
                .synoik
                .event_loop
                .insert_source(app_db_rx, |event, _, state| {
                    if let calloop::channel::Event::Msg(()) = event {
                        state.synoik.queue_app_catalog_reload();
                    }
                })
                .unwrap();
            // Decode app icons on a worker thread — the overview app grid shows ~24
            // at once, and rasterizing them inline on the first frame stutters the
            // open animation. Finished decodes land back here and queue a redraw.
            let (icon_tx, icon_rx) = calloop::channel::channel();
            state.synoik.app_icon_cache.spawn_worker(icon_tx);
            state
                .synoik
                .event_loop
                .insert_source(icon_rx, |event, _, state| {
                    if let calloop::channel::Event::Msg(decoded) = event {
                        if let Some((icon, logical, _)) =
                            state.synoik.app_icon_cache.apply_decoded(decoded)
                        {
                            state.synoik.drop_app_icon_uploads(&icon, logical);
                            state.synoik.queue_redraw_all();
                        }
                    }
                })
                .unwrap();

            // **Warm the icons here, not on the first output.** `add_output` also asks, but on a
            // TTY seat it runs from `backend.init` — long before this worker exists — so
            // `prewarm_app_icons` hits its own no-worker guard and returns having warmed nothing.
            // The grid then decoded every tile lazily on the frame it first appeared, which is
            // what "the app grid icons sometimes flicker on first open" was; *sometimes*, because
            // any later settings change, output resize or catalog reload would warm it first and
            // hide the bug. The grid is populated by `sync_app_grid` above, so by here there is
            // something to warm.
            state.synoik.prewarm_app_icons();

            // Images an app pointed us at (album art) get their OWN worker, not the app-icon
            // one: a remote cover can block for the whole fetch timeout, and the dash and app
            // grid must never queue behind a slow or dead cover server.
            let (img_tx, img_rx) = calloop::channel::channel();
            state.synoik.image_cache.spawn_worker(img_tx);
            state
                .synoik
                .event_loop
                .insert_source(img_rx, |event, _, state| {
                    if let calloop::channel::Event::Msg(loaded) = event {
                        if let Some(source) = state.synoik.image_cache.apply_loaded(loaded) {
                            // A load landing is a *content* change for the surface showing it, not
                            // just a new texture: a media card bakes its themed fallback into the
                            // card texture, and the list's cache keys are revision-scoped rather
                            // than content-hashed, so nothing else would invalidate it.
                            state.synoik.panel_popover.note_art_decoded(&source);
                            state.synoik.queue_redraw_all();
                        }
                    }
                })
                .unwrap();

            // Same for *symbolic* icons, which had stayed inline: a miss resolved the name across
            // every theme and category (a few hundred `stat`s when it finds nothing), read the
            // file and parsed the SVG, all inside element collection on the frame thread.
            let (sym_tx, sym_rx) = calloop::channel::channel();
            state.synoik.symbolic_icon_tx =
                crate::render_helpers::icon::spawn_symbolic_worker(sym_tx);
            if let Some(tx) = state.synoik.symbolic_icon_tx.clone() {
                state.synoik.icon_cache.set_worker(tx);
            }
            state
                .synoik
                .event_loop
                .insert_source(sym_rx, |event, _, state| {
                    if let calloop::channel::Event::Msg(done) = event {
                        if state.synoik.icon_cache.apply_rasterized(done) {
                            state.synoik.queue_redraw_all();
                        }
                    }
                })
                .unwrap();

            // Decode wallpapers on a worker thread (a 4K JPEG-XL decode would
            // otherwise stall the main loop, e.g. on a color-scheme flip), and
            // route finished decodes back here to swap in + redraw.
            let (wp_tx, wp_rx) = calloop::channel::channel();
            state.synoik.wallpaper.spawn_worker(wp_tx);
            state
                .synoik
                .event_loop
                .insert_source(wp_rx, |event, _, state| {
                    if let calloop::channel::Event::Msg(decoded) = event {
                        if state.synoik.wallpaper.apply_decoded(decoded) {
                            state.synoik.queue_redraw_all();
                        }
                    }
                })
                .unwrap();
            // The input method, which is two channels and a thread: requests out to IBus,
            // engine output and key verdicts back. Both halves are inert until the worker
            // reports a daemon, so a session with no `ibus-daemon` behaves exactly as it did
            // before this existed.
            {
                use smithay::wayland::text_input::TextInputSeat as _;

                let (to_worker, requests) = async_channel::unbounded();
                let (updates_tx, updates_rx) = calloop::channel::channel();
                crate::input_method::worker::spawn(requests, updates_tx);
                state.synoik.input_method = Some(crate::input_method::InputMethod::new(to_worker));

                let (events_tx, events_rx) = calloop::channel::channel();
                state
                    .synoik
                    .seat
                    .text_input()
                    .set_internal_input_method(Some(crate::input_method::make_sink(events_tx)));

                state
                    .synoik
                    .event_loop
                    .insert_source(events_rx, |event, _, state| {
                        if let calloop::channel::Event::Msg(event) = event {
                            state.on_text_input_event(event);
                        }
                    })
                    .unwrap();
                state
                    .synoik
                    .event_loop
                    .insert_source(updates_rx, |event, _, state| {
                        if let calloop::channel::Event::Msg(update) = event {
                            state.on_im_update(update);
                        }
                    })
                    .unwrap();
            }

            // The device, so the decode worker can write its pixels straight into device-visible
            // memory instead of leaving the render thread a multi-megabyte copy (see
            // `wallpaper`'s module doc). `None` before a renderer exists — the decode then falls
            // back to the heap.
            let gpu = state.backend.with_vulkan_renderer(|r| r.gpu().clone());
            state
                .synoik
                .wallpaper
                .update(&state.synoik.gnome_settings.background, gpu.as_ref());
            state
                .synoik
                .panel
                .set_clock_format(state.synoik.gnome_settings.clock);
            state
                .synoik
                .panel
                .set_quick_toggles(state.synoik.gnome_settings.quick_toggles);
            state
                .synoik
                .panel
                .set_a11y(state.synoik.gnome_settings.a11y);
            state
                .synoik
                .event_loop
                .insert_source(rx, |event, _, state| {
                    if let calloop::channel::Event::Msg(settings) = event {
                        debug!("GNOME settings changed: {settings:?}");
                        state
                            .synoik
                            .layout
                            .set_gnome_edge_tiling(settings.edge_tiling);
                        state
                            .synoik
                            .layout
                            .set_gnome_center_new_windows(settings.center_new_windows);
                        state
                            .synoik
                            .layout
                            .set_gnome_auto_maximize(settings.auto_maximize);
                        let gpu = state.backend.with_vulkan_renderer(|r| r.gpu().clone());
                        state
                            .synoik
                            .wallpaper
                            .update(&settings.background, gpu.as_ref());
                        state.synoik.panel.set_clock_format(settings.clock);
                        state.synoik.panel.set_quick_toggles(settings.quick_toggles);
                        state.synoik.panel.set_a11y(settings.a11y);
                        // An a11y key written by anyone else moves the switch under an
                        // open menu (GNOME's rows are `settings.bind`-ed).
                        if state.synoik.panel_popover.set_a11y(settings.a11y) {
                            state.synoik.queue_redraw_all();
                        }
                        // A `favorite-apps` change re-seeds the dash favorites and
                        // the grid (an app moves between the two).
                        state
                            .synoik
                            .app_system
                            .set_favorites(settings.favorite_apps.clone());
                        // A keymap-affecting change (layout list / options / model)
                        // rebuilds the keymap; an mru-only change (e.g. our own
                        // switch write) just re-seeds the active group — no rebuild.
                        let old_is = &state.synoik.gnome_settings.input_sources;
                        let new_is = &settings.input_sources;
                        let keymap_changed = new_is.present
                            && (old_is.sources != new_is.sources
                                || old_is.xkb_options != new_is.xkb_options
                                || old_is.xkb_model != new_is.xkb_model);
                        let mru_changed =
                            new_is.present && old_is.mru_sources != new_is.mru_sources;
                        let icon_theme_changed =
                            state.synoik.gnome_settings.icon_theme != settings.icon_theme;
                        let base_font_changed = state.synoik.gnome_settings.base_font_pt
                            != settings.base_font_pt
                            || state.synoik.gnome_settings.base_font_family
                                != settings.base_font_family;
                        state.synoik.gnome_settings = settings;
                        // `enable-animations` can flip mid-session (a11y, a user toggle), and an
                        // animation already running is left to finish on the old clock rather than
                        // snapping — same as flipping it in GNOME.
                        state.refresh_animation_clock();
                        // The scroll bindings live in this model, and the pointer
                        // handlers gate on a set derived from it.
                        state.synoik.refresh_keybinding_state();
                        state.apply_peripherals();
                        state
                            .synoik
                            .screen_shield
                            .set_settings(state.synoik.gnome_settings.shield);
                        state.synoik.unlock_dialog.set_peek_locked_down(
                            state.synoik.gnome_settings.shield.disable_show_password,
                        );
                        // `lock-enabled` and `disable-lock-screen` are two of the inhibitor's
                        // conditions, and GNOME wires both keys straight to `_syncInhibitor`
                        // (`screenShield.js:110,113`). Without this, turning locking back on
                        // leaves the fd unheld until the next shield event — and the next suspend
                        // is the race the inhibitor exists to prevent.
                        state.synoik.sync_sleep_inhibitor();
                        // Every point size in the UI is a ratio against this
                        // (`crate::ui::base_font_pt`), so publishing it re-sizes all
                        // text at once — as `st_theme_context_set_font` does.
                        if base_font_changed {
                            crate::ui::set_base_font_pt(state.synoik.gnome_settings.base_font_pt);
                            synoik_vk::text::set_sans_family(
                                &state.synoik.gnome_settings.base_font_family,
                            );
                        }
                        // Both surfaces are re-derived *after* the assignment: the grid's
                        // order comes out of `app_picker_layout`, so syncing it against
                        // the old settings would show the previous arrangement until some
                        // later change happened to sync it again.
                        state.synoik.sync_dash_favorites();
                        state.synoik.sync_app_grid();
                        // Pinning/unpinning moves an app between the dash and the grid;
                        // warm any icon that just entered a surface (idempotent).
                        state.synoik.prewarm_app_icons();
                        if keymap_changed {
                            state.apply_effective_xkb();
                            state.ipc_keyboard_layouts_changed();
                        } else if mru_changed {
                            state.seed_active_layout_from_mru();
                        }
                        // Either kind of change can move the *active* source, and the engine
                        // has to follow it: the keymap alone does not produce dead keys, the
                        // engine does (`keyboard.js:510-528`).
                        if keymap_changed || mru_changed {
                            state.sync_input_method_engine();
                        }
                        // An icon-theme change re-themes both icon caches, so the
                        // dash's uploaded textures (keyed only by icon+size+scale, not
                        // theme) must be dropped or they keep serving old-theme pixels.
                        if icon_theme_changed {
                            let theme = state.synoik.gnome_settings.icon_theme.clone();
                            state.synoik.replace_icon_cache(theme.as_str());
                            // Old pixels stay on screen until each re-decode lands,
                            // which is also what drops that icon's uploads.
                            state.synoik.app_icon_cache.set_theme(&theme);
                            // `set_theme` invalidated the decode cache — re-warm it in
                            // the new theme off-thread for the next open.
                            state.synoik.prewarm_app_icons();
                        }
                        // A DND (`show-banners`) flip toggles the dateMenu dot.
                        state.synoik.update_messages_indicator();
                        // A world-clocks change (e.g. the Clocks mirror wrote new
                        // locations) refreshes the open section.
                        state.synoik.refresh_popover_world_clocks();
                        state.synoik.queue_redraw_all();
                    }
                })
                .unwrap();
        }

        // Load the xkb_file config option if set by the user.
        state.load_xkb_file();
        // Initialize some IPC server state.
        state.ipc_keyboard_layouts_changed();
        // Focus the default monitor if set by the user.
        state.focus_default_monitor();

        Ok(state)
    }

    pub fn refresh_and_flush_clients(&mut self) {
        let _span = tracy_client::span!("State::refresh_and_flush_clients");

        // Whatever polkit asked for while the screen was covered gets its turn once it is not.
        //
        // Polled here rather than driven from an unlock, because there are two ways the screen
        // gets covered — our own shield and an `ext-session-lock` client — and only one of them
        // has an event we own. A held request that nobody resumes is polkitd waiting forever on a
        // dialog that will never be drawn.
        if self.synoik.polkit_deferred.is_some() && !self.synoik.screen_is_covered() {
            self.resume_deferred_polkit();
        }

        self.refresh();

        // Advance animations to the current time (not target render time) before rendering outputs
        // in order to clear completed animations and render elements. Even if we're not rendering,
        // it's good to advance every now and then so the workspace clean-up and animations don't
        // build up (the 1 second frame callback timer will call this line).
        self.synoik.advance_animations();

        self.synoik.redraw_queued_outputs(&mut self.backend);

        {
            let _span = tracy_client::span!("flush_clients");
            self.synoik.display_handle.flush_clients().unwrap();
        }

        self.synoik.update_locked_hint();

        // Clear the time so it's fetched afresh next iteration.
        self.synoik.clock.clear();
        self.synoik.pointer_inactivity_timer_got_reset = false;
        self.synoik.notified_activity_this_iteration = false;

        // Close the loop-watch window. Last, because this callback is the last thing
        // in a turn of the event loop, and the window it measures runs from here to
        // the same point next turn — the poll plus every source callback dispatched
        // in between. See `frame_log::LoopWatch`.
        //
        // The pending flag is sampled here rather than when a stall is reported: the
        // question is whether the loop owed a frame *while* it was busy or blocked,
        // and by the time the window closes the frame may already have gone out.
        let redraw_pending = self.synoik.output_state.values().any(|state| {
            matches!(
                state.redraw_state,
                RedrawState::Queued
                    | RedrawState::ScheduledDispatch { .. }
                    | RedrawState::WaitingForEstimatedVBlankAndQueued(_)
            )
        });
        self.synoik.frame_log.loop_turn_end(redraw_pending);
    }

    // We monitor both libinput and logind: libinput is always there (including without DBus), but
    // it misses some switch events (e.g. after unsuspend) on some systems.
    pub fn set_lid_closed(&mut self, is_closed: bool) {
        if self.synoik.is_lid_closed == is_closed {
            return;
        }

        debug!("laptop lid {}", if is_closed { "closed" } else { "opened" });
        self.synoik.is_lid_closed = is_closed;
        self.backend.on_output_config_changed(&mut self.synoik);
    }

    /// The reconcile phase of a cycle: everything that brings derived state in line with the
    /// layout, with no animation advance and no redraw. `pub(crate)` so a test can drive exactly
    /// this — a helper that also redrew would hide UI state that is (wrongly) synced at render
    /// time, which is how the panel's Activities highlight stayed one cycle late unnoticed.
    pub(crate) fn refresh(&mut self) {
        let _span = tracy_client::span!("State::refresh");

        // Handle commits for surfaces whose blockers cleared this cycle. This should happen before
        // layout.refresh() since this is where these surfaces handle commits.
        self.notify_blocker_cleared();

        // These should be called periodically, before flushing the clients.
        self.synoik.popups.cleanup();
        self.refresh_popup_grab();
        // Before the focus update, so a panel menu this dismisses (the overview opening over it)
        // has its keyboard focus recomputed in the same cycle rather than the next one.
        self.synoik.refresh_overview_panel_state();
        self.update_keyboard_focus();
        self.synoik.refresh_overview_search_state();
        // An indicator's client must learn its menu closed however the menu went away.
        self.reconcile_indicator_menu();

        // Should be called before refresh_layout() because that one will refresh other window
        // states and then send a pending configure.
        self.synoik.refresh_window_states();

        // Needs to be called after updating the keyboard focus.
        self.synoik.refresh_layout();

        // After the focus update, so the running order sees this cycle's focus
        // timestamps. Re-snapshots unconditionally and only reports a change —
        // like the keyboard-layout indicator below, that costs one window walk and
        // needs no invalidation bookkeeping. A change redisplays the dash
        // (GNOME's `_queueRedisplay` on `app-state-changed`, `dash.js:381`).
        if self.synoik.sync_running_apps() {
            self.synoik.emit_introspect_changed();
            if self.synoik.sync_dash_favorites() {
                // An app that just started is a *new* dash tile whose icon has never been
                // decoded. Warm it off-thread here, exactly as the pin/unpin path does, instead
                // of letting the first frame that draws the tile decode it inline: only the
                // catalog-reload and settings paths warmed before, so a plain app launch — the
                // most common way the dash gains a tile — was the one case that missed.
                self.synoik.prewarm_app_icons();
                self.synoik.queue_redraw_all();
            }
        }
        // Cheap and idempotent, so it runs every refresh rather than only when the dash changed:
        // urgency also clears on *focus*, which moves no window and adds no app.
        self.synoik.sync_dock_urgency();

        self.synoik
            .cursor_manager
            .check_cursor_image_surface_alive();
        self.synoik.refresh_pointer_outputs();
        self.synoik.global_space.refresh();
        self.synoik.refresh_idle_inhibit();
        self.refresh_pointer_contents();
        foreign_toplevel::refresh(self);
        ext_workspace::refresh(self);

        #[cfg(feature = "xdp-gnome-screencast")]
        self.synoik.refresh_mapped_cast_outputs();
        // Should happen before refresh_window_rules(), but after anything that can start or stop
        // screencasts.
        #[cfg(feature = "xdp-gnome-screencast")]
        self.synoik.refresh_mapped_cast_window_rules();
        self.ipc_refresh_casts();

        self.synoik.refresh_window_rules();
        self.refresh_ipc_outputs();
        self.ipc_refresh_layout();
        self.ipc_refresh_keyboard_layout_index();
        self.refresh_keyboard_layout_indicator();

        // Needs to be called after updating the keyboard focus.
        self.synoik.refresh_a11y();
    }

    /// Push the active keyboard-layout short label into the panel's `keyboard` indicator (GNOME's
    /// `InputSourceIndicator`). Runs every `refresh()`, so it also catches `track_layout "window"`
    /// focus-driven switches that have no action hook. The panel setter compares against its stored
    /// label and only reports a change, so recomputing here is cheap and needs no invalidation
    /// bookkeeping. See [`crate::keyboard_layout::short_label`].
    fn refresh_keyboard_layout_indicator(&mut self) {
        let keyboard = self.synoik.seat.get_keyboard().unwrap();
        let (names, idx) = keyboard.with_xkb_state(self, |context| {
            let xkb = context.xkb().lock().unwrap();
            let names: Vec<String> = xkb
                .layouts()
                .map(|layout| xkb.layout_name(layout).to_owned())
                .collect();
            let idx = xkb.active_layout().0 as usize;
            (names, idx)
        });

        // The effective layout codes: the niri xkb config's `layout` string, unless it is a `file`
        // keymap (codes don't apply) or unset (then locale1 supplies the keymap — mirror the
        // resolution in `reload_config_and_outputs`).
        let xkb = self.effective_xkb();
        let codes: Vec<String> = if xkb.file.is_none() {
            xkb.layout
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        } else {
            Vec::new()
        };

        let label = crate::keyboard_layout::short_label(&codes, &names, idx);
        if self.synoik.panel.set_keyboard_layout(label) {
            self.synoik.queue_redraw_all();
        }
    }

    /// The keyboard `Xkb` config actually driving the keymap. GNOME's
    /// `org.gnome.desktop.input-sources` is the source of truth when its schema is
    /// present (GNOME's way replaces niri's `input.keyboard.xkb` — CLAUDE.md tenet);
    /// only where GNOME isn't installed do we fall back to niri's config, then to
    /// systemd-localed (`xkb_from_locale1`). Mirrored across the apply paths so the
    /// panel label matches the live keymap.
    fn effective_xkb(&self) -> Xkb {
        let input_sources = &self.synoik.gnome_settings.input_sources;
        if input_sources.present {
            return crate::keyboard_layout::xkb_from_input_sources(
                &input_sources.sources,
                &input_sources.xkb_options,
                &input_sources.xkb_model,
            );
        }
        let config = self.synoik.config.borrow();
        let xkb = config.input.keyboard.xkb.clone();
        drop(config);
        if xkb == Xkb::default() {
            self.synoik.xkb_from_locale1.clone().unwrap_or_default()
        } else {
            xkb
        }
    }

    /// (Re)apply the effective keymap to the seat keyboard — used at startup and
    /// whenever GNOME's `input-sources` change — then seed the active group from
    /// `mru-sources[0]` (gnome-shell activates the most-recently-used source
    /// after (re)loading, `js/ui/status/keyboard.js` `_inputSourcesChanged`).
    pub fn apply_effective_xkb(&mut self) {
        let xkb = self.effective_xkb();
        self.set_xkb_config(xkb.to_xkb_config());
        self.seed_active_layout_from_mru();
    }

    /// Set the active xkb group to `org.gnome.desktop.input-sources mru-sources[0]`
    /// (the xkb group order is the `sources` order, ibus filtered out). A no-op
    /// when GNOME isn't the source of truth or the MRU is empty/unmatched.
    pub fn seed_active_layout_from_mru(&mut self) {
        let sources = &self.synoik.gnome_settings.input_sources;
        if !sources.present {
            return;
        }
        let Some(first) = sources.mru_sources.first().cloned() else {
            return;
        };
        let idx = sources
            .sources
            .iter()
            .filter(|(ty, _)| ty == "xkb")
            .position(|s| *s == first);
        let Some(idx) = idx.filter(|&i| i > 0) else {
            return;
        };
        self.set_active_layout(idx);
    }

    /// Switch the active xkb group to `idx` (clamped to the compiled layout
    /// count) and refresh the panel/IPC layout state.
    fn set_active_layout(&mut self, idx: usize) {
        let keyboard = self.synoik.seat.get_keyboard().unwrap();
        keyboard.with_xkb_state(self, |mut context| {
            let num = context.xkb().lock().unwrap().layouts().count();
            if idx < num {
                context.set_layout(KeyboardLayout(idx as u32));
            }
        });
        self.refresh_keyboard_layout_indicator();
        self.ipc_refresh_keyboard_layout_index();
    }

    /// Toggle the date menu (calendar + message list) on `output`'s panel —
    /// gnome-shell's `Panel.toggleCalendar` (`js/ui/panel.js:603`), reached both
    /// by clicking the clock and by `toggle-message-tray`.
    ///
    /// Opening acknowledges every notification in the store, exactly once per
    /// open and never on close (`js/ui/messageList.js:1193-1199`).
    pub fn toggle_date_menu(&mut self, output: Output) {
        let output_w = output_size(&output).w;
        let anchor = self.synoik.panel.date_menu_rect(output_w);
        let cal = self.synoik.gnome_settings.calendar;
        let accent = self.synoik.gnome_settings.accent_color;
        let now = self.synoik.clock.now_unadjusted();
        let cards =
            crate::ui::notification_card::message_list_groups(&self.synoik.notifications, now);
        let opened = self.synoik.panel_popover.toggle_calendar(
            output,
            anchor,
            cal.week_start,
            cal.show_week_numbers,
            accent,
            cards,
        );
        if opened {
            let effects = self.synoik.notifications.acknowledge_all();
            self.synoik.apply_notification_effects(effects);
            // Load events for the now-open calendar's grid (`open-state-changed` →
            // today, `js/ui/dateMenu.js:907-915`) and populate the section from
            // what's cached.
            self.synoik.sync_calendar_range();
            self.synoik.refresh_popover_calendar_events();
            self.synoik.refresh_popover_world_clocks();
            self.synoik.refresh_popover_media();
        }
        self.synoik.queue_redraw_all();
    }

    /// Toggle the quick settings menu on `output`'s panel — gnome-shell's
    /// `Panel.toggleQuickSettings` (`js/ui/panel.js:607`), reached both by
    /// clicking the indicators and by `toggle-quick-settings`.
    ///
    /// The snapshot it opens on is resolved state rather than a model: the menu
    /// shows the headphone glyph if they are already plugged in, instead of
    /// waiting for the next port change to correct itself.
    pub fn toggle_quick_settings_menu(&mut self, output: Output) {
        let output_w = output_size(&output).w;
        let toggles = self.synoik.gnome_settings.quick_toggles;
        let anchor = self.synoik.panel.quick_settings_rect(output_w);
        let network = self.synoik.system_status.network;
        let airplane = self.synoik.system_status.airplane;
        let power = self.synoik.system_status.power.clone();
        let bluetooth = self.synoik.system_status.bluetooth.clone();
        let bluetooth_rfkill = self.synoik.system_status.bluetooth_rfkill;
        let battery = self.synoik.system_status.battery.clone();
        let audio = self.synoik.audio;
        let sink_list = self.synoik.sink_list.clone();
        let audio_cards = self.synoik.audio_cards.clone();
        let headphones = self.synoik.headphones.unwrap_or(false);
        let mic = self.synoik.mic;
        let source_list = self.synoik.source_list.clone();
        let brightness = self.synoik.brightness.view();
        let accent = self.synoik.gnome_settings.accent_color;
        self.synoik.panel_popover.toggle_quick_settings(
            output,
            anchor,
            toggles,
            network,
            airplane,
            power,
            bluetooth,
            bluetooth_rfkill,
            battery,
            audio,
            sink_list,
            audio_cards,
            headphones,
            mic,
            source_list,
            brightness,
            accent,
        );
        self.synoik.queue_redraw_all();
    }

    /// The input-source popover's items (a display name + short label per layout,
    /// in xkb/source order) and the active index — read from the live xkb state
    /// (which reflects GNOME's input-sources) so it matches the panel indicator.
    pub fn input_source_menu_snapshot(
        &mut self,
    ) -> (Vec<crate::ui::input_source_menu::InputSourceItem>, usize) {
        use crate::ui::input_source_menu::InputSourceItem;
        let keyboard = self.synoik.seat.get_keyboard().unwrap();
        let (names, idx) = keyboard.with_xkb_state(self, |context| {
            let xkb = context.xkb().lock().unwrap();
            let names: Vec<String> = xkb
                .layouts()
                .map(|layout| xkb.layout_name(layout).to_owned())
                .collect();
            (names, xkb.active_layout().0 as usize)
        });
        let xkb = self.effective_xkb();
        let codes: Vec<String> = if xkb.file.is_none() {
            xkb.layout
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        } else {
            Vec::new()
        };
        let shorts = crate::keyboard_layout::labels(&codes, &names);
        let items = names
            .into_iter()
            .enumerate()
            .map(|(i, display)| InputSourceItem {
                display,
                short: shorts.get(i).cloned().unwrap_or_default(),
            })
            .collect();
        (items, idx)
    }

    /// Switch to input source `idx` (a layout-menu row): set the active xkb group
    /// and record it at the front of GNOME's `mru-sources`
    /// (`js/ui/status/keyboard.js` `activateInputSource` → `_updateMruSettings`).
    pub fn set_input_source(&mut self, idx: usize) {
        self.set_active_layout(idx);

        let sources = &self.synoik.gnome_settings.input_sources;
        if !sources.present {
            return;
        }
        let xkb_sources: Vec<(String, String)> = sources
            .sources
            .iter()
            .filter(|(ty, _)| ty == "xkb")
            .cloned()
            .collect();
        let Some(picked) = xkb_sources.get(idx).cloned() else {
            return;
        };
        // MRU = picked first, then the rest of the old MRU, then any sources not
        // yet listed — deduplicated, preserving order.
        let mut mru = vec![picked.clone()];
        for s in sources
            .mru_sources
            .iter()
            .chain(xkb_sources.iter())
            .filter(|s| **s != picked)
        {
            if !mru.contains(s) {
                mru.push(s.clone());
            }
        }
        if let Some(writer) = &self.synoik.gnome_settings_writer {
            writer.set_mru_sources(mru);
        }
    }

    fn notify_blocker_cleared(&mut self) {
        let dh = self.synoik.display_handle.clone();
        while let Ok(client) = self.synoik.blocker_cleared_rx.try_recv() {
            trace!("calling blocker_cleared");
            self.client_compositor_state(&client)
                .blocker_cleared(self, &dh);
        }
    }

    pub fn move_cursor(&mut self, location: Point<f64, Logical>) {
        let mut under = match self.synoik.pointer_visibility {
            PointerVisibility::Disabled => PointContents::default(),
            _ => self.synoik.contents_under(location),
        };

        // Disable the hidden pointer if the contents underneath have changed.
        if !self.synoik.pointer_visibility.is_visible() && self.synoik.pointer_contents != under {
            self.synoik.pointer_visibility = PointerVisibility::Disabled;

            // When setting PointerVisibility::Hidden together with pointer contents changing,
            // we can change straight to nothing to avoid one frame of hover. Notably, this can
            // be triggered through warp-mouse-to-focus combined with hide-when-typing.
            under = PointContents::default();
        }

        self.synoik.pointer_contents.clone_from(&under);

        let pointer = &self.synoik.seat.get_pointer().unwrap();
        pointer.motion(
            self,
            under.surface,
            &MotionEvent {
                location,
                serial: SERIAL_COUNTER.next_serial(),
                time: get_monotonic_time().as_millis() as u32,
            },
        );
        pointer.frame(self);

        self.synoik.maybe_activate_pointer_constraint();

        // We do not show the pointer on programmatic or keyboard movement.

        // FIXME: granular
        self.synoik.queue_redraw_all();
    }

    /// Moves cursor within the specified rectangle, only adjusting coordinates if needed.
    fn move_cursor_to_rect(&mut self, rect: Rectangle<f64, Logical>, mode: CenterCoords) -> bool {
        let pointer = &self.synoik.seat.get_pointer().unwrap();
        let cur_loc = pointer.current_location();
        let x_in_bound = cur_loc.x >= rect.loc.x && cur_loc.x <= rect.loc.x + rect.size.w;
        let y_in_bound = cur_loc.y >= rect.loc.y && cur_loc.y <= rect.loc.y + rect.size.h;

        let p = match mode {
            CenterCoords::Separately => {
                if x_in_bound && y_in_bound {
                    return false;
                } else if y_in_bound {
                    // adjust x
                    Point::from((rect.loc.x + rect.size.w / 2.0, cur_loc.y))
                } else if x_in_bound {
                    // adjust y
                    Point::from((cur_loc.x, rect.loc.y + rect.size.h / 2.0))
                } else {
                    // adjust x and y
                    center_f64(rect)
                }
            }
            CenterCoords::Both => {
                if x_in_bound && y_in_bound {
                    return false;
                } else {
                    // adjust x and y
                    center_f64(rect)
                }
            }
            CenterCoords::BothAlways => center_f64(rect),
        };

        self.move_cursor(p);
        true
    }

    pub fn move_cursor_to_focused_tile(&mut self, mode: CenterCoords) -> bool {
        if !self.synoik.keyboard_focus.is_layout() {
            return false;
        }

        if self.synoik.tablet_cursor_location.is_some() {
            return false;
        }

        let Some(output) = self.synoik.layout.active_output() else {
            return false;
        };
        let monitor = self.synoik.layout.monitor_for_output(output).unwrap();

        let mut rv = false;
        let rect = monitor.active_window_visual_rectangle();

        if let Some(rect) = rect {
            let output_geo = self.synoik.global_space.output_geometry(output).unwrap();
            let mut rect = rect;
            rect.loc += output_geo.loc.to_f64();
            rv = self.move_cursor_to_rect(rect, mode);
        }

        rv
    }

    pub fn focus_default_monitor(&mut self) {
        // Our default target is the first output in sorted order.
        let Some(mut target) = self.synoik.sorted_outputs.first().cloned() else {
            // No outputs are connected.
            return;
        };

        let config = self.synoik.config.borrow();
        for config in &config.outputs.0 {
            if !config.focus_at_startup {
                continue;
            }
            if let Some(output) = self.synoik.output_by_name_match(&config.name) {
                target = output.clone();
                break;
            }
        }
        drop(config);

        self.synoik.layout.focus_output(&target);
        self.move_cursor_to_output(&target);
    }

    /// Focus a specific window, taking care of a potential active output change and cursor
    /// warp.
    pub fn focus_window(&mut self, window: &Window) {
        let active_output = self.synoik.layout.active_output().cloned();

        self.synoik.layout.activate_window(window);

        let new_active = self.synoik.layout.active_output().cloned();
        if new_active != active_output {
            if !self.maybe_warp_cursor_to_focus_centered() {
                self.move_cursor_to_output(&new_active.unwrap());
            }
        } else {
            self.maybe_warp_cursor_to_focus();
        }

        // FIXME: granular
        self.synoik.queue_redraw_all();
    }

    /// `switch-applications` — the app switcher (`windowManager.js:1670-1712`).
    pub fn switch_applications(&mut self, backward: bool) {
        if self.advance_open_switcher(backward) {
            return;
        }

        // `org.gnome.shell.app-switcher current-workspace-only`, default **false** — the app
        // switcher spans workspaces where the window switcher does not.
        let only_here = self
            .synoik
            .gnome_settings
            .switchers
            .apps_current_workspace_only;
        let tab_list = self.synoik.switcher_tab_list(only_here);
        let items = app_items(self.synoik.app_system.running(), &tab_list);
        if items.is_empty() {
            return;
        }

        let art = self.synoik.switcher_app_art(&items);
        self.raise_switcher(Items::Apps(items), art, backward);
    }

    /// `switch-group` — walking the windows of the app you are already in.
    ///
    /// DIVERGENCE: GNOME opens the *app* switcher pinned to app 0 with its thumbnail sub-list up
    /// (`altTab.js:117-137`), so the app row you cannot use takes up most of the popup. We show
    /// the window switcher over that app's windows instead — the same previews, the same footer
    /// title, no app row — because every item in this session belongs to one app by construction,
    /// and the row above them names it once and then just sits there. Recorded in
    /// `docs/fork/alt-tab-port.md`.
    pub fn switch_group(&mut self, backward: bool) {
        if self.synoik.switcher.is_open() {
            let now = self.synoik.clock.now_unadjusted();
            let outcome = self
                .synoik
                .switcher
                .key_press(SwitcherKey::Group { backward }, now);
            self.finish_switcher(outcome);
            self.hide_osd_for_switcher();
            self.synoik.queue_redraw_switcher_output();
            return;
        }

        // `switch-group` spans workspaces like the app switcher whose setting it reads — the same
        // key `GroupCyclerPopup` reads for its own list (`altTab.js:557-570`).
        let only_here = self
            .synoik
            .gnome_settings
            .switchers
            .apps_current_workspace_only;
        let tab_list = self.synoik.switcher_tab_list(only_here);

        // The focused app's windows, in tab-list order — `focus_app.get_windows()`.
        let focused = self.synoik.layout.focus().map(|m| m.id());
        let items = app_items(self.synoik.app_system.running(), &tab_list);
        let Some(item) =
            focused.and_then(|id| items.into_iter().find(|item| item.windows.contains(&id)))
        else {
            return;
        };
        let windows = item.windows;
        if windows.is_empty() {
            return;
        }

        let art = self.synoik.switcher_window_art(&windows);
        self.raise_switcher(Items::Windows(windows), art, backward);
    }

    /// `cycle-windows` (`<Alt>Escape`) and `cycle-group` (`<Alt>F6`) — the listless switchers
    /// (`CyclerPopup`, `altTab.js:487-540`).
    ///
    /// Same session machinery as every other popup — the modal grab, the modifier-release commit,
    /// Escape — with no list drawn. The selection is shown by raising the window itself and
    /// framing it with `.cycler-highlight`, so it starts showing *immediately*: `_highlightItem`
    /// runs from `_initialSelection` inside `show()`, and `POPUP_DELAY` only ever hid a popup
    /// actor this popup does not have.
    ///
    /// A cycler advances only on **its own** action. `_keyPressHandler` matches one
    /// `Meta.KeyBindingAction` and propagates the rest, so `<Alt>F6` while `<Alt>Escape` is up
    /// does nothing at all rather than cross-driving the other cycler's list.
    pub fn cycle_windows(&mut self, backward: bool, group: bool) {
        if self.synoik.switcher.is_open() {
            // Only this cycler's own binding resolves while it is up (see `SwitcherGrab`), so
            // getting here at all means the press was ours.
            debug_assert_eq!(self.synoik.switcher.cycler_is_group(), Some(group));
            let now = self.synoik.clock.now_unadjusted();
            let outcome = self
                .synoik
                .switcher
                .key_press(SwitcherKey::Advance { backward }, now);
            self.finish_switcher(outcome);
            self.hide_osd_for_switcher();
            self.synoik.sync_switcher_preview();
            self.synoik.queue_redraw_switcher_output();
            return;
        }

        // Each cycler reads the `current-workspace-only` key of the switcher it mirrors:
        // `WindowCyclerPopup` the window switcher's (`altTab.js:640-655`), `GroupCyclerPopup`
        // the app switcher's (`:557-570`).
        let settings = &self.synoik.gnome_settings.switchers;
        let only_here = if group {
            settings.apps_current_workspace_only
        } else {
            settings.windows_current_workspace_only
        };
        let tab_list = self.synoik.switcher_tab_list(only_here);

        // `GroupCyclerPopup._getWindows` is `focus_app.get_windows()`, so the group cycler is
        // over the focused app's windows in the *same* tab-list order, not over every window.
        let windows = if group {
            let focused = self.synoik.layout.focus().map(|m| m.id());
            let items = app_items(self.synoik.app_system.running(), &tab_list);
            let Some(item) =
                focused.and_then(|id| items.into_iter().find(|item| item.windows.contains(&id)))
            else {
                return;
            };
            item.windows
        } else {
            tab_list
        };
        if windows.is_empty() {
            return;
        }

        self.raise_switcher(Items::Cycler { windows, group }, Vec::new(), backward);
        self.synoik.sync_switcher_preview();
    }

    /// `switch-windows` — the *window* switcher (`altTab.js:580-640`).
    ///
    /// A different popup class from the app switcher, not the same one in another mode: its items
    /// are windows, its previews are live, and it filters by workspace by default where the app
    /// switcher does not.
    pub fn switch_windows(&mut self, backward: bool) {
        if self.advance_open_switcher(backward) {
            return;
        }

        let only_here = self
            .synoik
            .gnome_settings
            .switchers
            .windows_current_workspace_only;
        let tab_list = self.synoik.switcher_tab_list(only_here);
        if tab_list.is_empty() {
            return;
        }

        let art = self.synoik.switcher_window_art(&tab_list);
        self.raise_switcher(Items::Windows(tab_list), art, backward);
    }

    /// A key the open popup acts on — the arrows, Escape, and the explicit-commit keys.
    ///
    /// The switch binding itself does *not* come through here: it resolves as a binding and lands
    /// in [`switch_applications`](Self::switch_applications) /
    /// [`switch_windows`](Self::switch_windows), which is the same split GNOME has between
    /// `_keyPressHandler`'s `action ===` arms and its `keysym ===` arms
    /// (`switcherPopup.js:194-219`).
    pub fn switcher_key(&mut self, key: SwitcherKey) {
        if !self.synoik.switcher.is_open() {
            return;
        }

        // Resolved before the key is dispatched, because the popup names the target and the
        // compositor performs it — `w` and `q` act on something outside the popup, unlike every
        // other key here. Neither ends the session: the list loses the item when the client
        // actually goes away, through the same `window_removed` path a client-initiated close
        // takes (GNOME reacts to `unmanaged` too, `altTab.js:983-985`).
        let close = matches!(key, SwitcherKey::CloseWindow)
            .then(|| self.synoik.switcher.close_target())
            .flatten();
        let quit = matches!(key, SwitcherKey::QuitApp)
            .then(|| self.synoik.switcher.quit_target().map(str::to_owned))
            .flatten();

        let now = self.synoik.clock.now_unadjusted();
        let outcome = self.synoik.switcher.key_press(key, now);

        if let Some(id) = close {
            if let Some((_, mapped)) = self.synoik.layout.windows().find(|(_, m)| m.id() == id) {
                mapped.toplevel().send_close();
            }
        }
        if let Some(app) = quit {
            self.request_app_quit(&app);
        }

        self.finish_switcher(outcome);
        self.hide_osd_for_switcher();
        self.synoik.queue_redraw_switcher_output();
    }

    /// A press while a popup is already up advances it rather than raising a new one — which is
    /// how holding the modifier and tapping Tab walks the row. Returns whether it handled the
    /// press.
    fn advance_open_switcher(&mut self, backward: bool) -> bool {
        if !self.synoik.switcher.is_open() {
            return false;
        }

        let now = self.synoik.clock.now_unadjusted();
        let outcome = self
            .synoik
            .switcher
            .key_press(SwitcherKey::Advance { backward }, now);
        self.finish_switcher(outcome);
        self.hide_osd_for_switcher();
        self.synoik.queue_redraw_switcher_output();
        true
    }

    /// The switcher hides every OSD as it becomes visible (`switcherPopup.js:178`), so a volume
    /// pill and an Alt-Tab popup are never up together.
    pub fn hide_osd_for_switcher(&mut self) {
        if self.synoik.switcher.take_just_shown() {
            self.synoik.osd.hide_all();
        }
    }

    fn raise_switcher(
        &mut self,
        items: Items,
        art: Vec<crate::ui::switcher::ui::ItemArt>,
        backward: bool,
    ) {
        let now = self.synoik.clock.now_unadjusted();
        let Some(output) = self.synoik.layout.active_output().cloned() else {
            return;
        };
        let mods = crate::input::modifiers_from_state(
            self.synoik.seat.get_keyboard().unwrap().modifier_state(),
        );

        let monitor = Rectangle::from_size(output_size(&output));
        let label_height = crate::ui::switcher::ui::label_height();

        let outcome = self.synoik.switcher.open(
            OpenRequest {
                items,
                art,
                backward,
                // GNOME takes the mask from the binding; we take what is held at the moment the
                // binding fired, which is the same set for every real switch binding and makes
                // the no-modifier case (a bind with no modifier, or a gesture) fall out for free.
                mask: mods,
                held: mods,
                output,
                monitor,
                label_height,
            },
            now,
        );
        self.finish_switcher(outcome);
        self.synoik.queue_redraw_switcher_output();
    }

    /// Act on a finished switcher session: activate the selection, or leave focus alone.
    pub fn finish_switcher(
        &mut self,
        outcome: Option<(
            crate::ui::switcher::SwitcherOutcome,
            crate::ui::switcher::ui::Activation,
        )>,
    ) {
        // A timeout that fired inside `advance_animations` has been waiting for a caller that can
        // actually focus a window.
        let outcome = outcome.or_else(|| self.synoik.pending_switcher_outcome.take());
        let Some((outcome, target)) = outcome else {
            return;
        };

        // Drained *before* the sync below, which would otherwise see a closed switcher, decide the
        // session was abandoned and hand every previewed workspace back before the commit gets to
        // say which one it is keeping.
        let previews = std::mem::take(&mut self.synoik.switcher_ws_preview);

        self.synoik.queue_redraw_all();
        self.synoik.sync_switcher_preview();

        let stack: Vec<Window> = target
            .iter()
            .filter_map(|&id| self.synoik.find_window_by_id(id))
            .collect();
        let committed = (outcome == crate::ui::switcher::SwitcherOutcome::Commit)
            .then(|| stack.split_first())
            .flatten();

        // A commit keeps the workspace it landed on and gives back every other one it passed
        // through; anything else — Escape, a click outside, the last item closing — gives all of
        // them back. Reversed, so a monitor touched twice ends on its earliest origin.
        let keep = committed.and_then(|(window, _)| self.synoik.layout.output_of_window(window));
        for origin in previews.iter().rev() {
            if Some(origin.output()) == keep.as_ref() {
                self.synoik.layout.keep_workspace_preview(origin);
            } else {
                self.synoik.layout.undo_workspace_preview(origin);
            }
        }

        let Some((window, under)) = committed else {
            return;
        };

        // `shell_app_activate_window` (`shell-app.c:413-425`) raises the app's *other* windows
        // first and in reverse, so they come forward as a block without re-sorting among
        // themselves, and only then activates the one that takes focus.
        self.synoik.layout.raise_under(window, under);

        // Same ordering as `confirm_mru`: the keyboard focus still points at the popup we just
        // closed (it is only recomputed at the end of the loop), so force it before focusing or
        // cursor warping does not happen.
        self.update_keyboard_focus();
        self.focus_window(window);
        // The popup is gone, so nothing is raised any more. Done here rather than left to the
        // next frame because the commit *activates* the window, and a stale raise would keep a
        // second one on top of it in between.
        self.synoik.sync_switcher_preview();
    }

    pub fn maybe_warp_cursor_to_focus(&mut self) -> bool {
        let focused = match self.synoik.config.borrow().input.warp_mouse_to_focus {
            None => return false,
            Some(inner) => match inner.mode {
                None => CenterCoords::Separately,
                Some(WarpMouseToFocusMode::CenterXy) => CenterCoords::Both,
                Some(WarpMouseToFocusMode::CenterXyAlways) => CenterCoords::BothAlways,
            },
        };
        self.move_cursor_to_focused_tile(focused)
    }

    pub fn maybe_warp_cursor_to_focus_centered(&mut self) -> bool {
        let focused = match self.synoik.config.borrow().input.warp_mouse_to_focus {
            None => return false,
            Some(inner) => match inner.mode {
                None => CenterCoords::Both,
                Some(WarpMouseToFocusMode::CenterXy) => CenterCoords::Both,
                Some(WarpMouseToFocusMode::CenterXyAlways) => CenterCoords::BothAlways,
            },
        };
        self.move_cursor_to_focused_tile(focused)
    }

    pub fn refresh_pointer_contents(&mut self) {
        let _span = tracy_client::span!("Synoik::refresh_pointer_contents");

        let pointer = &self.synoik.seat.get_pointer().unwrap();
        let location = pointer.current_location();

        if !self.synoik.exit_confirm_dialog.is_open()
            && !self.synoik.run_dialog.is_open()
            && !self.synoik.is_locked()
            && !self.synoik.screenshot_ui.is_open()
        {
            // Don't refresh cursor focus during transitions.
            if let Some((output, _)) = self.synoik.output_under(location) {
                let monitor = self.synoik.layout.monitor_for_output(output).unwrap();
                if monitor.are_transitions_ongoing() {
                    return;
                }
            }
        }

        if !self.update_pointer_contents() {
            return;
        }

        pointer.frame(self);

        // Pointer motion from a surface to nothing triggers a cursor change to default, which
        // means we may need to redraw.

        // FIXME: granular
        self.synoik.queue_redraw_all();
    }

    pub fn update_pointer_contents(&mut self) -> bool {
        let _span = tracy_client::span!("Synoik::update_pointer_contents");

        let pointer = &self.synoik.seat.get_pointer().unwrap();
        let location = pointer.current_location();
        let mut under = match self.synoik.pointer_visibility {
            PointerVisibility::Disabled => PointContents::default(),
            _ => self.synoik.contents_under(location),
        };

        // We're not changing the global cursor location here, so if the contents did not change,
        // then nothing changed.
        if self.synoik.pointer_contents == under {
            return false;
        }

        // Disable the hidden pointer if the contents underneath have changed.
        if !self.synoik.pointer_visibility.is_visible() {
            self.synoik.pointer_visibility = PointerVisibility::Disabled;

            // When setting PointerVisibility::Hidden together with pointer contents changing,
            // we can change straight to nothing to avoid one frame of hover. Notably, this can
            // be triggered through warp-mouse-to-focus combined with hide-when-typing.
            under = PointContents::default();
            if self.synoik.pointer_contents == under {
                return false;
            }
        }

        self.synoik.pointer_contents.clone_from(&under);

        pointer.motion(
            self,
            under.surface,
            &MotionEvent {
                location,
                serial: SERIAL_COUNTER.next_serial(),
                time: get_monotonic_time().as_millis() as u32,
            },
        );

        self.synoik.maybe_activate_pointer_constraint();

        true
    }

    pub fn move_cursor_to_output(&mut self, output: &Output) {
        let geo = self.synoik.global_space.output_geometry(output).unwrap();
        self.move_cursor(center(geo).to_f64());
    }

    pub fn refresh_popup_grab(&mut self) {
        if let Some(grab) = &mut self.synoik.popup_grab {
            if grab.grab.has_ended() {
                self.synoik.popup_grab = None;
            }
        }
    }

    pub fn update_keyboard_focus(&mut self) {
        // Clean up on-demand layer surface focus if necessary.
        if let Some(surface) = &self.synoik.layer_shell_on_demand_focus {
            // Still alive and has on-demand interactivity.
            let mut good = surface.alive()
                && surface.cached_state().keyboard_interactivity
                    == wlr_layer::KeyboardInteractivity::OnDemand;

            if let Some(mapped) = self.synoik.mapped_layer_surfaces.get(surface) {
                // Check if it moved to the overview backdrop.
                if mapped.place_within_backdrop() {
                    good = false;
                }
            } else {
                // The layer surface is alive but it got unmapped.
                good = false;
            }

            if !good {
                self.synoik.layer_shell_on_demand_focus = None;
            }
        }

        // Compute the current focus.
        let focus = if self.synoik.exit_confirm_dialog.is_open() {
            KeyboardFocus::ExitConfirmDialog
        } else if self.synoik.run_dialog.is_open() {
            KeyboardFocus::RunDialog
        } else if self.synoik.end_session_dialog.is_open() {
            KeyboardFocus::EndSessionDialog
        } else if self.synoik.is_locked() {
            KeyboardFocus::LockScreen {
                surface: self.synoik.lock_surface_focus(),
            }
        } else if self.synoik.polkit_is_open() {
            KeyboardFocus::PolkitDialog
        } else if self.synoik.screenshot_ui.is_open() {
            KeyboardFocus::ScreenshotUi
        } else if self.synoik.switcher.is_open() {
            KeyboardFocus::Switcher
        } else if self.synoik.panel_popover.grabs_input() {
            KeyboardFocus::Popover
        } else if let Some(output) = self.synoik.layout.active_output() {
            let mon = self.synoik.layout.monitor_for_output(output).unwrap();
            let layers = layer_map_for_output(output);

            // Explicitly check for layer-shell popup grabs here, our keyboard focus will stay on
            // the root layer surface while it has grabs.
            let layer_grab = self.synoik.popup_grab.as_ref().and_then(|g| {
                layers
                    .layer_for_surface(&g.root, WindowSurfaceType::TOPLEVEL)
                    .and_then(|l| l.can_receive_keyboard_focus().then(|| (&g.root, l.layer())))
            });
            let grab_on_layer = |layer: Layer| {
                layer_grab
                    .and_then(move |(s, l)| if l == layer { Some(s.clone()) } else { None })
                    .map(|surface| KeyboardFocus::LayerShell { surface })
            };

            let layout_focus = || {
                self.synoik
                    .layout
                    .focus()
                    .map(|win| win.toplevel().wl_surface().clone())
                    .map(|surface| KeyboardFocus::Layout {
                        surface: Some(surface),
                    })
            };

            let excl_focus_on_layer = |layer| {
                layers.layers_on(layer).find_map(|surface| {
                    if surface.cached_state().keyboard_interactivity
                        != wlr_layer::KeyboardInteractivity::Exclusive
                    {
                        return None;
                    }

                    let mapped = self.synoik.mapped_layer_surfaces.get(surface)?;
                    if mapped.place_within_backdrop() {
                        return None;
                    }

                    let surface = surface.wl_surface().clone();
                    Some(KeyboardFocus::LayerShell { surface })
                })
            };

            let on_d_focus_on_layer = |layer| {
                layers.layers_on(layer).find_map(|surface| {
                    let is_on_demand_surface =
                        Some(surface) == self.synoik.layer_shell_on_demand_focus.as_ref();
                    is_on_demand_surface
                        .then(|| surface.wl_surface().clone())
                        .map(|surface| KeyboardFocus::LayerShell { surface })
                })
            };

            // Prefer exclusive focus on a layer, then check on-demand focus.
            let focus_on_layer =
                |layer| excl_focus_on_layer(layer).or_else(|| on_d_focus_on_layer(layer));

            let is_overview_open = self.synoik.layout.is_overview_open();

            let mut surface = grab_on_layer(Layer::Overlay);
            // FIXME: we shouldn't prioritize the top layer grabs over regular overlay input or a
            // fullscreen layout window. This will need tracking in grab() to avoid handing it out
            // in the first place. Or a better way to structure this code.
            surface = surface.or_else(|| grab_on_layer(Layer::Top));

            if !is_overview_open {
                surface = surface.or_else(|| grab_on_layer(Layer::Bottom));
                surface = surface.or_else(|| grab_on_layer(Layer::Background));
            }

            surface = surface.or_else(|| focus_on_layer(Layer::Overlay));

            if mon.render_above_top_layer() {
                surface = surface.or_else(layout_focus);
                surface = surface.or_else(|| focus_on_layer(Layer::Top));
                surface = surface.or_else(|| focus_on_layer(Layer::Bottom));
                surface = surface.or_else(|| focus_on_layer(Layer::Background));
            } else {
                surface = surface.or_else(|| focus_on_layer(Layer::Top));

                if is_overview_open {
                    surface = Some(surface.unwrap_or(KeyboardFocus::Overview));
                }

                surface = surface.or_else(|| on_d_focus_on_layer(Layer::Bottom));
                surface = surface.or_else(|| on_d_focus_on_layer(Layer::Background));
                surface = surface.or_else(layout_focus);

                // Bottom and background layers can only receive exclusive focus when there are no
                // layout windows.
                surface = surface.or_else(|| excl_focus_on_layer(Layer::Bottom));
                surface = surface.or_else(|| excl_focus_on_layer(Layer::Background));
            }

            surface.unwrap_or(KeyboardFocus::Layout { surface: None })
        } else {
            KeyboardFocus::Layout { surface: None }
        };

        let keyboard = self.synoik.seat.get_keyboard().unwrap();
        if self.synoik.keyboard_focus != focus {
            trace!(
                "keyboard focus changed from {:?} to {:?}",
                self.synoik.keyboard_focus,
                focus
            );

            // Tell the windows their new focus state for window rule purposes.
            if let KeyboardFocus::Layout {
                surface: Some(surface),
            } = &self.synoik.keyboard_focus
            {
                if let Some((mapped, _)) = self.synoik.layout.find_window_and_output_mut(surface) {
                    mapped.set_is_focused(false);
                }
            }
            if let KeyboardFocus::Layout {
                surface: Some(surface),
            } = &focus
            {
                if let Some((mapped, _)) = self.synoik.layout.find_window_and_output_mut(surface) {
                    mapped.set_is_focused(true);

                    // Focus *is* the user time, as in mutter (`meta_window_focus` sets it).
                    //
                    // niri debounced this by `recent-windows debounce-ms` (750 by default) so
                    // that tabbing through windows did not reorder the list underneath the
                    // switcher. That protection is no longer needed and the knob is gone: GNOME's
                    // switchers cache their window list when the popup opens
                    // (`altTab.js:719-720`), and so does ours, so nothing the focus does
                    // mid-switch can reorder the row you are looking at.
                    mapped.set_focus_timestamp(get_monotonic_time());
                }
            }

            if let Some(grab) = self.synoik.popup_grab.as_mut() {
                if grab.has_keyboard_grab && Some(&grab.root) != focus.surface() {
                    trace!(
                        "grab root {:?} is not the new focus {:?}, ungrabbing",
                        grab.root,
                        focus
                    );

                    grab.grab.ungrab(PopupUngrabStrategy::All);
                    keyboard.unset_grab(self);
                    self.synoik.seat.get_pointer().unwrap().unset_grab(
                        self,
                        SERIAL_COUNTER.next_serial(),
                        get_monotonic_time().as_millis() as u32,
                    );
                    self.synoik.popup_grab = None;
                }
            }

            if self.synoik.config.borrow().input.keyboard.track_layout == TrackLayout::Window {
                let current_layout = keyboard.with_xkb_state(self, |context| {
                    let xkb = context.xkb().lock().unwrap();
                    xkb.active_layout()
                });

                let mut new_layout = current_layout;
                // Store the currently active layout for the surface.
                if let Some(current_focus) = self.synoik.keyboard_focus.surface() {
                    with_states(current_focus, |data| {
                        let cell = data
                            .data_map
                            .get_or_insert::<Cell<KeyboardLayout>, _>(Cell::default);
                        cell.set(current_layout);
                    });
                }

                if let Some(focus) = focus.surface() {
                    new_layout = with_states(focus, |data| {
                        let cell = data.data_map.get_or_insert::<Cell<KeyboardLayout>, _>(|| {
                            // The default layout is effectively the first layout in the
                            // keymap, so use it for new windows.
                            Cell::new(KeyboardLayout::default())
                        });
                        cell.get()
                    });
                }
                if new_layout != current_layout && focus.surface().is_some() {
                    keyboard.set_focus(self, None, SERIAL_COUNTER.next_serial());
                    keyboard.with_xkb_state(self, |mut context| {
                        context.set_layout(new_layout);
                    });
                }
            }

            self.synoik.keyboard_focus.clone_from(&focus);
            keyboard.set_focus(self, focus.into_surface(), SERIAL_COUNTER.next_serial());

            // FIXME: can be more granular.
            self.synoik.queue_redraw_all();
        }

        // Outside the `!=` guard: an entry of ours can open or close without keyboard focus
        // moving (a modal dialog over the same window keeps `KeyboardFocus::Layout`), and the
        // engine must not keep composing into an entry that has gone away.
        self.sync_im_focus();
    }

    /// Loads the xkb keymap from a file config setting.
    fn set_xkb_file(&mut self, xkb_file: String) -> anyhow::Result<()> {
        let xkb_file = PathBuf::from(xkb_file);
        let xkb_file = expand_home(&xkb_file)
            .context("failed to expand ~")?
            .unwrap_or(xkb_file);

        let keymap = std::fs::read_to_string(xkb_file).context("failed to read xkb_file")?;

        let keyboard = self.synoik.seat.get_keyboard().unwrap();
        let num_lock = keyboard.modifier_state().num_lock;

        keyboard
            .set_keymap_from_string(self, keymap)
            .context("failed to set keymap")?;

        // Restore num lock to its previous value.
        let mut mods_state = keyboard.modifier_state();
        if mods_state.num_lock != num_lock {
            mods_state.num_lock = num_lock;
            keyboard.set_modifier_state(mods_state);
        }

        Ok(())
    }

    fn load_xkb_file(&mut self) {
        let xkb_file = self.synoik.config.borrow().input.keyboard.xkb.file.clone();
        if let Some(xkb_file) = xkb_file {
            if let Err(err) = self.set_xkb_file(xkb_file) {
                warn!("error loading xkb_file: {err:?}");
            }
        }
    }

    pub fn set_xkb_config(&mut self, xkb: XkbConfig) {
        let keyboard = self.synoik.seat.get_keyboard().unwrap();
        let num_lock = keyboard.modifier_state().num_lock;
        if let Err(err) = keyboard.set_xkb_config(self, xkb) {
            warn!("error updating xkb config: {err:?}");
            return;
        }

        // Restore num lock to its previous value.
        let mut mods_state = keyboard.modifier_state();
        if mods_state.num_lock != num_lock {
            mods_state.num_lock = num_lock;
            keyboard.set_modifier_state(mods_state);
        }
    }

    /// Swap in a new [`Config`] wholesale and re-derive everything that depends on it.
    ///
    /// Push `gnome_settings.peripherals` onto the devices and the keyboard.
    ///
    /// The model lands in `config.input` — the same fields the config file used to fill — so
    /// `apply_libinput_settings` and the repeat timer read it exactly where they always did.
    /// Safe to call on every settings change: each half is skipped when nothing it cares about
    /// moved, because re-applying libinput settings walks every device.
    pub fn apply_peripherals(&mut self) {
        let _span = tracy_client::span!("State::apply_peripherals");

        let p = self.synoik.gnome_settings.peripherals.clone();

        let (devices_changed, repeat_changed, numlock) = {
            let mut config = self.synoik.config.borrow_mut();
            let input = &mut config.input;

            let devices_changed = input.touchpad != p.touchpad
                || input.mouse != p.mouse
                || input.trackpoint != p.trackpoint
                || input.trackball != p.trackball;
            input.touchpad = p.touchpad;
            input.mouse = p.mouse;
            input.trackpoint = p.trackpoint;
            input.trackball = p.trackball;

            let kb = &mut input.keyboard;
            let repeat_changed =
                kb.repeat_delay != p.repeat_delay || kb.repeat_rate != p.repeat_rate;
            kb.repeat_delay = p.repeat_delay;
            kb.repeat_rate = p.repeat_rate;
            // Only worth pushing at the keyboard when it is being asked for: the lock is
            // live state the user toggles, not something to keep resetting under them.
            let numlock = !kb.numlock && p.numlock;
            kb.numlock = p.numlock;

            (devices_changed, repeat_changed, numlock)
        };

        if repeat_changed {
            let config = self.synoik.config.borrow();
            let keyboard = self.synoik.seat.get_keyboard().unwrap();
            keyboard.change_repeat_info(
                config.input.keyboard.repeat_rate.into(),
                config.input.keyboard.repeat_delay.into(),
            );
        }

        if numlock {
            let keyboard = self.synoik.seat.get_keyboard().unwrap();
            let mut modifier_state = keyboard.modifier_state();
            modifier_state.num_lock = true;
            keyboard.set_modifier_state(modifier_state);
        }

        if devices_changed {
            let config = self.synoik.config.borrow();
            for mut device in self.synoik.devices.iter().cloned() {
                apply_libinput_settings(&config.input, &mut device);
            }
        }
    }

    /// Point the animation clock at the current settings: how fast animations run, and whether
    /// they run at all.
    ///
    /// Two things can turn them off, and either one is enough. `animations.off` is ours (the
    /// debug/config switch the tests reach for). `org.gnome.desktop.interface enable-animations`
    /// is GNOME's, and honoring it is what mutter does — a session where the user, an a11y
    /// profile or a VM image turned animations off must not animate. This used to be read and
    /// ignored, which left the key advertised over `org.gnome.Shell.Introspect` (so the portal
    /// animated to match it) while the shell itself kept animating.
    ///
    /// It doubles as the deterministic mode any test rig wants: with animations off there is no
    /// transition to race, and it is reached through the same key a user has rather than through a
    /// test-only switch. Pinning the *clock* is Fixture-internal and deliberately stays that way.
    pub(crate) fn apply_animation_clock(&mut self, config: &Config) {
        let rate = 1.0 / config.animations.slowdown.max(0.001);
        let off = config.animations.off || !self.synoik.gnome_settings.enable_animations;

        self.synoik.clock.set_rate(rate);
        self.synoik.clock.set_complete_instantly(off);
    }

    /// As [`apply_animation_clock`](Self::apply_animation_clock), for the callers that just want
    /// the current config re-applied (a gsettings change, startup).
    pub(crate) fn refresh_animation_clock(&mut self) {
        let config = self.synoik.config.clone();
        let config = config.borrow();
        self.apply_animation_clock(&config);
    }

    /// Nothing in the session reaches this any more — there is no config file to reload — but
    /// it stays as the one place that knows how to apply a config, and the renderer tests use
    /// it to install a custom shader through the real path.
    pub fn reload_config(&mut self, mut config: Config) {
        let _span = tracy_client::span!("State::reload_config");

        // Find & orphan removed named workspaces.
        let mut removed_workspaces: Vec<String> = vec![];
        for ws in &self.synoik.config.borrow().workspaces {
            if !config.workspaces.iter().any(|w| w.name == ws.name) {
                removed_workspaces.push(ws.name.0.clone());
            }
        }
        for name in removed_workspaces {
            self.synoik.layout.unname_workspace(&name);
        }

        self.synoik.layout.update_config(&config);
        for mapped in self.synoik.mapped_layer_surfaces.values_mut() {
            mapped.update_config(&config);
        }

        // Create new named workspaces.
        for ws_config in &config.workspaces {
            self.synoik.layout.ensure_named_workspace(ws_config);
        }

        self.apply_animation_clock(&config);

        *CHILD_ENV.write().unwrap() = mem::take(&mut config.environment);

        let mut reload_xkb = None;
        let mut libinput_config_changed = false;
        let mut output_config_changed = false;
        let mut preserved_output_config = None;
        let mut window_rules_changed = false;
        let mut layer_rules_changed = false;
        let mut shaders_changed = false;
        let mut cursor_inactivity_timeout_changed = false;
        let mut xwls_changed = false;
        // Through a clone of the Rc, so holding this borrow does not also hold a
        // borrow of `self.synoik` — several of the updates below need it mutably.
        let config_cell = self.synoik.config.clone();
        let mut old_config = config_cell.borrow_mut();

        // Reload the cursor.
        if config.cursor != old_config.cursor {
            self.synoik
                .cursor_manager
                .reload(&config.cursor.xcursor_theme, config.cursor.xcursor_size);
            self.synoik.cursor_texture_cache.clear();
        }

        // We need &mut self to reload the xkb config, so just store it here.
        if config.input.keyboard.xkb != old_config.input.keyboard.xkb {
            reload_xkb = Some(config.input.keyboard.xkb.clone());
        }

        // Reload the repeat info.
        if config.input.keyboard.repeat_rate != old_config.input.keyboard.repeat_rate
            || config.input.keyboard.repeat_delay != old_config.input.keyboard.repeat_delay
        {
            let keyboard = self.synoik.seat.get_keyboard().unwrap();
            keyboard.change_repeat_info(
                config.input.keyboard.repeat_rate.into(),
                config.input.keyboard.repeat_delay.into(),
            );
        }

        if config.input.touchpad != old_config.input.touchpad
            || config.input.mouse != old_config.input.mouse
            || config.input.trackball != old_config.input.trackball
            || config.input.trackpoint != old_config.input.trackpoint
            || config.input.tablet != old_config.input.tablet
            || config.input.touch != old_config.input.touch
        {
            libinput_config_changed = true;
        }

        let ignored_nodes_changed =
            config.debug.ignored_drm_devices != old_config.debug.ignored_drm_devices;

        if config.outputs != self.synoik.config_file_output_config {
            output_config_changed = true;
            self.synoik
                .config_file_output_config
                .clone_from(&config.outputs);
        } else {
            // Output config did not change from the last disk load, so we need to preserve the
            // transient changes.
            preserved_output_config = Some(mem::take(&mut old_config.outputs));
        }

        // Only the mod key still comes from the config; the bindings themselves are in
        // GSettings and change through their own watch.
        let new_mod_key = self.backend.mod_key(&config);
        if new_mod_key != self.backend.mod_key(&old_config) {
            self.synoik
                .hotkey_overlay
                .on_hotkey_config_updated(new_mod_key);
            self.synoik.refresh_keybinding_state();
        }

        if config.window_rules != old_config.window_rules {
            window_rules_changed = true;
        }

        if config.layer_rules != old_config.layer_rules {
            layer_rules_changed = true;
        }

        if config.animations.window_resize.custom_shader
            != old_config.animations.window_resize.custom_shader
        {
            let src = config.animations.window_resize.custom_shader.as_deref();
            self.backend
                .with_vulkan_renderer(|vk| vk.set_custom_resize_shader(src));
            shaders_changed = true;
        }

        if config.animations.window_close.custom_shader
            != old_config.animations.window_close.custom_shader
        {
            let src = config.animations.window_close.custom_shader.as_deref();
            self.backend
                .with_vulkan_renderer(|vk| vk.set_custom_close_shader(src));
            shaders_changed = true;
        }

        if config.animations.window_open.custom_shader
            != old_config.animations.window_open.custom_shader
        {
            let src = config.animations.window_open.custom_shader.as_deref();
            self.backend
                .with_vulkan_renderer(|vk| vk.set_custom_open_shader(src));
            shaders_changed = true;
        }

        if config.cursor.hide_after_inactive_ms != old_config.cursor.hide_after_inactive_ms {
            cursor_inactivity_timeout_changed = true;
        }

        if config.debug.keep_laptop_panel_on_when_lid_is_closed
            != old_config.debug.keep_laptop_panel_on_when_lid_is_closed
        {
            output_config_changed = true;
        }

        if config.debug.ignored_drm_devices != old_config.debug.ignored_drm_devices {
            output_config_changed = true;
        }

        // FIXME: move backdrop rendering into layout::Monitor, then this will become unnecessary.
        if config.overview.backdrop_color != old_config.overview.backdrop_color {
            output_config_changed = true;
        }
        if config.layout.background_color != old_config.layout.background_color {
            output_config_changed = true;
        }

        if config.xwayland_satellite != old_config.xwayland_satellite {
            xwls_changed = true;
        }

        *old_config = config;

        if let Some(outputs) = preserved_output_config {
            old_config.outputs = outputs;
        }

        // Release the borrow.
        drop(old_config);

        // Now with a &mut self we can reload the xkb config — unless GNOME's
        // input-sources own the keymap, in which case synoik-config xkb changes are
        // ignored (GNOME's way replaces niri's).
        if let Some(mut xkb) =
            reload_xkb.filter(|_| !self.synoik.gnome_settings.input_sources.present)
        {
            let mut set_xkb_config = true;

            // It's fine to .take() the xkb file, as this is a
            // clone and the file field is not used in the XkbConfig.
            if let Some(xkb_file) = xkb.file.take() {
                if let Err(err) = self.set_xkb_file(xkb_file) {
                    warn!("error reloading xkb_file: {err:?}");
                } else {
                    // We successfully set xkb file so we don't need to fallback to XkbConfig.
                    set_xkb_config = false;
                }
            }

            if set_xkb_config {
                // If xkb is unset in the niri config, use settings from locale1.
                if xkb == Xkb::default() {
                    trace!("using xkb from locale1");
                    xkb = self.synoik.xkb_from_locale1.clone().unwrap_or_default();
                }

                self.set_xkb_config(xkb.to_xkb_config());
            }

            self.ipc_keyboard_layouts_changed();
        }

        if libinput_config_changed {
            let config = self.synoik.config.borrow();
            for mut device in self.synoik.devices.iter().cloned() {
                apply_libinput_settings(&config.input, &mut device);
            }
        }

        if ignored_nodes_changed {
            self.backend.update_ignored_nodes_config(&mut self.synoik);
        }

        if output_config_changed {
            self.reload_output_config();
        }

        if window_rules_changed {
            self.synoik.recompute_window_rules();
        }

        if layer_rules_changed {
            self.synoik.recompute_layer_rules();
        }

        if shaders_changed {
            self.synoik.update_shaders();
        }

        if cursor_inactivity_timeout_changed {
            // Force reset due to timeout change.
            self.synoik.pointer_inactivity_timer_got_reset = false;
            self.synoik.reset_pointer_inactivity_timer();
        }

        if xwls_changed {
            // If xwl-s was previously working and is now off, we don't try to kill it or stop
            // watching the sockets, for simplicity's sake.
            let was_working = self.synoik.satellite.is_some();

            // Try to start, or restart in case the user corrected the path or something.
            xwayland::satellite::setup(self);

            let config = self.synoik.config.borrow();
            let display_name = (!config.xwayland_satellite.off)
                .then_some(self.synoik.satellite.as_ref())
                .flatten()
                .map(|satellite| satellite.display_name().to_owned());

            if let Some(name) = &display_name {
                if !was_working {
                    info!("listening on X11 socket: {name}");
                }
            }

            // This won't change the systemd environment, but oh well.
            *CHILD_DISPLAY.write().unwrap() = display_name;
        }

        // Can't really update xdg-decoration settings since we have to hide the globals for CSD
        // due to the SDL2 bug... I don't imagine clients are prepared for the xdg-decoration
        // global suddenly appearing? Either way, right now it's live-reloaded in a sense that new
        // clients will use the new xdg-decoration setting.

        self.synoik.queue_redraw_all();
    }

    pub fn reload_output_config(&mut self) {
        let mut resized_outputs = vec![];
        let mut recolored_outputs = vec![];

        // Precedence for scale/transform, top first: a config applied live this session
        // (GNOME Settings' ApplyMonitorsConfig / `synoik msg output` / wlr-output-management —
        // mutter's "current" config, never overridden by the store), then GNOME's display-config
        // store (`~/.config/monitors.xml` — what mutter restores every login; per the fork tenet
        // it wins over niri's KDL), then the KDL `output {}` config (advanced escape hatch), then
        // the DPI guess. The store is loaded fresh each reload (cheap, and picks up external
        // edits without a file watcher).
        let monitors_config = crate::monitors_xml::MonitorsConfig::load();

        for output in self.synoik.global_space.outputs() {
            let name = output.user_data().get::<OutputName>().unwrap();
            let (scale, transform) = self
                .synoik
                .derive_output_scale_transform(output, monitors_config.as_ref());
            let full_config = self.synoik.config.borrow_mut();
            let config = full_config.outputs.find(name);

            if output.current_scale().fractional_scale() != scale
                || output.current_transform() != transform
            {
                output.change_current_state(
                    None,
                    Some(transform),
                    Some(output::Scale::Fractional(scale)),
                    None,
                );
                self.synoik.ipc_outputs_changed = true;
                resized_outputs.push(output.clone());
            }

            let mut backdrop_color = config
                .and_then(|c| c.backdrop_color)
                .unwrap_or(full_config.overview.backdrop_color)
                .to_array_unpremul();
            backdrop_color[3] = 1.;
            let backdrop_color = Color32F::from(backdrop_color);

            if let Some(state) = self.synoik.output_state.get_mut(output) {
                if state.backdrop_buffer.color() != backdrop_color {
                    state.backdrop_buffer.set_color(backdrop_color);
                    recolored_outputs.push(output.clone());
                }
            }

            for mon in self.synoik.layout.monitors_mut() {
                if mon.output() != output {
                    continue;
                }

                let mut layout_config = config.and_then(|c| c.layout.clone());
                // Support the deprecated non-layout background-color key.
                if let Some(layout) = &mut layout_config {
                    if layout.background_color.is_none() {
                        layout.background_color = config.and_then(|c| c.background_color);
                    }
                }

                if mon.update_layout_config(layout_config) {
                    // Also redraw these; if anything, the background color could've changed.
                    recolored_outputs.push(output.clone());
                }
                break;
            }
        }

        for output in resized_outputs {
            self.synoik.output_resized(&output);
        }

        for output in recolored_outputs {
            self.synoik.queue_redraw(&output);
        }

        self.backend.on_output_config_changed(&mut self.synoik);

        self.synoik.reposition_outputs(None);

        if let Some(touch) = self.synoik.seat.get_touch() {
            touch.cancel(self);
        }

        let config = self.synoik.config.borrow().outputs.clone();
        self.synoik
            .output_management_state
            .on_config_changed(config);
    }

    pub fn modify_output_config<F>(&mut self, name: &str, fun: F)
    where
        F: FnOnce(&mut synoik_config::Output),
    {
        // Try hard to find the output config section corresponding to the output set by the
        // user. Since if we add a new section and some existing section also matches the
        // output, then our new section won't do anything.
        let temp;
        let match_name = if let Some(output) = self.synoik.output_by_name_match(name) {
            output.user_data().get::<OutputName>().unwrap()
        } else if let Some(output_name) = self
            .backend
            .tty_checked()
            .and_then(|tty| tty.disconnected_connector_name_by_name_match(name))
        {
            temp = output_name;
            &temp
        } else {
            // Even if name is "make model serial", matching will work fine this way.
            temp = OutputName {
                connector: name.to_owned(),
                make: None,
                model: None,
                serial: None,
            };
            &temp
        };

        let mut config = self.synoik.config.borrow_mut();
        let config = if let Some(config) = config.outputs.find_mut(match_name) {
            config
        } else {
            config.outputs.0.push(synoik_config::Output {
                // Save name as set by the user.
                name: String::from(name),
                ..Default::default()
            });
            config.outputs.0.last_mut().unwrap()
        };

        fun(config);
    }

    pub fn apply_transient_output_config(&mut self, name: &str, action: synoik_ipc::OutputAction) {
        // A live-applied scale/transform outranks the monitors.xml store (see
        // `Synoik::applied_display_config`); record it so the reload below doesn't resurrect the
        // stored value.
        let connector = self
            .synoik
            .output_by_name_match(name)
            .map(|o| o.user_data().get::<OutputName>().unwrap().connector.clone());
        if let Some(connector) = connector {
            match &action {
                synoik_ipc::OutputAction::Scale { scale } => {
                    let entry = self
                        .synoik
                        .applied_display_config
                        .entry(connector)
                        .or_default();
                    // Automatic clears the override: fall back to the store, KDL, then the guess.
                    entry.scale = match scale {
                        synoik_ipc::ScaleToSet::Automatic => None,
                        synoik_ipc::ScaleToSet::Specific(scale) => Some(*scale),
                    };
                }
                synoik_ipc::OutputAction::Transform { transform } => {
                    self.synoik
                        .applied_display_config
                        .entry(connector)
                        .or_default()
                        .transform = Some(ipc_transform_to_smithay(*transform));
                }
                _ => (),
            }
        }

        self.modify_output_config(name, move |config| match action {
            synoik_ipc::OutputAction::Off => config.off = true,
            synoik_ipc::OutputAction::On => config.off = false,
            synoik_ipc::OutputAction::Mode { mode } => {
                config.mode = match mode {
                    synoik_ipc::ModeToSet::Automatic => None,
                    synoik_ipc::ModeToSet::Specific(mode) => Some(synoik_config::output::Mode {
                        custom: false,
                        mode,
                    }),
                };
                config.modeline = None;
            }
            synoik_ipc::OutputAction::CustomMode { mode } => {
                config.mode = Some(synoik_config::output::Mode { custom: true, mode });
                config.modeline = None;
            }
            synoik_ipc::OutputAction::Modeline {
                clock,
                hdisplay,
                hsync_start,
                hsync_end,
                htotal,
                vdisplay,
                vsync_start,
                vsync_end,
                vtotal,
                hsync_polarity,
                vsync_polarity,
            } => {
                // Do not reset config.mode to None since it's used as a fallback.
                config.modeline = Some(synoik_config::output::Modeline {
                    clock,
                    hdisplay,
                    hsync_start,
                    hsync_end,
                    htotal,
                    vdisplay,
                    vsync_start,
                    vsync_end,
                    vtotal,
                    hsync_polarity,
                    vsync_polarity,
                })
            }
            synoik_ipc::OutputAction::Scale { scale } => {
                config.scale = match scale {
                    synoik_ipc::ScaleToSet::Automatic => None,
                    synoik_ipc::ScaleToSet::Specific(scale) => Some(FloatOrInt(scale)),
                }
            }
            synoik_ipc::OutputAction::Transform { transform } => config.transform = transform,
            synoik_ipc::OutputAction::Position { position } => {
                config.position = match position {
                    synoik_ipc::PositionToSet::Automatic => None,
                    synoik_ipc::PositionToSet::Specific(position) => {
                        Some(synoik_config::Position {
                            x: position.x,
                            y: position.y,
                        })
                    }
                }
            }
            synoik_ipc::OutputAction::Vrr { vrr } => {
                config.variable_refresh_rate = if vrr.vrr {
                    Some(synoik_config::Vrr {
                        on_demand: vrr.on_demand,
                    })
                } else {
                    None
                }
            }
            synoik_ipc::OutputAction::MaxBpc { max_bpc } => config.max_bpc = Some(MaxBpc(max_bpc)),
        });

        self.reload_output_config();
    }

    /// Applies a display configuration coming from `org.gnome.Mutter.DisplayConfig`
    /// `ApplyMonitorsConfig` (GNOME Settings, our quick-settings).
    ///
    /// Mirrors mutter (meta-monitor-manager.c, `meta_monitor_manager_apply_monitors_config` →
    /// `meta_monitor_config_manager_set_current`): the applied config becomes the current session
    /// config immediately and outranks the `monitors.xml` store. Persisting the store is the DBus
    /// handler's separate concern; it is never re-read to override a live apply — doing so is
    /// what made Settings' scale changes land one try late (the reload raced the store write and
    /// resurrected the previous value).
    pub fn apply_display_config(
        &mut self,
        new_conf: HashMap<String, Option<synoik_config::Output>>,
    ) {
        for (name, conf) in new_conf {
            match &conf {
                Some(output) => {
                    let applied = AppliedDisplayConfig {
                        scale: output.scale.map(|s| s.0),
                        transform: Some(ipc_transform_to_smithay(output.transform)),
                    };
                    self.synoik
                        .applied_display_config
                        .insert(name.clone(), applied);
                }
                // Output disabled: drop the override so a later enable re-derives from the store.
                None => {
                    self.synoik.applied_display_config.remove(&name);
                }
            }
            self.modify_output_config(&name, move |output| {
                if let Some(new_output) = conf {
                    *output = new_output;
                } else {
                    output.off = true;
                }
            });
        }
        self.reload_output_config();
    }

    pub fn refresh_ipc_outputs(&mut self) {
        if !self.synoik.ipc_outputs_changed {
            return;
        }
        self.synoik.ipc_outputs_changed = false;

        let _span = tracy_client::span!("State::refresh_ipc_outputs");

        for ipc_output in self.backend.ipc_outputs().lock().unwrap().values_mut() {
            let logical = self
                .synoik
                .global_space
                .outputs()
                .find(|output| output.name() == ipc_output.name)
                .map(logical_output);
            ipc_output.logical = logical;
        }

        self.synoik.on_ipc_outputs_changed();

        // The backlight device match depends on the connector set and on each connector's
        // `enabled` attribute, which flips on mode-set — so it rides the same funnel.
        self.refresh_backlights();

        let new_config = self.backend.ipc_outputs().lock().unwrap().clone();
        self.synoik
            .output_management_state
            .notify_changes(new_config);
    }

    pub fn open_screenshot_ui(&mut self, path: Option<String>) {
        if self.synoik.is_locked() || self.synoik.screenshot_ui.is_open() {
            return;
        }

        self.synoik.update_render_elements(None);

        // Capture the Output-target neutrals through the owned renderer first.
        let vk_neutrals = self
            .backend
            .with_vulkan_renderer(|vk| self.synoik.capture_screenshot_neutrals(vk))
            .unwrap_or_default();
        // Every window on every active workspace, frozen the same way — Window mode picks from
        // these, not from live windows.
        let window_shots = self
            .backend
            .with_vulkan_renderer(|vk| self.synoik.capture_screenshot_window_neutrals(vk))
            .unwrap_or_default();

        self.open_screenshot_ui_with(vk_neutrals, window_shots, path);
    }

    /// Open the picker around neutrals that have **already been captured**.
    ///
    /// Split from [`Self::open_screenshot_ui`] at exactly the renderer boundary: everything above
    /// needs a Vulkan device, everything from here down is CPU. A `ScreenshotNeutral` is plain
    /// `MemoryBuffer` pixels, so the headless corpus can hand-build a frozen screen and drive the
    /// real picker with no device at all (see `screenshot_ui_fixture` in `src/tests/gnome.rs`).
    pub fn open_screenshot_ui_with(
        &mut self,
        vk_neutrals: std::collections::HashMap<Output, [ScreenshotNeutral; RenderTarget::COUNT]>,
        window_shots: std::collections::HashMap<Output, Vec<crate::ui::screenshot_ui::WindowShot>>,
        path: Option<String>,
    ) {
        let default_output = self
            .synoik
            .output_under_cursor()
            .or_else(|| self.synoik.layout.active_output().cloned());
        let Some(default_output) = default_output else {
            return;
        };

        // The captures are already taken, so opening the UI must not depend on a renderer being
        // available here, or it would silently never open.
        let screenshots = self.synoik.capture_screenshots(vk_neutrals).collect();

        // Now that we captured the screenshots, clear grabs like drag-and-drop, etc.
        self.synoik.seat.get_pointer().unwrap().unset_grab(
            self,
            SERIAL_COUNTER.next_serial(),
            get_monotonic_time().as_millis() as u32,
        );
        if let Some(touch) = self.synoik.seat.get_touch() {
            touch.unset_grab(self);
        }

        let focused_window = self.synoik.layout.focus().map(|mapped| mapped.id().get());
        self.synoik.screenshot_ui.open(
            screenshots,
            window_shots,
            default_output,
            focused_window,
            path,
        );

        // Selection is the mode it opens in, so the crosshair is right — and it is all we can say
        // yet: the panel has no rect until the first bake, so `over_chrome` cannot answer. The
        // first motion refines it, which is also the first moment the panel is on screen to be
        // pointed at.
        self.synoik
            .cursor_manager
            .set_cursor_image(CursorImageStatus::Named(CursorIcon::Crosshair));
        self.synoik.queue_redraw_all();
    }

    pub fn handle_pick_color(
        &mut self,
        tx: async_channel::Sender<Option<synoik_ipc::PickedColor>>,
    ) {
        let pointer = self.synoik.seat.get_pointer().unwrap();
        let start_data = PointerGrabStartData {
            focus: None,
            button: 0,
            location: pointer.current_location(),
        };
        let grab = PickColorGrab::new(start_data);
        pointer.set_grab(self, grab, SERIAL_COUNTER.next_serial(), Focus::Clear);
        self.synoik.pick_color = Some(tx);
        self.synoik
            .cursor_manager
            .set_cursor_image(CursorImageStatus::Named(CursorIcon::Crosshair));
        self.synoik.queue_redraw_all();
    }

    /// Arm a delayed capture and dismiss the picker.
    ///
    /// The delay only means anything with the shell out of the way, so the picker goes now and the
    /// shot is taken from the **live** screen when the timer runs out. That is why every scrap of
    /// context the capture will need — the output, the target, the reply channel, the path — is
    /// lifted out here: none of it survives `close_screenshot_ui`.
    fn arm_delayed_shot(&mut self, delay: Duration, write_to_disk: bool, path: Option<String>) {
        let Some((output, target)) = self.synoik.screenshot_ui.pending_target() else {
            warn!("nothing to capture; not arming the delay");
            self.cancel_screenshot();
            return;
        };
        let show_pointer = self.synoik.screenshot_ui.show_pointer();

        // Taken *before* the close, which answers whatever is still pending with `None`. An armed
        // capture has not failed, so its caller must keep waiting for the real answer.
        let reply = self.synoik.interactive_screenshot_reply.take();

        self.arm_delayed_capture(
            delay,
            output,
            PendingAction::Shot {
                target,
                show_pointer,
                write_to_disk,
                path,
                reply,
            },
        );
    }

    /// Arm `action` to fire on `output` after `delay`, and dismiss the picker.
    fn arm_delayed_capture(&mut self, delay: Duration, output: Output, action: PendingAction) {
        self.synoik.close_screenshot_ui();
        self.synoik
            .cursor_manager
            .set_cursor_image(CursorImageStatus::default_named());

        // Replaces anything already armed rather than stacking: two countdowns on screen at once
        // would be two shots the user asked for once.
        self.cancel_pending_capture();

        let fires_at = self.synoik.clock.now_unadjusted() + delay;
        // Ticks every second so the countdown can redraw; the fire condition is the clock, not the
        // tick count, so a late or coalesced wakeup shortens the last tick instead of the delay.
        let timer = calloop::timer::Timer::from_duration(Duration::from_secs(1));
        let token = self
            .synoik
            .event_loop
            .insert_source(timer, move |_, _, state| state.tick_pending_capture())
            .map_err(|err| warn!("error arming the delayed capture: {err:?}"))
            .ok();
        let Some(token) = token else {
            action.dismiss();
            return;
        };

        self.synoik.pending_capture = Some(PendingCapture {
            output: output.downgrade(),
            action,
            fires_at,
            token,
        });
        self.synoik.queue_redraw_all();
    }

    /// Start a recording of whatever the picker has selected, and dismiss it.
    ///
    /// GNOME closes the UI **instantly** here rather than fading it, "so the fade-out doesn't get
    /// recorded" (`js/ui/screenshot.js:2035-2036`) — and the same reasoning is why a delay applies:
    /// it is armed through the same [`State::arm_delayed_capture`] path, so the countdown runs with
    /// the picker already gone.
    fn start_screencast_from_picker(&mut self) {
        // Screen mode selects the whole output, and a whole-output recording wants no crop at all —
        // the crop path relocates every frame into a smaller buffer for nothing.
        let crop = (self.synoik.screenshot_ui.capture_type() != CaptureType::Screen)
            .then(|| self.synoik.screenshot_ui.selection_rect_global())
            .flatten();
        let draw_cursor = self.synoik.screenshot_ui.show_pointer();
        let output = self.synoik.screenshot_ui.selection_output().cloned();

        if let (Some(delay), Some(output)) = (self.synoik.screenshot_ui.delay(), output.clone()) {
            self.arm_delayed_capture(delay, output, PendingAction::Cast { crop, draw_cursor });
            return;
        }

        self.synoik.close_screenshot_ui();
        self.synoik
            .cursor_manager
            .set_cursor_image(CursorImageStatus::default_named());
        self.synoik.queue_redraw_all();

        self.start_picker_recording(output, crop, draw_cursor);
    }

    /// The recorder call both the immediate and the delayed path end at.
    fn start_picker_recording(
        &mut self,
        output: Option<Output>,
        crop: Option<Rectangle<i32, Logical>>,
        draw_cursor: bool,
    ) {
        let Some(output) = output else {
            warn!("no output to record");
            return;
        };

        // GNOME's own template and folder (`js/ui/screenshot.js:2056-2065`), which is what
        // `default_recording_path` already encodes — so the picker's recordings land beside the
        // keybind's rather than in a second place.
        let base = self.synoik.recordings_base.clone();
        let path = match crate::recording::default_recording_path(base.as_deref()) {
            Ok(path) => path,
            Err(err) => {
                warn!("could not resolve the recording path: {err:?}");
                return;
            }
        };

        // 30fps is what `org.gnome.Shell.Screencast` defaults to when a caller omits `framerate`.
        if let Err(err) = self
            .synoik
            .start_native_recording(&output, path, 30, draw_cursor, crop)
        {
            warn!("could not start the recorder: {err:?}");
            return;
        }

        // Mark what is being recorded, for as long as it is (`_startScreencast`,
        // `js/ui/screenshot.js:2022-2032`). No crop means the whole output, which GNOME marks with
        // the monitor's own rect — the shades then have nothing to cover.
        let scale = output.current_scale().fractional_scale();
        let rect = match crop {
            Some(crop) => {
                let local = Rectangle::new(crop.loc - output.current_location(), crop.size);
                local.to_f64().to_physical_precise_round(scale)
            }
            None => Rectangle::from_size(crate::utils::output_size(&output))
                .to_physical_precise_round(scale),
        };
        self.synoik.cast_area_indicator.set(output, rect);

        self.synoik.queue_redraw_all();
    }

    /// One second of the countdown. Fires the capture when the clock says so.
    ///
    /// `pub(crate)` for the corpus: the cancellation rules live here, and the alternative is a test
    /// that reimplements them.
    pub(crate) fn tick_pending_capture(&mut self) -> calloop::timer::TimeoutAction {
        let Some(pending) = &self.synoik.pending_capture else {
            return calloop::timer::TimeoutAction::Drop;
        };

        // A lock arriving mid-countdown cancels: the delay was armed against a screen the user
        // could see, and firing into a lock screen would capture what the lock exists to hide.
        if self.synoik.is_locked() || self.synoik.screen_shield.is_active() {
            debug!("screen locked mid-countdown; dropping the delayed capture");
            self.cancel_pending_capture();
            return calloop::timer::TimeoutAction::Drop;
        }

        // The output is held weakly, so unplugging it mid-countdown lands here.
        if pending.output.upgrade().is_none() {
            debug!("the delayed capture's output is gone; dropping it");
            self.cancel_pending_capture();
            return calloop::timer::TimeoutAction::Drop;
        }

        if self.synoik.clock.now_unadjusted() < pending.fires_at {
            self.synoik.queue_redraw_all();
            return calloop::timer::TimeoutAction::ToDuration(Duration::from_secs(1));
        }

        self.fire_pending_capture();
        calloop::timer::TimeoutAction::Drop
    }

    /// Do what the delay was armed for, against the live screen.
    fn fire_pending_capture(&mut self) {
        let Some(pending) = self.synoik.pending_capture.take() else {
            return;
        };
        // The timer that got us here is dropped by its own return value; removing it as well would
        // be a double removal, so the token is deliberately left alone.
        let PendingCapture { output, action, .. } = pending;
        let Some(output) = output.upgrade() else {
            action.dismiss();
            return;
        };

        // The card is gone by the time anything is rendered for the capture (it reads
        // `pending_capture`, which is now `None`), and this is the redraw that clears it.
        self.synoik.queue_redraw_all();

        match action {
            PendingAction::Cast { crop, draw_cursor } => {
                self.start_picker_recording(Some(output), crop, draw_cursor)
            }
            PendingAction::Shot {
                target,
                show_pointer,
                write_to_disk,
                path,
                reply,
            } => {
                // The reply travels into `save_screenshot`, which answers it once the PNG lands.
                // The spare is for the paths that never get that far — the channel is used once, so
                // whichever sends first is the answer.
                let spare = reply.clone();
                let res = self.backend.with_vulkan_renderer(|renderer| match target {
                    PendingTarget::Window(id) => {
                        let found = self
                            .synoik
                            .layout
                            .windows()
                            .find(|(_, m)| m.id().get() == id);
                        let Some((_, mapped)) = found else {
                            return Err(anyhow::anyhow!("the window is gone"));
                        };
                        self.synoik.screenshot_window(
                            renderer,
                            &output,
                            mapped,
                            write_to_disk,
                            show_pointer,
                            path,
                            reply,
                        )
                    }
                    PendingTarget::Area(rect) => self.synoik.screenshot_area(
                        renderer,
                        &output,
                        rect,
                        write_to_disk,
                        show_pointer,
                        path,
                        reply,
                    ),
                });

                let failed = match res {
                    Some(Ok(())) => None,
                    Some(Err(err)) => Some(format!("{err:?}")),
                    None => Some(String::from("no renderer available")),
                };
                if let Some(err) = failed {
                    warn!("error taking the delayed screenshot: {err}");
                    if let Some(tx) = spare {
                        let _ = tx.send_blocking(None);
                    }
                    return;
                }

                // `save_screenshot` owns the reply from here; it answers when the PNG lands.
                drop(spare);
            }
        }
    }

    /// Disarm whatever is counting down, answering its caller with a dismissal.
    pub fn cancel_pending_capture(&mut self) -> bool {
        let Some(pending) = self.synoik.pending_capture.take() else {
            return false;
        };
        self.synoik.event_loop.remove(pending.token);
        pending.action.dismiss();
        self.synoik.queue_redraw_all();
        true
    }

    pub fn confirm_screenshot(&mut self, write_to_disk: bool) {
        let ScreenshotUi::Open { path, .. } = &mut self.synoik.screenshot_ui else {
            return;
        };
        let path = path.take();

        // Cast mode takes no picture at all: the capture button starts a recording, and everything
        // below — the frozen neutral, the clipboard, the D-Bus screenshot reply — is beside the
        // point (`_onCaptureButtonClicked`, `js/ui/screenshot.js:2085-2095`).
        if self.synoik.screenshot_ui.mode() == CaptureMode::Cast {
            self.start_screencast_from_picker();
            return;
        }

        // A `SelectArea` caller wanted coordinates, not a picture: hand them over and save
        // nothing. Answered before the close, which would otherwise answer `None`.
        let selecting = self.synoik.select_area_reply.is_some();
        if selecting {
            // A delay has nothing to apply to here: the caller wants coordinates, and it already
            // has them.
            let rect = self.synoik.screenshot_ui.selection_rect_global();
            self.synoik.answer_select_area(rect);
        } else if let Some(delay) = self.synoik.screenshot_ui.delay() {
            self.arm_delayed_shot(delay, write_to_disk, path);
            return;
        } else {
            // Save from the frozen-screen neutral CPU buffer: a pure crop + pointer composite, no
            // render or readback. The neutral is captured when the UI opens, so a missing one means
            // that capture failed (warned there) — fail closed rather than save a wrong screenshot.
            match self.synoik.screenshot_ui.capture_from_neutral() {
                Some((size, pixels)) => {
                    let reply = self.synoik.interactive_screenshot_reply.take();
                    if let Err(err) =
                        self.synoik
                            .save_screenshot(size, pixels, write_to_disk, path, reply)
                    {
                        warn!("error saving screenshot: {err:?}");
                    }
                }
                None => warn!("no frozen-screen capture to save the screenshot from"),
            }
        }

        self.synoik.close_screenshot_ui();
        self.synoik
            .cursor_manager
            .set_cursor_image(CursorImageStatus::default_named());
        self.synoik.queue_redraw_all();
    }

    /// Stop every live recording and report the ones that produced a file.
    ///
    /// The notification comes from *here* rather than from `Synoik::stop_screen_recordings`, and so
    /// does not fire for a `org.gnome.Shell.Screencast.StopScreencast` caller: in GNOME the shell
    /// UI is what notifies (`_showNotification`, `js/ui/screenshot.js:2109-2144`) and the recorder
    /// service does not, so a client driving the recorder gets to do its own reporting.
    pub fn stop_screen_recordings(&mut self) {
        for path in self.synoik.stop_screen_recordings() {
            self.show_screencast_notification(path);
        }
    }

    /// The "Screencast recorded" notification (`js/ui/screenshot.js:2109-2185`) — same source and
    /// shape as the screenshot one, minus the image (there is no still to show) and with the body
    /// and button pointing at the video.
    pub fn show_screencast_notification(&mut self, path: PathBuf) {
        use crate::notifications::{NotificationIcon, ShellAction, ShellNotifyRequest, Urgency};

        let req = ShellNotifyRequest {
            source: crate::notifications::SHELL_SOURCE_SCREENSHOT,
            source_title: String::from("Screenshot"),
            source_icon: Some(NotificationIcon::Themed(String::from(
                "applets-screenshooter-symbolic",
            ))),
            title: String::from("Screencast recorded"),
            body: String::from("Click here to view the video"),
            icon: None,
            actions: vec![(
                String::from("Show in Files"),
                ShellAction::ShowInFiles(path.clone()),
            )],
            default_action: Some(ShellAction::OpenFile(path)),
            urgency: Urgency::Normal,
            transient: true,
        };

        let now = self.synoik.clock.now_unadjusted();
        let show_banners = !self.synoik.gnome_settings.quick_toggles.do_not_disturb;
        let (_, effects) = self.synoik.notifications.add_shell(req, show_banners, now);
        self.apply_notification_effects(effects);
    }

    /// The "Screenshot captured" notification (`js/ui/screenshot.js:2386-2420`).
    ///
    /// Carries the shot itself as its image, and — when it went to disk — a **Show in Files**
    /// button plus a body click that opens it. `None` means the capture went to the clipboard
    /// only, and GNOME drops both of those with it (`disableSaveToDisk`, `:2400`).
    pub fn show_screenshot_notification(
        &mut self,
        path: Option<PathBuf>,
        thumbnail: Option<Arc<PixelIcon>>,
    ) {
        use crate::notifications::{NotificationIcon, ShellAction, ShellNotifyRequest, Urgency};

        let mut actions = Vec::new();
        let mut default_action = None;
        if let Some(path) = &path {
            actions.push((
                String::from("Show in Files"),
                ShellAction::ShowInFiles(path.clone()),
            ));
            default_action = Some(ShellAction::OpenFile(path.clone()));
        }

        let req = ShellNotifyRequest {
            source: crate::notifications::SHELL_SOURCE_SCREENSHOT,
            source_title: String::from("Screenshot"),
            source_icon: Some(NotificationIcon::Themed(String::from(
                "applets-screenshooter-symbolic",
            ))),
            title: String::from("Screenshot captured"),
            body: String::from("You can paste the image from the clipboard"),
            // The shot itself, downscaled on the encoding thread. A clipboard-only capture still
            // gets one — there is no file, but there is very much an image.
            icon: thumbnail.map(NotificationIcon::Pixels),
            actions,
            default_action,
            urgency: Urgency::Normal,
            transient: true,
        };

        let now = self.synoik.clock.now_unadjusted();
        let show_banners = !self.synoik.gnome_settings.quick_toggles.do_not_disturb;
        let (_, effects) = self.synoik.notifications.add_shell(req, show_banners, now);
        self.apply_notification_effects(effects);
    }

    /// Dismiss the screenshot UI without capturing anything.
    ///
    /// Shared by Escape and the panel's close button — both must go through
    /// `close_screenshot_ui`, which is what answers a `SelectArea`/`InteractiveScreenshot` caller
    /// that would otherwise wait for its timeout.
    pub fn cancel_screenshot(&mut self) {
        if !self.synoik.screenshot_ui.is_open() {
            return;
        }

        self.synoik.close_screenshot_ui();
        self.synoik
            .cursor_manager
            .set_cursor_image(CursorImageStatus::default_named());
        self.synoik.queue_redraw_all();
    }

    /// Feed the picker a pointer motion and put the cursor where that motion says it belongs.
    ///
    /// Every motion goes through here rather than calling `pointer_motion` directly, for the same
    /// reason the release does: the cursor is a consequence of the hit test, and a hit test whose
    /// consequence is applied at three of four call sites is a bug waiting for the fourth.
    /// Returns whether anything changed, so callers keep their own redraw scope.
    pub fn handle_screenshot_ui_motion(
        &mut self,
        point: Point<i32, Physical>,
        slot: Option<smithay::backend::input::TouchSlot>,
    ) -> bool {
        let changed = self.synoik.screenshot_ui.pointer_motion(point, slot);
        self.sync_screenshot_ui_cursor();
        changed
    }

    /// Feed the picker a press, and perform whatever it asks for.
    ///
    /// Returns whether the caller owes a redraw. The warp goes through `move_cursor` — the same
    /// path `warp-mouse-to-focus` uses — so the pointer really moves rather than the picker merely
    /// pretending it did.
    pub fn handle_screenshot_ui_pointer_down(
        &mut self,
        output: Output,
        point: Point<i32, Physical>,
        slot: Option<smithay::backend::input::TouchSlot>,
        move_existing: bool,
    ) -> bool {
        let Some(down) =
            self.synoik
                .screenshot_ui
                .pointer_down(output.clone(), point, slot, move_existing)
        else {
            return false;
        };

        if let PointerDown::WarpTo(target) = down {
            if let Some(geo) = self.synoik.global_space.output_geometry(&output) {
                let scale = output.current_scale().fractional_scale();
                let global = target.to_f64().to_logical(scale) + geo.loc.to_f64();
                self.move_cursor(global);
            }
        }
        self.sync_screenshot_ui_cursor();
        true
    }

    /// Apply the picker's current cursor. Also the way a *click* that changes mode gets a fresh
    /// cursor without the pointer having to move — switching to Screen mode under a parked pointer
    /// must drop the crosshair there and then.
    fn sync_screenshot_ui_cursor(&mut self) {
        if !self.synoik.screenshot_ui.is_open() {
            return;
        }
        let icon = self.synoik.screenshot_ui.cursor_icon();
        self.synoik
            .cursor_manager
            .set_cursor_image(CursorImageStatus::Named(icon));
    }

    /// Switch the picker's capture type, as its type row would. A no-op unless it is open.
    pub fn set_screenshot_capture_type(&mut self, ty: CaptureType) {
        if !self.synoik.screenshot_ui.is_open() {
            return;
        }
        self.synoik.screenshot_ui.set_capture_type(ty);
        // The type decides what the pointer means where it already is.
        self.sync_screenshot_ui_cursor();
        self.synoik.queue_redraw_all();
    }

    /// Flip the picker between shot and cast, as its pill would. A no-op unless it is open.
    pub fn toggle_screenshot_capture_mode(&mut self) {
        if !self.synoik.screenshot_ui.is_open() {
            return;
        }
        let next = match self.synoik.screenshot_ui.mode() {
            CaptureMode::Shot => CaptureMode::Cast,
            CaptureMode::Cast => CaptureMode::Shot,
        };
        self.synoik.screenshot_ui.set_mode(next);
        self.sync_screenshot_ui_cursor();
        self.synoik.queue_redraw_all();
    }

    /// Act on a release over the screenshot UI's control panel.
    pub fn handle_screenshot_ui_pointer_up(&mut self, up: PointerUp) {
        match up {
            PointerUp::Capture => self.confirm_screenshot(true),
            PointerUp::Close => self.cancel_screenshot(),
            PointerUp::Redraw => {
                // A click can change the capture type or the mode, and both change what the cursor
                // over that very spot should be.
                self.sync_screenshot_ui_cursor();
                self.synoik.queue_redraw_all();
            }
        }
    }

    /// Snapshots a window's layout state into the session store, if it is registered in a session.
    ///
    /// Mutter's only save trigger, via `on_window_unmanaging`
    /// (`meta-wayland-xdg-session.c:262-276`): state is captured when the window goes away, not
    /// continuously as it moves. Must run *before* `Layout::remove_window` — after it there is
    /// nothing left to read.
    pub fn save_session_toplevel(&mut self, window: &Window) {
        let Some(toplevel) = window.toplevel() else {
            return;
        };
        let toplevel = toplevel.xdg_toplevel().clone();
        let Some((session_id, name)) = self
            .synoik
            .session_manager_state
            .registration_for(&toplevel)
        else {
            return;
        };

        let Some(record) = self.session_record_for(window) else {
            return;
        };

        self.synoik
            .session_manager_state
            .store
            .save_toplevel(&session_id, &name, record);
        self.schedule_session_save();
    }

    /// The store record for a mapped window: sizing mode, global floating rect, workspace index.
    fn session_record_for(&self, window: &Window) -> Option<ToplevelRecord> {
        let snapshot = self.synoik.layout.session_snapshot(window)?;

        let state = match snapshot.sizing_mode {
            SizingMode::Normal => WindowState::Floating,
            SizingMode::Maximized => WindowState::Maximized,
            SizingMode::Fullscreen => WindowState::Fullscreen,
        };

        // The layout works in output-local coordinates; the store is global, so that restore can
        // pick the output back out of the rect.
        let origin = snapshot
            .output
            .and_then(|output| self.synoik.global_space.output_geometry(output))
            .map_or_else(Point::default, |geo| geo.loc.to_f64());

        let floating_rect = snapshot.floating_rect.map(|rect| {
            let loc = rect.loc + origin;
            [
                loc.x.round() as i32,
                loc.y.round() as i32,
                rect.size.w.round() as i32,
                rect.size.h.round() as i32,
            ]
        });

        Some(ToplevelRecord {
            state: Some(state.as_raw()),
            floating_rect,
            workspace: Some(snapshot.workspace_idx as u32),
            ..Default::default()
        })
    }

    /// Snapshots every registered window that is still mapped, for the shutdown save.
    ///
    /// Mutter gets this for free: `meta_display_close` unmanages every window (`display.c:1052`)
    /// before the context's synchronous save (`meta-context-main.c:445`), so each one goes through
    /// the unmap path above. We tear down without unmapping, so the sweep is explicit — without it
    /// the flagship case, logging out with windows open, would save nothing.
    pub fn save_session_toplevels_still_mapped(&mut self) {
        self.save_live_session_toplevels_matching(None);
    }

    /// Snapshots every still-mapped registered toplevel, or only one session's when `only` is set.
    ///
    /// Two callers with the same need: shutdown, which has to sweep everything because we tear down
    /// without unmapping, and `xdg_session_v1.destroy`, which has to freeze one session's state
    /// before its registrations go away.
    pub(crate) fn save_live_session_toplevels_matching(&mut self, only: Option<&str>) {
        for (session_id, name, toplevel) in self.synoik.session_manager_state.live_registrations() {
            if only.is_some_and(|wanted| wanted != session_id) {
                continue;
            }
            let Some(window) = self
                .synoik
                .layout
                .windows()
                .find(|(_, mapped)| mapped.toplevel().xdg_toplevel() == &toplevel)
                .map(|(_, mapped)| mapped.window.clone())
            else {
                continue;
            };

            let Some(record) = self.session_record_for(&window) else {
                continue;
            };
            self.synoik
                .session_manager_state
                .store
                .save_toplevel(&session_id, &name, record);
        }
    }

    pub fn store_unmap_snapshot(&mut self, window: &Window, output: Option<&Output>) {
        let appearance = self.synoik.appearance();
        // The unmapping tile may have an xray background, in which case we will render xray
        // elements, so they need to be updated.
        self.synoik.update_xray_render_elements(output);

        self.backend.with_vulkan_renderer(|renderer| {
            if let Some(output) = output {
                let mut ctx = RenderCtx {
                    target: RenderTarget::Output,
                    renderer,
                    xray: None,
                    appearance: Some(appearance),
                };

                self.synoik.fill_xray_elements(ctx.r(), output);

                // If any background layer has block_out_from, also fill the Screencast xray
                // buffer so the unmap snapshot can render a buffer with blocked-out background.
                //
                // This will be used in Tile::render_snapshot().
                let has_blocked_out = self.synoik.has_blocked_out_background_layers(output);
                if has_blocked_out {
                    let screencast_ctx = RenderCtx {
                        target: RenderTarget::Screencast,
                        ..ctx.r()
                    };
                    self.synoik.fill_xray_elements(screencast_ctx, output);
                }

                let state = self.synoik.output_state.get_mut(output).unwrap();
                self.synoik.layout.store_unmap_snapshot(
                    renderer,
                    Some(&mut state.xray),
                    has_blocked_out,
                    window,
                );

                self.synoik.clear_xray_elements(output);
            } else {
                self.synoik
                    .layout
                    .store_unmap_snapshot(renderer, None, false, window);
            }
        });
    }

    #[cfg(not(feature = "xdp-gnome-screencast"))]
    pub fn set_dynamic_cast_target(&mut self, _target: CastTarget) {}

    pub fn on_screen_shot_msg(
        &mut self,
        to_screenshot: &async_channel::Sender<SynoikToScreenshot>,
        msg: ScreenshotToSynoik,
    ) {
        match msg {
            ScreenshotToSynoik::TakeScreenshot {
                include_cursor,
                path,
            } => {
                self.handle_take_screenshot(to_screenshot, include_cursor, None, path);
            }
            ScreenshotToSynoik::TakeScreenshotArea { area, path } => {
                let (x, y, w, h) = area;
                let area = Rectangle::new(Point::from((x, y)), Size::from((w, h)));
                self.handle_take_screenshot(to_screenshot, false, Some(area), path);
            }
            ScreenshotToSynoik::TakeScreenshotWindow {
                include_cursor,
                path,
            } => {
                self.handle_take_screenshot_window(to_screenshot, include_cursor, path);
            }
            ScreenshotToSynoik::SelectArea(tx) => {
                self.handle_select_area(tx);
            }
            ScreenshotToSynoik::Interactive(tx) => {
                self.handle_interactive_screenshot(tx);
            }
            ScreenshotToSynoik::FlashArea { area } => {
                let (x, y, w, h) = area;
                let area = Rectangle::new(Point::from((x, y)), Size::from((w, h)));
                self.synoik
                    .flashspot
                    .fire(area, self.synoik.clock.now_unadjusted());
                self.synoik.queue_redraw_all();
            }
            ScreenshotToSynoik::PickColor(tx) => {
                self.handle_pick_color(tx);
            }
        }
    }

    fn handle_take_screenshot(
        &mut self,
        to_screenshot: &async_channel::Sender<SynoikToScreenshot>,
        include_cursor: bool,
        area: Option<Rectangle<i32, Logical>>,
        path: Option<PathBuf>,
    ) {
        let _span = tracy_client::span!("TakeScreenshot");

        let rv = self.backend.with_vulkan_renderer(|renderer| {
            Self::take_screenshot_with_renderer(
                &mut self.synoik,
                renderer,
                include_cursor,
                area,
                path,
                to_screenshot,
            )
        });

        if rv.is_none() {
            let msg = SynoikToScreenshot::ScreenshotResult(None);
            if let Err(err) = to_screenshot.send_blocking(msg) {
                warn!("error sending None to screenshot: {err:?}");
            }
        }
    }

    /// `SelectArea` — the picker, opened to hand back coordinates rather than save a file.
    fn handle_select_area(&mut self, tx: crate::dbus::gnome_shell_screenshot::SelectAreaReply) {
        self.synoik.select_area_reply = Some(tx);
        self.open_screenshot_ui(None);

        // The picker refuses to open when the screen is locked or it is already up. Answering now
        // is the difference between a caller that gets an error and one that hangs until its
        // D-Bus timeout.
        if !self.synoik.screenshot_ui.is_open() {
            self.synoik.answer_select_area(None);
        }
    }

    /// `InteractiveScreenshot` — the shell's own picker, answering with the saved file's URI.
    fn handle_interactive_screenshot(
        &mut self,
        tx: crate::dbus::gnome_shell_screenshot::InteractiveReply,
    ) {
        self.synoik.interactive_screenshot_reply = Some(tx);
        self.open_screenshot_ui(None);

        // Same reason as `SelectArea`: a picker that refuses to open must still answer, or the
        // caller blocks until its D-Bus timeout with nothing on screen to explain why.
        if !self.synoik.screenshot_ui.is_open() {
            self.synoik.answer_interactive_screenshot(None);
        }
    }

    /// `ScreenshotWindow` captures the *focused* window, like GNOME's.
    fn handle_take_screenshot_window(
        &mut self,
        to_screenshot: &async_channel::Sender<SynoikToScreenshot>,
        include_cursor: bool,
        path: Option<PathBuf>,
    ) {
        let _span = tracy_client::span!("TakeScreenshotWindow");

        let target = self
            .synoik
            .layout
            .focus()
            .map(|mapped| mapped.id())
            .and_then(|id| {
                let output = self.synoik.layout.active_output().cloned()?;
                Some((id, output))
            });

        let rv = target.and_then(|(id, output)| {
            let to_screenshot = to_screenshot.clone();
            let on_done = move |path| {
                let msg = SynoikToScreenshot::ScreenshotResult(Some(path));
                if let Err(err) = to_screenshot.send_blocking(msg) {
                    warn!("error sending path to screenshot: {err:?}");
                }
            };

            self.backend.with_vulkan_renderer(|renderer| {
                let Some(mapped) = self
                    .synoik
                    .layout
                    .windows()
                    .find(|(_, m)| m.id() == id)
                    .map(|(_, m)| m)
                else {
                    return false;
                };
                match self.synoik.screenshot_window_to_path(
                    renderer,
                    &output,
                    mapped,
                    include_cursor,
                    path,
                    on_done,
                ) {
                    Ok(()) => true,
                    Err(err) => {
                        warn!("error taking a window screenshot: {err:?}");
                        false
                    }
                }
            })
        });

        if rv != Some(true) {
            let msg = SynoikToScreenshot::ScreenshotResult(None);
            if let Err(err) = to_screenshot.send_blocking(msg) {
                warn!("error sending None to screenshot: {err:?}");
            }
        }
    }

    fn take_screenshot_with_renderer(
        synoik: &mut Synoik,
        renderer: &mut VulkanRenderer,
        include_cursor: bool,
        area: Option<Rectangle<i32, Logical>>,
        path: Option<PathBuf>,
        to_screenshot: &async_channel::Sender<SynoikToScreenshot>,
    ) {
        let on_done = {
            let to_screenshot = to_screenshot.clone();
            move |path| {
                let msg = SynoikToScreenshot::ScreenshotResult(Some(path));
                if let Err(err) = to_screenshot.send_blocking(msg) {
                    warn!("error sending path to screenshot: {err:?}");
                }
            }
        };

        let res = synoik.screenshot_to_path(renderer, include_cursor, area, path, on_done);

        if let Err(err) = res {
            warn!("error taking a screenshot: {err:?}");

            let msg = SynoikToScreenshot::ScreenshotResult(None);
            if let Err(err) = to_screenshot.send_blocking(msg) {
                warn!("error sending None to screenshot: {err:?}");
            }
        }
    }

    pub fn on_introspect_msg(
        &mut self,
        to_introspect: &async_channel::Sender<SynoikToIntrospect>,
        msg: IntrospectToSynoik,
    ) {
        let reply = match msg {
            IntrospectToSynoik::GetWindows => {
                SynoikToIntrospect::Windows(self.introspect_windows())
            }
            IntrospectToSynoik::GetRunningApplications => {
                SynoikToIntrospect::RunningApplications(self.introspect_running_applications())
            }
            IntrospectToSynoik::GetScreenSize => {
                // `global.screen_width/height` (`introspect.js:198-199`) is the union bounding box
                // of every output, not one monitor's size.
                let bounds = self
                    .synoik
                    .global_space
                    .outputs()
                    .filter_map(|output| self.synoik.global_space.output_geometry(output))
                    .reduce(|acc, geo| acc.merge(geo))
                    .unwrap_or_default();
                SynoikToIntrospect::ScreenSize(
                    bounds.loc.x + bounds.size.w,
                    bounds.loc.y + bounds.size.h,
                )
            }
            IntrospectToSynoik::GetAnimationsEnabled => {
                SynoikToIntrospect::AnimationsEnabled(self.synoik.gnome_settings.enable_animations)
            }
        };

        if let Err(err) = to_introspect.send_blocking(reply) {
            warn!("error replying to introspect: {err:?}");
        }
    }

    /// `GetWindows` (`introspect.js:135-182`).
    fn introspect_windows(&mut self) -> HashMap<u64, gnome_shell_introspect::WindowProperties> {
        use crate::utils::with_toplevel_role;

        let _span = tracy_client::span!("GetWindows");

        let mut windows = HashMap::new();

        // Not a window: a synthetic entry that exists only so the portal's picker can offer a cast
        // whose target is chosen *later*, and can then be changed live with
        // `Action::SetDynamicCastWindow` and friends without reopening the share dialog. GNOME has
        // no equivalent — mutter binds a stream to its target at `RecordWindow` time — so this is
        // an additional capability we keep rather than one of niri's ways of doing a GNOME thing.
        //
        // The label is deliberately product-neutral; the app id still carries niri's, pending the
        // wider branding decision.
        #[cfg(feature = "xdp-gnome-screencast")]
        windows.insert(
            self.synoik.casting.dynamic_cast_id_for_portal.get(),
            gnome_shell_introspect::WindowProperties {
                app_id: String::from("rs.bxt.synoik.desktop"),
                client_type: gnome_shell_introspect::CLIENT_TYPE_WAYLAND,
                is_hidden: false,
                has_focus: false,
                width: 0,
                height: 0,
                title: Some(String::from(DYNAMIC_CAST_TARGET_LABEL)),
                wm_class: None,
            },
        );

        let focused = self.synoik.layout.focus().map(|m| m.id());
        // `MetaWindow::hidden` is set by `meta_window_hide` (`window.c:2669-2674`) — a window that
        // should not be showing right now. We have no minimize, so the only such window is one on
        // a workspace that is not its output's active one.
        let visible: std::collections::HashSet<_> = self
            .synoik
            .layout
            .monitors()
            .map(|mon| mon.active_workspace_ref().id())
            .collect();
        // Window -> desktop id, taken from the app system's own grouping rather than re-derived
        // here: `app-id` must be the same resolved desktop id the dash and the switcher use, or
        // the chooser looks up an icon nothing else agrees with.
        let desktop_ids: HashMap<_, _> = self
            .synoik
            .app_system
            .running()
            .iter()
            .flat_map(|app| app.windows.iter().map(|w| (w.id, app.id.clone())))
            .collect();

        self.synoik
            .layout
            .with_windows(|mapped, _, workspace, layout| {
                let id = mapped.id();
                let (w, h) = layout.window_size;
                let props = with_toplevel_role(mapped.toplevel(), |role| {
                    let app_id = role.app_id.clone();
                    gnome_shell_introspect::WindowProperties {
                        // The **desktop id**, resolved through the app system — the chooser looks
                        // the icon up by it. Before the app-lifecycle port
                        // this was `{app_id}.desktop`, which is why the
                        // portal's window list had no icons.
                        app_id: desktop_ids.get(&id).cloned().unwrap_or_default(),
                        client_type: gnome_shell_introspect::CLIENT_TYPE_WAYLAND,
                        is_hidden: workspace.is_some_and(|ws| !visible.contains(&ws)),
                        has_focus: Some(id) == focused,
                        width: w.max(0) as u32,
                        height: h.max(0) as u32,
                        title: role.title.clone().filter(|t| !t.is_empty()),
                        wm_class: app_id,
                    }
                });

                windows.insert(id.get(), props);
            });

        windows
    }

    /// `GetRunningApplications` (`introspect.js:73-133`).
    ///
    /// GNOME keys the map by desktop id and sends an *empty* dict for each app, adding
    /// `active-on-seats` only to the focused one (`:86-95`).
    fn introspect_running_applications(
        &mut self,
    ) -> HashMap<String, gnome_shell_introspect::AppProperties> {
        let _span = tracy_client::span!("GetRunningApplications");

        let focused = self.synoik.layout.focus().map(|m| m.id());
        let active = focused.and_then(|id| {
            self.synoik
                .app_system
                .running()
                .iter()
                .find(|app| app.windows.iter().any(|w| w.id == id))
                .map(|app| app.id.clone())
        });

        self.synoik
            .app_system
            .running()
            .iter()
            .map(|app| {
                let props = gnome_shell_introspect::AppProperties {
                    active_on_seats: (Some(&app.id) == active.as_ref())
                        // `seatName` is the literal 'seat0' upstream (`introspect.js:77`).
                        .then(|| vec![String::from("seat0")]),
                };
                (app.id.clone(), props)
            })
            .collect()
    }

    pub fn on_login1_msg(&mut self, msg: Login1ToSynoik) {
        match msg {
            Login1ToSynoik::LidClosedChanged(is_closed) => {
                trace!("login1 lid {}", if is_closed { "closed" } else { "opened" });
                self.set_lid_closed(is_closed);
            }
            // gdm authenticated us on its own VT, or someone ran `loginctl unlock-session`.
            // GNOME wires these straight to the shield (`screenShield.js`'s `_loginSession`
            // handlers), and without `Unlock` a session unlocked from gdm's login screen switches
            // VT back and sits there still locked.
            Login1ToSynoik::SessionLock(lock) => {
                let now = crate::utils::get_monotonic_time();
                let effects = if lock {
                    match self.synoik.screen_shield.lock(now, false) {
                        Ok(effects) => effects,
                        Err(crate::screen_shield::LockRefused::LockedDown) => {
                            debug!("screen lock is locked down, ignoring logind Lock");
                            return;
                        }
                    }
                } else {
                    self.synoik.screen_shield.deactivate()
                };
                self.apply_shield_effects(effects);
            }
            Login1ToSynoik::SessionActive(active) => {
                self.synoik.session_active = active;
                self.synoik.sync_sleep_inhibitor();
            }
            // The last moment before the machine goes down — logind is holding the suspend on our
            // delay inhibitor, and `apply_shield_effects` releases it once we have locked *and* the
            // curtain has reached the screen.
            Login1ToSynoik::PrepareForSleep(about_to_suspend) => {
                let now = crate::utils::get_monotonic_time();
                // Armed before the lock, because that is when the question can still be asked: the
                // curtain has to travel unless it is already down. Arming after would see the
                // descent that `prepare_for_sleep` just started and never distinguish the two.
                // `apply_shield_effects` ends in `sync_sleep_inhibitor`, which reads this.
                if about_to_suspend {
                    if !self.synoik.shield_curtain_landed() {
                        self.synoik.arm_shield_present_wait();
                    }
                } else {
                    // A suspend can be called off — another delay inhibitor may refuse it, and
                    // logind then emits `false` without the machine ever going down. A wait left
                    // armed there would hold the fd until its deadline for nothing.
                    self.synoik.clear_shield_present_wait();

                    // Start the idle clock over (`prepare_for_sleep_cb`,
                    // `meta-backend.c:1006-1023`, and the native backend's `resume`,
                    // `meta-backend-native.c:1027-1028`). `CLOCK_MONOTONIC` does not tick across a
                    // suspend, so without this the seat comes back holding whatever idle time it
                    // went down with — and gnome-settings-daemon, reading `GetIdletime`, dims,
                    // blanks or suspends again straight away instead of starting its countdown.
                    self.synoik.reset_idletime(now);
                }

                let effects =
                    self.synoik
                        .screen_shield
                        .prepare_for_sleep(about_to_suspend, now, false);
                self.apply_shield_effects(effects);

                // A shield that did not go down (`lock-enabled` off, or locked down) is never going
                // to present the frame we just armed a wait for. The inhibitor is already released
                // in that case — the settings it hangs on are the same ones — so this only saves a
                // timer and the deadline warning it would log.
                if about_to_suspend && !self.synoik.screen_shield.is_active() {
                    self.synoik.clear_shield_present_wait();
                }
            }
            Login1ToSynoik::BrightnessWriteDone { connector, outcome } => {
                let Some(tty) = self.backend.tty_checked() else {
                    return;
                };
                // The serializer hands back the one write a drag queued up while this one was in
                // flight (`meta-backlight.c:139-148`).
                if let Some(write) = tty.backlights.write_finished(&connector, outcome) {
                    self.send_backlight_write(write);
                }
            }
        }
    }

    /// A `backlight`-subsystem uevent: an external brightness change, or a device coming or going.
    pub fn on_backlight_uevent(&mut self, event: &crate::backend::backlight::BacklightUevent) {
        let Some(tty) = self.backend.tty_checked() else {
            return;
        };
        if !tty.backlights.handle_uevent(event) {
            return;
        }

        let snapshot = tty.backlights.snapshot();
        self.synoik.backlight = snapshot;
        trace!("backlight changed: {:?}", self.synoik.backlight);

        // GNOME's `backlights-changed` — the scales adopt a change made behind our back, and one
        // that was not ours cancels idle dimming (`brightnessManager.js:194-200`).
        self.sync_brightness(|manager, snapshot| manager.backlights_changed(snapshot));
    }

    /// Run one pass of the brightness algebra and push whatever it wants written to the hardware.
    ///
    /// The manager is moved out for the pass because it reads the snapshot that lives beside it.
    fn sync_brightness(
        &mut self,
        f: impl FnOnce(
            &mut crate::brightness::BrightnessManager,
            &crate::backlight::BacklightSnapshot,
        ) -> crate::brightness::BrightnessUpdate,
    ) {
        self.sync_brightness_inner(false, f);
    }

    /// [`sync_brightness`](Self::sync_brightness) for a change the **user** made — a slider drag
    /// or a brightness key. gnome-shell's manager emits `user-update` for exactly those
    /// (`brightnessManager.js:151-158,172-179`) and `org.gnome.Shell.Brightness` turns it into
    /// `BrightnessChanged`, which is how the ambient-light loop learns to back off. Changes that
    /// came *from* gsd-power, from the hardware, or from a monitor hotplug must not emit it, or
    /// the two would chase each other.
    fn sync_brightness_user(
        &mut self,
        f: impl FnOnce(
            &mut crate::brightness::BrightnessManager,
            &crate::backlight::BacklightSnapshot,
        ) -> crate::brightness::BrightnessUpdate,
    ) {
        self.sync_brightness_inner(true, f);
    }

    fn sync_brightness_inner(
        &mut self,
        user: bool,
        f: impl FnOnce(
            &mut crate::brightness::BrightnessManager,
            &crate::backlight::BacklightSnapshot,
        ) -> crate::brightness::BrightnessUpdate,
    ) {
        let mut manager = std::mem::take(&mut self.synoik.brightness);
        let update = f(&mut manager, &self.synoik.backlight);
        self.synoik.brightness = manager;
        let crate::brightness::BrightnessUpdate { writes, osd } = update;

        // gnome-shell's key handlers are `this._globalScale?.stepUp()` and
        // `this._monitorScales.get(monitor)?.stepUp()` (`brightnessManager.js:107-132`): with no
        // scale to move there is no `notify::value`, so no `user-update` either. An empty write
        // list is exactly that case — every entry point returns early when its scale is missing.
        let moved = !writes.is_empty();

        for write in writes {
            self.set_backlight_brightness(&write.connector, write.brightness);
        }

        self.show_brightness_osd(&osd);

        // gnome-shell's `BrightnessItem._sync` off the manager's `changed`/`notify::value`.
        let view = self.synoik.brightness.view();
        let has_control = view.global.is_some();
        if self.synoik.panel_popover.set_brightness(view) {
            self.synoik.queue_redraw_all();
        }

        if let Some(tx) = self.synoik.brightness_emit.as_ref() {
            use crate::dbus::gnome_shell_brightness::SynoikToBrightness;

            // The service dedups `HasControl`, so this can fire on every sync.
            let _ = tx.send_blocking(SynoikToBrightness::HasControl(has_control));
            if user && moved {
                let _ = tx.send_blocking(SynoikToBrightness::UserChanged);
            }
        }
    }

    /// `BrightnessManager._showOSD` (`js/misc/brightnessManager.js:264-275`): one bar per monitor
    /// that moved, drawn with `display-brightness-symbolic` and no label. There is no `max_level`,
    /// so the bar tops out at 1.0. An empty request is *not* `hideAll` — GNOME simply does not
    /// call `show`, leaving any OSD already on screen to expire on its own deadline.
    fn show_brightness_osd(&mut self, osd: &[crate::brightness::OsdRequest]) {
        if osd.is_empty() {
            return;
        }

        let levels: Vec<_> = osd
            .iter()
            .filter_map(|request| {
                let output = self
                    .synoik
                    .output_by_name_match(&request.connector)?
                    .clone();
                Some((output, crate::ui::osd::OsdLevel::new(request.level, 1.)))
            })
            .collect();
        if levels.is_empty() {
            return;
        }

        self.synoik
            .osd
            .show(&["display-brightness-symbolic"], None, &levels);
        self.synoik.queue_redraw_all();
    }

    /// A call on `org.gnome.Shell.Brightness` — gsd-power asking for idle dimming or feeding an
    /// auto-brightness target. Never `user`: these are not the user touching a slider.
    /// A call on `org.gnome.ScreenSaver` — see [`crate::dbus::gnome_screen_saver`].
    pub fn on_screen_saver_msg(
        &mut self,
        msg: crate::dbus::gnome_screen_saver::ScreenSaverToSynoik,
    ) {
        use crate::dbus::gnome_screen_saver::ScreenSaverToSynoik;

        let now = crate::utils::get_monotonic_time();
        let effects = match msg {
            ScreenSaverToSynoik::Lock(reply) => {
                // A passwordless account gets a shield that covers the screen and never locks
                // (`screenShield.js:656-659`). Unknown reads as "has a password" — see
                // `accounts_service`, where that default is the whole risk of reading this
                // asynchronously.
                let passwordless = self.synoik.user_account.password_mode.is_none();
                match self.synoik.screen_shield.lock(now, passwordless) {
                    Ok(effects) => {
                        self.synoik.lock_replies.extend(reply);
                        effects
                    }
                    Err(crate::screen_shield::LockRefused::LockedDown) => {
                        // GNOME logs and returns (`screenShield.js:638-641`).
                        debug!("screen lock is locked down, not locking");
                        // **Divergence: a refused lock still answers.** GNOME's `LockAsync` waits
                        // on a signal this path never emits, so a caller that locks down the
                        // screen and then calls `Lock` blocks until the D-Bus timeout. Dropping
                        // the reply closes its channel, which is the caller's cue to stop waiting.
                        drop(reply);
                        return;
                    }
                }
            }
            ScreenSaverToSynoik::SetActive(true) => self.synoik.screen_shield.activate(now),
            ScreenSaverToSynoik::SetActive(false) => self.synoik.screen_shield.deactivate(),
        };
        self.apply_shield_effects(effects);
    }

    /// A key arrived while the screen shield is down.
    ///
    /// Unlocked, any key raises the shield (the screensaver half). Locked, the key drives the
    /// unlock dialog: it raises the prompt page and — if printable — is kept, so typing a password
    /// blind from the clock does not eat its first letter (`unlockDialog.js:672-692`).
    pub fn on_shield_key(
        &mut self,
        raw: Option<smithay::input::keyboard::Keysym>,
        text: Option<char>,
        mods: crate::ui::text_edit::EditMods,
    ) {
        use smithay::input::keyboard::Keysym;

        if !self.synoik.screen_shield.is_active() {
            return;
        }
        let now = crate::utils::get_monotonic_time();

        if self.synoik.screen_shield.is_dismissible() {
            // A screensaver raises on anything. Bare modifiers included: GNOME's shield is not
            // fussy, and a user pressing Shift to wake the screen expects it to wake.
            //
            // `is_dismissible`, not `!is_locked`: a lock still waiting on its verifier must not be
            // typed away before the answer lands.
            let effects = self.synoik.screen_shield.deactivate();
            self.apply_shield_effects(effects);
            return;
        }

        self.synoik.lock_screen.note_activity(now);

        // **Shift and caps lock do not raise the prompt.** GNOME returns early for exactly these
        // four and for nothing else (`unlockDialog.js:677-682`) — Ctrl, Alt and Super fall through
        // to `_showPrompt()` like any other key. They are the keys you press *before* the one you
        // meant: holding Shift for a capital, or setting caps lock, should leave the clock up so
        // the letter that follows is what wakes the prompt and gets typed into it.
        //
        // Setting caps lock at the clock is also the case the warning has to survive — it is on
        // before the entry exists, and must appear with the entry rather than waiting for another
        // press.
        let is_shift_like = matches!(
            raw,
            Some(Keysym::Shift_L | Keysym::Shift_R | Keysym::Shift_Lock | Keysym::Caps_Lock)
        );
        if is_shift_like && self.synoik.unlock_dialog.page() == crate::unlock_dialog::Page::Clock {
            return;
        }

        // The entry gets first refusal on everything but the shield's own two keys, so the
        // password field has the same editing surface as every other entry in the shell.
        let theme = self.synoik.gnome_settings.key_theme;
        let was_clock = self.synoik.unlock_dialog.page() == crate::unlock_dialog::Page::Clock;
        let mut effects = match raw {
            Some(Keysym::Escape) => self.synoik.unlock_dialog.cancel(),
            Some(Keysym::Return | Keysym::KP_Enter) => self.synoik.unlock_dialog.submit(now),
            _ => {
                let entry = self
                    .synoik
                    .unlock_dialog
                    .entry_key(raw, text, mods, theme, now);
                match entry {
                    Some(effects) => effects,
                    // Not an editing key (and not text): still activity, so the clock page
                    // raises the prompt — `type_char` used to be the only path that did.
                    None => self.synoik.unlock_dialog.show_prompt(now),
                }
            }
        };

        // Raising the prompt is what arms the fingerprint reader, so that has to be read off the
        // page actually moving — not carried by whichever branch happened to run. Two of them
        // raise the prompt inside `entry_key` and then return `None`, discarding the effects that
        // said so: a key that edits nothing, and a key arriving before gdm has asked its question.
        // Both left a prompt on screen with a sensor that was never started, and nothing anywhere
        // said so — the password still worked, so it read as a hardware problem.
        if was_clock && self.synoik.unlock_dialog.page() == crate::unlock_dialog::Page::Prompt {
            effects.start_fingerprint = true;
        }

        self.apply_unlock_effects(effects);
    }

    /// A click while the shield is down: raise it, raise the prompt page
    /// (`unlockDialog.js:571-573`'s click gesture), or hit something on the prompt.
    pub fn on_shield_click(&mut self, pos: Point<f64, Logical>) {
        if !self.synoik.screen_shield.is_active() {
            return;
        }
        let now = crate::utils::get_monotonic_time();

        if !self.synoik.screen_shield.is_dismissible() {
            self.synoik.lock_screen.note_activity(now);

            // The switch-user button, which is reactive only while the prompt page is up
            // (`unlockDialog.js:811-814`) — on the clock page a click there just raises the prompt
            // like a click anywhere else.
            if self.synoik.switch_user_reactive(now) {
                let on_button = self
                    .synoik
                    .output_under(pos)
                    .and_then(|(output, _)| self.synoik.global_space.output_geometry(output))
                    .is_some_and(|geo| crate::ui::lock_screen::switch_user_hit(geo.to_f64(), pos));
                if on_button {
                    self.switch_user();
                    return;
                }
            }

            // The peek toggle, if the pointer is on it and the prompt is up.
            if self.synoik.unlock_dialog.page() == crate::unlock_dialog::Page::Prompt
                && self.synoik.unlock_dialog.peek().is_some()
            {
                let output = self.synoik.output_under(pos).map(|(o, _)| o.clone());
                let hit = output.and_then(|output| {
                    let geo = self.synoik.global_space.output_geometry(&output)?;
                    crate::ui::lock_screen::peek_hit(geo.to_f64(), pos)
                });
                if hit == Some(crate::ui::widget::EntryHit::Trailing) {
                    let effects = self.synoik.unlock_dialog.toggle_peek(now);
                    self.apply_unlock_effects(effects);
                    return;
                }
            }

            let effects = self.synoik.unlock_dialog.show_prompt(now);
            self.apply_unlock_effects(effects);
        } else {
            let effects = self.synoik.screen_shield.deactivate();
            self.apply_shield_effects(effects);
        }
    }

    /// Go to the login screen, dropping back to the clock with whatever was typed cleared.
    ///
    /// **Divergence: the gdm conversation is kept, where GNOME cancels it.** `_otherUserClicked`
    /// (`unlockDialog.js:901-905`) calls `authPrompt.cancel()`, which reaches
    /// `this._userVerifier.cancel()` through `reset()` (`authPrompt.js:839-852`, `:742`). GNOME can
    /// afford that because the prompt is an actor it *rebuilds*: `_maybeDestroyAuthPrompt` disposes
    /// of it when the crossfade lands (`:795`) and the next `_showPrompt` calls
    /// `_ensureAuthPrompt`, beginning a fresh conversation.
    ///
    /// We have no such lifecycle. `VerifierRequest::Begin` is sent from exactly one place, driven
    /// by `ScreenShield::lock`, and a screen that is already locked never locks again — so
    /// cancelling here would close the only channel we have and leave the shield locked with
    /// nothing left to authenticate against. That matters because switching users does **not** end
    /// this session: it stays running in the background and the user can VT-switch straight back to
    /// it. The trade is a conversation that outlives the page flip, which is what Escape already
    /// does and for the same stated reason ([`crate::unlock_dialog::UnlockDialog::show_clock`]) —
    /// against a lock screen nobody can get past. Cancelling becomes safe once there is a re-Begin;
    /// see `authenticator_lost`, which is the same fail-open instinct from the other direction.
    pub fn switch_user(&mut self) {
        let now = crate::utils::get_monotonic_time();
        let effects = self.synoik.unlock_dialog.cancel();
        self.apply_unlock_effects(effects);
        self.synoik.lock_screen.note_activity(now);

        let Some(conn) = self
            .synoik
            .dbus
            .as_ref()
            .and_then(|d| d.conn_accounts.as_ref())
        else {
            warn!("cannot switch users: no system bus connection");
            return;
        };
        // Detached: this is several system-bus round trips and the event loop is also the render
        // loop. Nothing waits for it — the outcome is a session switch, not a frame.
        let async_conn = conn.inner().clone();
        conn.inner()
            .executor()
            .spawn(
                async move { crate::dbus::user_switching::goto_login_session(&async_conn).await },
                "goto-login-session",
            )
            .detach();
    }

    /// Publish an [`UnlockEffects`](crate::unlock_dialog::UnlockEffects).
    /// Raise or drop the caps-lock warning.
    ///
    /// Only for a **secret** question (`authPrompt.js:414` sets the label's visibility straight
    /// from `secret`) — a username prompt gets no warning — and only on the prompt page, since that
    /// is the only place the label exists.
    pub(crate) fn sync_caps_warning(&mut self) -> bool {
        // **Read live, never from a cache.** GNOME asks the keymap every time it syncs
        // (`shellEntry.js:192`), which is why it cannot show a stale warning. xkb state is updated
        // by every `input()` regardless of what the shield is doing, so this is always current —
        // whereas a value sampled only on the shield's own key path is wrong for every other way
        // the prompt goes up. Clicking to raise it after locking with caps already on showed no
        // warning; clicking after a lock/unlock/re-lock cycle showed one that was not true.
        self.synoik.caps_lock = self
            .synoik
            .seat
            .get_keyboard()
            .map(|kbd| kbd.modifier_state().caps_lock)
            .unwrap_or(self.synoik.caps_lock);

        let warn = self.synoik.caps_lock
            && self.synoik.unlock_dialog.page() == crate::unlock_dialog::Page::Prompt
            && self.synoik.unlock_dialog.asks_for_secret();
        self.synoik
            .lock_screen
            .set_caps_warning(warn, crate::utils::get_monotonic_time())
    }

    /// Point the curtain's crossfade at whichever page the dialog is on.
    ///
    /// Two things it deliberately does not key off `is_locked()`, which both call sites used to:
    ///
    /// - **A shield on its way out keeps the page it had.** `locked` drops the instant gdm accepts,
    ///   so keying off it reset the page to the clock *during the slide-out* — while GNOME never
    ///   calls `_showClock` on success at all, and slides the group away still showing the prompt
    ///   you just authenticated with (`_continueDeactivate`, `:551-556`).
    /// - **A screensaver has no prompt to move to**, which is the real content of the old check:
    ///   `is_dismissible` is the shield that raises on any input, and it must not display a
    ///   password entry with no conversation behind it.
    fn sync_lock_page(&mut self) {
        if !self.synoik.screen_shield.is_active() {
            return;
        }
        let prompt = self.synoik.unlock_dialog.page() == crate::unlock_dialog::Page::Prompt
            && !self.synoik.screen_shield.is_dismissible();
        self.synoik
            .lock_screen
            .set_page(prompt, crate::utils::get_monotonic_time());
    }

    /// The page and the caps warning move together: the warning belongs to the prompt page and to
    /// secret questions, and both of those change underneath us when gdm speaks.
    fn sync_lock_page_and_caps(&mut self) {
        self.sync_lock_page();
        self.sync_caps_warning();
    }

    pub fn apply_unlock_effects(&mut self, effects: crate::unlock_dialog::UnlockEffects) {
        if let Some(request) = effects.request {
            if let Some(tx) = self.synoik.gdm_requests.as_ref() {
                let _ = tx.send_blocking(request);
            }
        }

        // The reader is armed by the prompt coming up, never before it (`_showPrompt` →
        // `_ensureAuthPrompt`, `unlockDialog.js:799-800`). Sent unconditionally: whether there is a
        // reader at all, and whether it has already been started on this channel, is the verifier
        // task's to know — it is the one holding the conversation.
        if effects.start_fingerprint {
            if let Some(tx) = self.synoik.gdm_requests.as_ref() {
                let _ = tx.send_blocking(crate::dbus::gdm::VerifierRequest::StartFingerprint);
            }
        }

        // The view's crossfade clock follows the model's page. Synced here rather than at each
        // `show_prompt`/`show_clock` call site because this is the one funnel every page change
        // already goes through, and a missed site would be a page that snaps instead of fading.
        self.sync_lock_page_and_caps();

        // Every path that can queue a message funnels through here, so this is the one place that
        // has to arm the wake-up. A queued message with no timer behind it is a message that never
        // appears.
        self.arm_unlock_message_timer();

        // ...and the same funnel is where a fingerprint error becomes a wiggle.
        if self.synoik.unlock_dialog.take_wiggle() {
            let now = crate::utils::get_monotonic_time();
            self.synoik.lock_screen.start_wiggle(now);
            self.synoik.queue_redraw_all();
        }

        if effects.unlock {
            // gdm accepted. This is the only call to `deactivate` that can raise a *locked*
            // shield, and it is reachable only from `VerifierEvent::Complete`.
            let effects = self.synoik.screen_shield.deactivate();
            self.apply_shield_effects(effects);
        } else if effects.redraw {
            self.synoik.queue_redraw_all();
        }
    }

    /// Wake the dialog when the shown message has had its read time and something is waiting.
    ///
    /// Replaced rather than stacked, and dropped outright when the queue is empty — the deadline
    /// moves every time a message is promoted, so a timer left armed from a previous message would
    /// fire against the wrong one.
    fn arm_unlock_message_timer(&mut self) {
        if let Some(token) = self.synoik.unlock_message_timer.take() {
            self.synoik.event_loop.remove(token);
        }
        let Some(deadline) = self.synoik.unlock_dialog.message_deadline() else {
            return;
        };
        let now = crate::utils::get_monotonic_time();
        let timer = calloop::timer::Timer::from_duration(deadline.saturating_sub(now));
        self.synoik.unlock_message_timer = self
            .synoik
            .event_loop
            .insert_source(timer, move |_, _, state| {
                state.synoik.unlock_message_timer = None;
                let now = crate::utils::get_monotonic_time();
                let effects = state.synoik.unlock_dialog.tick(now);
                // Re-arms through `apply_unlock_effects` if more is queued behind this one.
                state.apply_unlock_effects(effects);
                calloop::timer::TimeoutAction::Drop
            })
            .map_err(|err| warn!("error arming the unlock message timer: {err:?}"))
            .ok();
    }

    /// A message from the polkit agent — see [`crate::dbus::polkit_agent`].
    pub fn on_polkit_msg(&mut self, msg: crate::dbus::polkit_agent::PolkitToSynoik) {
        use crate::dbus::polkit_agent::PolkitToSynoik;

        // Not over a lock screen. GNOME holds the request and re-runs it when the session mode
        // changes (`polkitAgent.js:439-450`); the one action it exempts is extending a
        // parental-controls session limit, which we have no subsystem for.
        if let PolkitToSynoik::Begin(request) = &msg {
            if self.synoik.screen_is_covered() {
                debug!(
                    "polkit: holding {} until the screen unlocks",
                    request.action_id
                );
                self.synoik.polkit_deferred = Some(request.clone());
                return;
            }
        }
        // A withdrawn request that never got on screen still has to stop being held.
        if matches!(msg, PolkitToSynoik::Cancel) {
            self.synoik.polkit_deferred = None;
        }

        let effects = self.synoik.polkit_dialog.on_agent_event(msg);
        self.apply_polkit_effects(effects);
    }

    /// The screen has unlocked: run whatever polkit asked for while it was down.
    pub fn resume_deferred_polkit(&mut self) {
        let Some(request) = self.synoik.polkit_deferred.take() else {
            return;
        };
        self.on_polkit_msg(crate::dbus::polkit_agent::PolkitToSynoik::Begin(request));
    }

    /// The one funnel for everything the dialog decides.
    pub fn apply_polkit_effects(&mut self, effects: crate::polkit_dialog::PolkitEffects) {
        if let Some(request) = effects.request {
            if let Some(tx) = self.synoik.polkit_requests.as_ref() {
                let _ = tx.send_blocking(request);
            } else {
                warn!("polkit: no agent to send to; the dialog is talking to nothing");
            }
        }

        // Open and close ride the state machine rather than a separate flag, so there is one
        // answer to "is it up" and the animation cannot disagree with it.
        if self.synoik.polkit_dialog.is_open() {
            self.synoik.polkit_ui.show();
        } else {
            self.synoik.polkit_ui.hide();
        }

        if effects.wiggle {
            self.synoik
                .polkit_ui
                .start_wiggle(crate::utils::get_monotonic_time());
        }

        if effects.arm_reset {
            self.arm_polkit_reset_timer();
        }

        if effects.close {
            if let Some(token) = self.synoik.polkit_reset_timer.take() {
                self.synoik.event_loop.remove(token);
            }
        }

        if effects.redraw || effects.close || effects.wiggle {
            // The focus chain has a `PolkitDialog` arm, so opening or closing changes who gets
            // keys; recomputing it here is what makes the modal grab actually modal.
            self.synoik.queue_redraw_all();
            self.refresh_and_flush_clients();
        }
    }

    /// Hide the entry [`DELAYED_RESET`] after a conversation ends, unless another has asked by
    /// then.
    ///
    /// [`DELAYED_RESET`]: crate::polkit_dialog::DELAYED_RESET
    fn arm_polkit_reset_timer(&mut self) {
        if let Some(token) = self.synoik.polkit_reset_timer.take() {
            self.synoik.event_loop.remove(token);
        }
        let timer = calloop::timer::Timer::from_duration(crate::polkit_dialog::DELAYED_RESET);
        self.synoik.polkit_reset_timer = self
            .synoik
            .event_loop
            .insert_source(timer, move |_, _, state| {
                state.synoik.polkit_reset_timer = None;
                let effects = state.synoik.polkit_dialog.on_reset_timeout();
                state.apply_polkit_effects(effects);
                calloop::timer::TimeoutAction::Drop
            })
            .map_err(|err| warn!("error arming the polkit reset timer: {err:?}"))
            .ok();
    }

    /// A message from gdm's verifier — see [`crate::dbus::gdm`].
    pub fn on_verifier_event(&mut self, event: crate::dbus::gdm::VerifierEvent) {
        use crate::dbus::gdm::VerifierEvent;

        // The shield's gate: a live channel is what makes locking safe, and its absence is what
        // keeps a screensaver a screensaver.
        match &event {
            VerifierEvent::Ready(epoch) => {
                let effects = self.synoik.screen_shield.authenticator_ready(*epoch, true);
                self.apply_shield_effects(effects);
            }
            VerifierEvent::Unavailable(epoch, reason) => {
                warn!("the screen will not lock: {reason}");
                let effects = self.synoik.screen_shield.authenticator_ready(*epoch, false);
                self.apply_shield_effects(effects);
            }
            VerifierEvent::Lost => {
                let effects = self.synoik.screen_shield.authenticator_lost();
                self.apply_shield_effects(effects);
            }
            _ => (),
        }

        let now = crate::utils::get_monotonic_time();
        let effects = self.synoik.unlock_dialog.on_verifier_event(event, now);
        self.apply_unlock_effects(effects);
    }

    /// fprintd answered the startup probe.
    ///
    /// Only matters for the *next* lock: the conversation reads the reader type when it begins, so
    /// a probe landing after a lock is already up leaves that one without a reader. That matches
    /// GNOME closely enough — it re-runs `_maybeStartFingerprintVerification` on a late detection
    /// (`util.js:437-442`) — and the window is one round trip at startup against a lock screen that
    /// is not up yet.
    /// gsd's smartcard tokens changed.
    pub fn on_smartcard_msg(&mut self, msg: crate::dbus::smartcard::SmartcardToSynoik) {
        let crate::dbus::smartcard::SmartcardToSynoik::Detected(detected) = msg;
        self.synoik.smartcard_detected = detected;
    }

    pub fn on_fingerprint_reader(&mut self, reader: crate::dbus::fprintd::ReaderType) {
        if self.synoik.fingerprint_reader == reader {
            return;
        }
        if reader.is_present() {
            debug!("fingerprint reader detected: {reader:?}");
        }
        self.synoik.fingerprint_reader = reader;
    }

    /// AccountsService answered, or the account changed under us.
    ///
    /// Both are the same event: re-read and re-render. `PasswordMode` in particular is not
    /// read-once — a user setting or clearing their password mid-session changes what every
    /// *later* lock should do, and a cached value would keep locking (or not) the old way.
    pub fn on_accounts_msg(&mut self, msg: crate::dbus::accounts_service::AccountsToSynoik) {
        use crate::dbus::accounts_service::AccountsToSynoik;

        match msg {
            AccountsToSynoik::UserChanged(account) => {
                if self.synoik.user_account == account {
                    return;
                }
                // The real name is the only part the dialog itself holds; it falls back to the
                // login name when AccountsService has nothing, which is what we did from GECOS.
                self.synoik
                    .unlock_dialog
                    .set_real_name(account.real_name.clone());
                // Everything downstream is keyed by *path*, and AccountsService reuses one path
                // per user — so a picture the user just changed has to be evicted explicitly, or
                // the old bytes are served for the rest of the session. `icon_stamp` is what let
                // the equality check above notice at all.
                let stale = self.synoik.avatar_source();
                self.synoik.user_account = account;
                if let Some(stale) = stale {
                    self.synoik.image_cache.retain(|source| source != &stale);
                    self.synoik.lock_screen.forget_avatar();
                }
                // Decode the picture now, not on the frame that first draws it. A cold key draws
                // *nothing* and the prompt falls back to the themed glyph, so a lazy decode means
                // the first lock after login shows the default avatar and then swaps to the
                // photograph — the cold-key flicker again ([[cold-cost-class]]).
                self.synoik.warm_avatar();
            }
            AccountsToSynoik::MultipleUsers(multiple) => {
                if self.synoik.multiple_users == multiple {
                    return;
                }
                self.synoik.multiple_users = multiple;
            }
            AccountsToSynoik::CanSwitch(can) => {
                if self.synoik.can_switch_user == can {
                    return;
                }
                self.synoik.can_switch_user = can;
            }
        }
        if self.synoik.screen_shield.is_active() {
            self.synoik.queue_redraw_all();
        }
    }

    /// Publish a [`ShieldEffects`](crate::screen_shield::ShieldEffects): the shared snapshot the
    /// bus reads, the signals it emits, logind's locked hint, and the clipboard wipe.
    pub fn apply_shield_effects(&mut self, effects: crate::screen_shield::ShieldEffects) {
        // The curtain follows `active`, not `locked`: a shield down without a lock is still a
        // shield, and it is what a `lock-enabled = false` screensaver is.
        if effects.active_changed.is_some() {
            let now = self.synoik.screen_shield.is_active();
            self.synoik
                .lock_screen
                .set_shown(now, crate::utils::get_monotonic_time());
            if effects.curtain_instant {
                // The idle path's shield lands on an already-black screen; sliding onto it would
                // animate something nobody can see.
                self.synoik.lock_screen.settle();
            }
            self.synoik.queue_redraw_all();
        }

        // Asking for a verifier makes the shield undismissible until the answer lands
        // (`is_dismissible`), so *something* must always answer. Two ways it would not:
        //
        // - there is nobody to ask (no D-Bus, or the gdm client failed to start). Answered here, at
        //   once — "no channel" is precisely what the gate exists to catch.
        // - gdm takes the request and never replies. A dead socket arrives as `Lost`, but a live
        //   gdm that simply goes quiet has no event at all, so it needs a deadline.
        //
        // Without both, a shield that cannot lock also cannot be raised: a lockout, and a worse
        // one than the lock it was standing in for.
        if let Some(epoch) = effects.request_authenticator {
            let mut asked = false;
            if let Some(tx) = self.synoik.gdm_requests.as_ref() {
                let username = self.synoik.unlock_dialog.user().name.clone();
                asked = tx
                    .send_blocking(crate::dbus::gdm::VerifierRequest::Begin {
                        username,
                        epoch,
                        reader: self.synoik.fingerprint_reader,
                    })
                    .is_ok();
            }

            if asked {
                self.arm_authenticator_watchdog(epoch);
            } else {
                warn!("no way to reach gdm; the shield stays a screensaver");
                let effects = self.synoik.screen_shield.authenticator_ready(epoch, false);
                self.apply_shield_effects(effects);
            }
        }

        // A raised shield ends the conversation: leaving a PAM worker and an open channel behind
        // for a screen nobody is looking at is both a leak and a stale verifier that could answer
        // a later lock.
        if effects.cancel_authenticator {
            if let Some(tx) = self.synoik.gdm_requests.as_ref() {
                let _ = tx.send_blocking(crate::dbus::gdm::VerifierRequest::Cancel);
            }
        }
        if effects.cancel_authenticator {
            self.synoik.unlock_dialog.show_clock();
        }

        if effects.clear_clipboard {
            // Both selections, as `lock` does (`screenShield.js:645-651`): the unlock entry can be
            // unmasked, so a password sitting in the clipboard would be readable by whoever walks
            // up. Cheap, and its absence is invisible until it matters.
            let dh = self.synoik.display_handle.clone();
            clear_data_device_selection(&dh, &self.synoik.seat);
            clear_primary_selection(&dh, &self.synoik.seat);
            self.synoik.clipboard_mime_types.clear();
        }

        {
            // `_activationTime` is not gated: it is stamped when the session went idle or the
            // lock was asked for, and `GetActiveTime` is what gsd reads to decide how long the
            // seat has been unattended.
            self.synoik.shield_snapshot.lock().unwrap().activation_time =
                self.synoik.screen_shield.activation_time();

            use crate::dbus::gnome_screen_saver::SynoikToScreenSaver;
            if let Some(tx) = self.synoik.screen_saver_emit.as_ref() {
                if effects.wake_up_screen {
                    let _ = tx.send_blocking(SynoikToScreenSaver::WakeUpScreen);
                }
            }

            // logind's `LockedHint` is what `loginctl` and the session tooling read.
            if effects.locked_changed.is_some() {
                self.synoik.update_locked_hint();
            }
        }

        if effects.cancel_dialog {
            self.synoik.unlock_dialog.cancel();
        }

        if effects.stop_fade {
            self.synoik.lock_screen.light_off();
            if let Some(token) = self.synoik.fade_timer.take() {
                self.synoik.event_loop.remove(token);
            }
        }

        if effects.start_fade {
            let now = crate::utils::get_monotonic_time();
            self.synoik.lock_screen.light_on(now);
            self.arm_fade_timer();
            self.synoik.queue_redraw_all();
        }

        if effects.cancel_lock_timer {
            if let Some(token) = self.synoik.lock_timer.take() {
                self.synoik.event_loop.remove(token);
            }
        }

        if let Some(delay) = effects.arm_lock_timer {
            self.arm_lock_timer(delay);
        }

        // The shield's own paths move the page too (`cancel_dialog`, and the `show_clock` a
        // cancelled conversation forces), so the crossfade clock is synced from here as well.
        self.sync_lock_page_and_caps();

        // A `Lock` caller may be waiting on the state we just moved to — including the case where
        // the shield is not going to land at all.
        self.synoik.settle_lock_replies();
        // Falls publish here rather than waiting for a frame; rises wait for the curtain.
        self.synoik.publish_shield_active();

        // Last, and unconditionally: the inhibitor is a function of the state we just moved to, and
        // the suspend path *depends* on it being dropped here — logind is waiting on that fd.
        self.synoik.sync_sleep_inhibitor();
    }

    /// Bound how long a lock may wait on gdm before giving up and staying a screensaver.
    ///
    /// Long enough that a slow PAM stack is not mistaken for a dead one, short enough that a user
    /// facing a shield that will never lock is not stuck looking at it. Nothing is lost by being
    /// wrong in the impatient direction: giving up produces a screensaver, which is what a shield
    /// with no verifier was always entitled to be.
    const AUTHENTICATOR_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

    /// Answer the gate ourselves if gdm has not, within [`Self::AUTHENTICATOR_TIMEOUT`].
    ///
    /// Epoch-tagged like every other answer, so a watchdog for an abandoned lock cannot refuse a
    /// later one; `authenticator_ready` drops it on the floor.
    fn arm_authenticator_watchdog(&mut self, epoch: u64) {
        let timer = calloop::timer::Timer::from_duration(Self::AUTHENTICATOR_TIMEOUT);
        let res = self
            .synoik
            .event_loop
            .insert_source(timer, move |_, _, state| {
                let effects = state.synoik.screen_shield.authenticator_ready(epoch, false);
                if effects != crate::screen_shield::ShieldEffects::default() {
                    warn!("gdm never answered; the shield stays a screensaver");
                }
                state.apply_shield_effects(effects);
                calloop::timer::TimeoutAction::Drop
            });
        if let Err(err) = res {
            warn!("error arming the verifier watchdog: {err:?}");
        }
    }

    /// Arm the fade's completion — the moment the screen is black and the shield goes down.
    ///
    /// A timer rather than "when the render says alpha is 1", because the shield must go down even
    /// on an output that is not drawing (a blanked or unplugged monitor still has to end up
    /// covered).
    fn arm_fade_timer(&mut self) {
        if let Some(token) = self.synoik.fade_timer.take() {
            self.synoik.event_loop.remove(token);
        }
        let timer = calloop::timer::Timer::from_duration(crate::ui::lock_screen::FADE_TIME);
        let token = self
            .synoik
            .event_loop
            .insert_source(timer, move |_, _, state| {
                state.synoik.fade_timer = None;
                let now = crate::utils::get_monotonic_time();
                let effects = state.synoik.screen_shield.fade_complete(now);
                state.apply_shield_effects(effects);
                calloop::timer::TimeoutAction::Drop
            })
            .map_err(|err| warn!("error arming the idle fade timer: {err:?}"))
            .ok();
        self.synoik.fade_timer = token;
    }

    /// Arm the idle grace period. Any timer already pending is replaced, never stacked.
    fn arm_lock_timer(&mut self, delay: std::time::Duration) {
        if let Some(token) = self.synoik.lock_timer.take() {
            self.synoik.event_loop.remove(token);
        }

        let timer = calloop::timer::Timer::from_duration(delay);
        let token = self
            .synoik
            .event_loop
            .insert_source(timer, move |_, _, state| {
                state.synoik.lock_timer = None;

                // The timer outlives anything that locked in the meantime — a suspend during the
                // grace period, or `loginctl lock-session` — because only `deactivate` cancels it
                // (`_completeDeactivate`, `screenShield.js:575-578`). GNOME's `lock()` is benign
                // when re-run; ours would bump the epoch and restart the gdm conversation, pulling
                // the prompt out from under someone already typing their password.
                // (`is_dismissible` already implies not locked and not mid-handshake, so it is the
                // whole condition — and it is the predicate the model's tests pin.)
                if !state.synoik.screen_shield.is_dismissible() {
                    return calloop::timer::TimeoutAction::Drop;
                }

                let now = crate::utils::get_monotonic_time();
                match state.synoik.screen_shield.lock(now, false) {
                    Ok(effects) => state.apply_shield_effects(effects),
                    Err(crate::screen_shield::LockRefused::LockedDown) => {
                        debug!("screen lock is locked down; the idle shield stays a screensaver");
                    }
                }
                calloop::timer::TimeoutAction::Drop
            })
            .map_err(|err| warn!("error arming the idle lock timer: {err:?}"))
            .ok();
        self.synoik.lock_timer = token;
    }

    /// gnome-session's presence changed (`_onStatusChanged`, `screenShield.js:242-272`).
    pub fn on_presence_msg(&mut self, msg: crate::dbus::gnome_session_presence::PresenceToSynoik) {
        use crate::dbus::gnome_session_presence::{PresenceStatus, PresenceToSynoik};

        let PresenceToSynoik::StatusChanged(status) = msg;
        let now = crate::utils::get_monotonic_time();

        let effects = match status {
            PresenceStatus::Idle => self.synoik.screen_shield.on_session_idle(now),
            // Only `Available` counts as the user coming back. GNOME hangs this on the core idle
            // monitor's user-active watch (`:282`), i.e. on real input; presence is the closest
            // thing we subscribe to. `Busy` and `Invisible` are *not* activity — an app taking an
            // inhibitor while the seat is idle flips the status without anyone touching the
            // machine, and treating that as a return would un-blank the screen and cancel the
            // pending lock on an unattended desk.
            PresenceStatus::Available => self.synoik.screen_shield.on_user_active(),
            PresenceStatus::Busy | PresenceStatus::Invisible | PresenceStatus::Unknown(_) => return,
        };
        self.apply_shield_effects(effects);
    }

    pub fn on_brightness_msg(
        &mut self,
        msg: crate::dbus::gnome_shell_brightness::BrightnessToSynoik,
    ) {
        use crate::dbus::gnome_shell_brightness::BrightnessToSynoik;

        match msg {
            BrightnessToSynoik::SetDimming(enable) => {
                trace!("brightness: dimming {enable}");
                self.sync_brightness(|manager, snapshot| manager.set_dimming(enable, snapshot));
            }
            BrightnessToSynoik::SetAutoBrightnessTarget(target) => {
                trace!("brightness: auto-brightness target {target}");
                self.sync_brightness(|manager, snapshot| {
                    manager.set_auto_brightness_target(target, snapshot)
                });
            }
        }
    }

    /// The quick-settings brightness slider: the global scale, which fans out to every monitor
    /// through its factor (`brightnessManager.js:229-240`).
    pub fn set_global_brightness(&mut self, value: f64) {
        self.sync_brightness_user(|manager, snapshot| manager.set_global_value(value, snapshot));
    }

    /// A brightness keybinding (`org.gnome.shell.keybindings screen-brightness-*`).
    ///
    /// `current_monitor` is GNOME's `-monitor` variant, which steps only
    /// `get_current_logical_monitor()` — the monitor under the pointer
    /// (`brightnessManager.js:107-132`). With no backlight on that monitor it is a no-op, as in
    /// gnome-shell, where the lookup simply misses (`this._monitorScales.get(monitor)?.stepUp()`).
    pub fn step_brightness(&mut self, step: crate::brightness::Step, current_monitor: bool) {
        if !current_monitor {
            self.sync_brightness_user(|manager, snapshot| manager.step_global(step, snapshot));
            return;
        }

        let Some(output) = self.synoik.output_under_cursor() else {
            return;
        };
        let connector = output.name();
        self.sync_brightness_user(|manager, snapshot| {
            manager.step_monitor(&connector, step, snapshot)
        });
    }

    /// One row of the per-monitor brightness card (`brightness.js:12-35`).
    pub fn set_monitor_brightness(&mut self, connector: &str, value: f64) {
        self.sync_brightness_user(|manager, snapshot| {
            manager.set_monitor_value(connector, value, snapshot)
        });
    }

    /// Re-match backlight devices against the connected outputs. Runs off the same funnel as the
    /// IPC output refresh, so it catches hotplug, mode-set (a connector's `enabled` attribute is
    /// part of the device match) and output-config changes alike.
    pub fn refresh_backlights(&mut self) {
        let outputs: Vec<(String, String)> = self
            .backend
            .ipc_outputs()
            .lock()
            .unwrap()
            .values()
            .map(|output| {
                let display_name = make_display_name(output, is_laptop_panel(&output.name));
                (output.name.clone(), display_name)
            })
            .collect();

        let Some(tty) = self.backend.tty_checked() else {
            return;
        };
        if !tty.backlights.set_outputs(outputs) {
            return;
        }

        let snapshot = tty.backlights.snapshot();
        self.synoik.backlight = snapshot;
        debug!("backlights: {:?}", self.synoik.backlight);

        // GNOME's `_monitorsChanged`: the per-monitor scales are rebuilt, the global scale is
        // created once and keeps its value (`brightnessManager.js:134-181`).
        self.sync_brightness(|manager, snapshot| manager.monitors_changed(snapshot));
    }

    /// Ask the hardware for a new brightness on one output. Goes through the write serializer, so
    /// a slider drag stays at one in-flight logind call.
    pub fn set_backlight_brightness(&mut self, connector: &str, brightness: i32) {
        let Some(tty) = self.backend.tty_checked() else {
            // No backend owns any backlight, so the snapshot is the only "hardware" there is.
            // Keeping it in step matters because the snapshot is what the next `_sync` reads back:
            // leaving it stale would make the manager mistake our own write for someone else
            // moving the panel and re-adopt the old value. On the TTY path below the same
            // invariant holds -- `snapshot()` reports the writer's *target*, not a fresh sysfs
            // read -- so this is the same rule, not a stand-in for hardware we don't have.
            if let Some(output) = self
                .synoik
                .backlight
                .outputs
                .iter_mut()
                .find(|o| o.connector == connector)
            {
                output.brightness = output.range.clamp(brightness);
            }
            return;
        };
        // The snapshot is refreshed whether or not a write goes out. `request` returns `None`
        // when the serializer holds the write back behind one already in flight — but the
        // writer's *target* has still moved, and the snapshot reports targets. Refreshing only on
        // `Some` left it stale for the rest of a key repeat or drag, and the next `_sync` would
        // then read its own old write back as an external change: the scale would snap up and
        // idle dimming would be cancelled. Mutter has no such window — it notifies
        // `brightness-target` immediately even while pending (`meta-backlight.c:159-196`).
        let write = tty.backlights.request(connector, brightness);
        let snapshot = tty.backlights.snapshot();
        self.synoik.backlight = snapshot;
        if let Some(write) = write {
            self.send_backlight_write(write);
        }
    }

    fn send_backlight_write(&mut self, write: crate::backend::backlight::PendingWrite) {
        let Some(dbus) = self.synoik.dbus.as_ref() else {
            return;
        };
        let Some(conn) = dbus.conn_login1.as_ref() else {
            return;
        };
        let Some(tx) = self.synoik.login1_tx.as_ref() else {
            return;
        };
        crate::dbus::freedesktop_login1::set_brightness(
            conn,
            tx.clone(),
            write.connector,
            write.device_name,
            write.brightness,
        );
    }

    pub fn on_locale1_msg(&mut self, msg: Locale1ToSynoik) {
        let Locale1ToSynoik::XkbChanged(xkb) = msg;

        trace!("locale1 xkb settings changed: {xkb:?}");
        let xkb = self.synoik.xkb_from_locale1.insert(xkb);

        // GNOME's input-sources take priority over systemd-localed when present;
        // keep the stored locale1 value (a capability for the no-GNOME fallback)
        // but don't apply it here.
        if self.synoik.gnome_settings.input_sources.present {
            trace!("ignoring locale1 xkb change because GNOME input-sources are present");
            return;
        }

        {
            let config = self.synoik.config.borrow();
            if config.input.keyboard.xkb != Xkb::default() {
                trace!("ignoring locale1 xkb change because synoik config has xkb settings");
                return;
            }
        }

        let xkb = xkb.clone();
        self.set_xkb_config(xkb.to_xkb_config());
        self.ipc_keyboard_layouts_changed();
    }

    pub fn on_system_status_msg(&mut self, msg: SystemStatusToSynoik) {
        match msg {
            SystemStatusToSynoik::Battery(battery) => self.synoik.system_status.battery = battery,
            SystemStatusToSynoik::Network(network) => self.synoik.system_status.network = network,
            SystemStatusToSynoik::PowerProfiles(power) => {
                // gnome-shell's `_sync`: whenever the (echoed) active profile is a known,
                // non-Balanced one, that becomes the last-selected the body-toggle returns to —
                // recorded for external changes too, not just our own clicks (gnome-shell's
                // `_sync`, which records *any* non-Balanced profile, vendor/custom
                // ones included, so a machine with a firmware profile toggles back
                // to it). Kept authoritative on `Synoik` (the gsettings model is
                // rebuilt from defaults on every unrelated change), with
                // best-effort write-through to persist it. External edits to the gsettings key
                // mid-session aren't re-seeded here (the next non-Balanced echo overwrites it) — an
                // accepted minor divergence, since re-seeding would clobber this copy from the
                // default on schema-less systems.
                if power.is_active()
                    && !power.active.is_empty()
                    && self.synoik.last_power_profile != power.active
                {
                    self.synoik.last_power_profile = power.active.clone();
                    if let Some(writer) = &self.synoik.gnome_settings_writer {
                        writer.set_last_power_profile(power.active.clone());
                    }
                }
                self.synoik.system_status.power = power;
            }
            SystemStatusToSynoik::Bluetooth(bluetooth) => {
                self.synoik.system_status.bluetooth = bluetooth;
            }
            SystemStatusToSynoik::BluetoothConnectDone(path) => {
                // Only clears an open menu's busy mark; no model change.
                if self.synoik.panel_popover.bluetooth_connect_done(&path) {
                    self.synoik.queue_redraw_all();
                }
                return;
            }
        }
        trace!("system status changed: {:?}", self.synoik.system_status);
        let mut redraw = self
            .synoik
            .panel
            .set_system_status(self.panel_system_status());
        // Keep an open quick-settings Power Mode tile in sync with live changes.
        redraw |= self
            .synoik
            .panel_popover
            .set_power_profile(self.synoik.system_status.power.clone());
        redraw |= self
            .synoik
            .panel_popover
            .set_bluetooth(self.synoik.system_status.bluetooth.clone());
        if redraw {
            self.synoik.queue_redraw_all();
        }
    }

    /// The system status as the panel should see it: the live snapshot, with a
    /// `debug-set-battery` override standing in for UPower's battery if one is set.
    pub fn panel_system_status(&self) -> SystemStatus {
        let mut status = self.synoik.system_status.clone();
        if let Some(battery) = &self.synoik.battery_override {
            status.battery = Some(battery.clone());
        }
        status
    }

    /// Adopt a fresh rfkill snapshot from the gsd-rfkill watcher: the panel airplane icon, an open
    /// QS "Airplane Mode" toggle tile (which appears/vanishes with `show`), and the Bluetooth
    /// tile's availability gate + kill-switch state.
    pub fn on_rfkill_status(&mut self, status: crate::dbus::rfkill::RfkillStatus) {
        self.synoik.system_status.airplane = status.airplane;
        self.synoik.system_status.bluetooth_rfkill = status.bluetooth;
        let mut redraw = self
            .synoik
            .panel
            .set_system_status(self.panel_system_status());
        redraw |= self.synoik.panel_popover.set_airplane(status.airplane);
        redraw |= self
            .synoik
            .panel_popover
            .set_bluetooth_rfkill(status.bluetooth);
        if redraw {
            self.synoik.queue_redraw_all();
        }
    }

    /// Adopt a fresh default-sink snapshot from the PipeWire watcher (`None` when
    /// no sink is bound). Updates the panel's output indicator + QS slider.
    pub fn on_audio_status(&mut self, status: Option<crate::audio::AudioStatus>) {
        self.synoik.audio = status;
        let mut redraw = self.synoik.panel.set_audio(status);
        // Keep an open quick-settings volume slider in sync with live changes.
        redraw |= self.synoik.panel_popover.set_audio(status);
        if redraw {
            self.synoik.queue_redraw_all();
        }
    }

    /// The PipeWire watcher reports a change in microphone activity (recording / mute); update the
    /// panel privacy indicator.
    pub fn on_mic_status(&mut self, mic: crate::audio::MicStatus) {
        self.synoik.mic = mic;
        let mut redraw = self.synoik.panel.set_mic(mic);
        // Keep an open quick-settings mic slider (level/mute/visibility) in sync with live changes.
        redraw |= self.synoik.panel_popover.set_mic(mic);
        if redraw {
            self.synoik.queue_redraw_all();
        }
    }

    /// Adopt a fresh output-sink list (+ current default) from the PipeWire watcher, for the
    /// quick-settings output-device picker.
    pub fn on_sink_list(&mut self, list: crate::audio::SinkList) {
        self.synoik.sink_list = list.clone();
        if self.synoik.panel_popover.set_sink_list(list) {
            self.synoik.queue_redraw_all();
        }
        // The bound sink's form factor and card membership ride along, so a new list can change the
        // answer — and a default-sink swap comes through here.
        self.refresh_headphones();
    }

    /// Adopt a fresh input-source list (+ current default) from the PipeWire watcher, for the
    /// quick-settings input-device picker.
    pub fn on_source_list(&mut self, list: crate::audio::SourceList) {
        self.synoik.source_list = list.clone();
        if self.synoik.panel_popover.set_source_list(list) {
            self.synoik.queue_redraw_all();
        }
    }

    /// Adopt a fresh card/route model from the PipeWire watcher — the port-level view GNOME builds
    /// its device list and its headphone detection from. Read-only for now: nothing renders it yet,
    /// so there is no redraw to queue.
    pub fn on_audio_cards(&mut self, cards: crate::audio::AudioCards) {
        self.synoik.audio_cards = cards.clone();
        if self.synoik.panel_popover.set_audio_cards(cards) {
            self.synoik.queue_redraw_all();
        }
        self.refresh_headphones();
    }

    /// GNOME's `OutputStreamSlider._portChanged` (`js/ui/status/volume.js:347-358`): when the
    /// default sink's headphone-ness changes, swap the quick-settings slider's icon and show the
    /// volume OSD — *except* on the very first answer.
    ///
    /// Three details that are easy to get wrong, all load-bearing:
    ///
    /// 1. The suppression is `initializing = this._hasHeadphones === undefined` — once per shell
    ///    lifetime, not once per stream. Hence `Option<bool>`: `None` is "no answer yet", and only
    ///    the transition out of it is silent.
    /// 2. `_hasHeadphones` is **not** reset when the default sink changes (`_connectStream` just
    ///    calls `_portChanged` again), so switching from a headphone sink to a speaker sink is a
    ///    change and *does* show the OSD. Keeping the last answer across sink swaps is deliberate.
    /// 3. The OSD's icon is the plain level glyph, never the headphone one — `showOSD` builds it
    ///    from `getIcon()` (`volume.js:283-288`). Only the slider's own button takes the override.
    fn refresh_headphones(&mut self) {
        let Some(headphones) = crate::audio::default_sink_has_headphones(
            &self.synoik.sink_list,
            &self.synoik.audio_cards,
        ) else {
            // No sink bound: no answer, and — importantly — the initial suppression is left
            // unspent.
            return;
        };
        if self.synoik.headphones == Some(headphones) {
            return;
        }
        let initializing = self.synoik.headphones.is_none();
        self.synoik.headphones = Some(headphones);
        if self.synoik.panel_popover.set_headphones(headphones) {
            self.synoik.queue_redraw_all();
        }
        if !initializing {
            if let Some(status) = self.synoik.audio {
                self.show_volume_osd(&status);
            }
        }
    }

    pub fn on_gnome_shell_msg(&mut self, msg: crate::dbus::gnome_shell::GnomeShellToSynoik) {
        use crate::dbus::gnome_shell::GnomeShellToSynoik;
        match msg {
            GnomeShellToSynoik::Grab {
                accelerator,
                mode_flags,
                grab_flags,
                sender,
                reply,
            } => {
                let action = self.grab_accelerator(&accelerator, mode_flags, grab_flags, sender);
                let _ = reply.send_blocking(action);
            }
            GnomeShellToSynoik::Ungrab {
                action,
                sender,
                reply,
            } => {
                let _ = reply.send_blocking(self.ungrab_accelerator(action, &sender));
            }
            GnomeShellToSynoik::SenderVanished(name) => {
                self.synoik.accel_grabs.retain(|g| g.owner != name);
            }
            GnomeShellToSynoik::ShowOsd {
                connector,
                label,
                level,
                max_level,
                icon,
            } => {
                self.show_osd(connector, label, level, max_level, icon);
            }
        }
    }

    /// `ShowOSD` (`js/ui/shellDBus.js:143-152`): route to one connector's monitor,
    /// or to all of them when none is given. This is how volume, mute, mic-mute,
    /// keyboard-backlight and rotation-lock OSDs arrive — gsd-media-keys handles
    /// those keys and supplies the icon, level and stepping.
    pub fn show_osd(
        &mut self,
        connector: Option<String>,
        label: Option<String>,
        level: Option<f64>,
        max_level: Option<f64>,
        icon: Option<String>,
    ) {
        let owned = icon.as_deref().map(crate::ui::osd::icon_candidates);
        let candidates: Vec<&str> = owned.iter().flatten().map(String::as_str).collect();
        // An absent `level` means "no bar", not "a bar at zero": `setLevel(undefined)`
        // hides it (`js/ui/osdWindow.js:71-72`). An absent `max_level` is 1 (`:86-88`).
        let lv = match level {
            Some(level) => crate::ui::osd::OsdLevel::new(level, max_level.unwrap_or(1.)),
            None => crate::ui::osd::OsdLevel::none(),
        };

        match connector {
            Some(connector) => {
                let Some(output) = self.synoik.output_by_name_match(&connector).cloned() else {
                    // GNOME would index its array with -1 and throw; we just skip.
                    warn!("ShowOSD for unknown connector {connector:?}");
                    return;
                };
                self.synoik
                    .osd
                    .show_one(&output, &candidates, label.as_deref(), lv);
            }
            None => self.synoik.osd.show_all(&candidates, label.as_deref(), lv),
        }
        self.synoik.queue_redraw_all();
    }

    pub fn on_idle_monitor_msg(
        &mut self,
        msg: crate::dbus::mutter_idle_monitor::IdleMonitorToSynoik,
    ) {
        use crate::dbus::mutter_idle_monitor::IdleMonitorToSynoik;

        let now = self.synoik.clock.now_unadjusted();
        match msg {
            IdleMonitorToSynoik::GetIdletime { reply } => {
                let _ = reply.send_blocking(self.synoik.idle_monitor.idletime_ms(now));
            }
            IdleMonitorToSynoik::AddIdleWatch {
                interval,
                owner,
                reply,
            } => {
                let id = self.synoik.idle_monitor.add_idle_watch(interval, owner);
                let _ = reply.send_blocking(id);
                // Arms the timer; a watch already past its interval fires on the next iteration.
                self.synoik.reschedule_idle_monitor_timer();
            }
            IdleMonitorToSynoik::AddUserActiveWatch { owner, reply } => {
                let id = self.synoik.idle_monitor.add_user_active_watch(owner);
                let _ = reply.send_blocking(id);
                // No timer: an active watch fires from `notify_activity`, not from elapsed time.
            }
            IdleMonitorToSynoik::RemoveWatch { id } => {
                self.synoik.idle_monitor.remove_watch(id);
                self.synoik.reschedule_idle_monitor_timer();
            }
            IdleMonitorToSynoik::ResetIdletime => {
                self.synoik.reset_idletime(now);
            }
            IdleMonitorToSynoik::SenderVanished(name) => {
                self.synoik.idle_monitor.remove_watches_for_owner(&name);
                self.synoik.reschedule_idle_monitor_timer();
            }
        }
    }

    /// `org.freedesktop.Notifications` requests land here (see
    /// `dbus::freedesktop_notifications`): mutate the store, reply, and apply the
    /// returned effects (signal emissions + banner-surface changes).
    pub fn on_notifications_msg(&mut self, msg: crate::notifications::NotificationsToSynoik) {
        use crate::notifications::NotificationsToSynoik;
        match msg {
            NotificationsToSynoik::Notify { mut req, reply } => {
                let now = self.synoik.clock.now_unadjusted();
                // A notification's source presents as its *app* when one resolves — the app's
                // name and icon beat the `app_name`/`app_icon` call parameters
                // (`js/ui/notificationDaemon.js:396-399`). This is what puts the browser's own
                // logo on a web notification, which arrives with an empty `app_icon` and only a
                // `desktop-entry` hint. Resolved here rather than in the D-Bus server because it
                // needs the app catalog, and the server is a plain-data seam.
                if let Some(app) = self
                    .synoik
                    .app_system
                    .app_for_notification(req.desktop_entry.as_deref(), &req.app_name)
                {
                    req.app_name = app.name;
                    req.app_icon = Some(app.icon);
                }
                // DND is the inverse of `show-banners` (the Q11 QS toggle).
                let show_banners = !self.synoik.gnome_settings.quick_toggles.do_not_disturb;
                match self.synoik.notifications.notify(req, show_banners, now) {
                    Ok((id, effects)) => {
                        let _ = reply.send_blocking(Ok(id));
                        self.apply_notification_effects(effects);
                    }
                    Err(err) => {
                        let _ = reply.send_blocking(Err(err));
                    }
                }
            }
            NotificationsToSynoik::Close { id, sender, reply } => {
                match self.synoik.notifications.close_checked(id, &sender) {
                    Ok(effects) => {
                        let _ = reply.send_blocking(Ok(()));
                        self.apply_notification_effects(effects);
                    }
                    Err(err) => {
                        let _ = reply.send_blocking(Err(err));
                    }
                }
            }
            NotificationsToSynoik::SenderVanished(name) => {
                let effects = self.synoik.notifications.sender_vanished(&name);
                self.apply_notification_effects(effects);
            }
            NotificationsToSynoik::AddGtk { req } => {
                let now = self.synoik.clock.now_unadjusted();
                let show_banners = !self.synoik.gnome_settings.quick_toggles.do_not_disturb;
                let effects = self.synoik.notifications.add_gtk(req, show_banners, now);
                self.apply_notification_effects(effects);
            }
            NotificationsToSynoik::RemoveGtk { app_id, gtk_id } => {
                let effects = self.synoik.notifications.remove_gtk(&app_id, &gtk_id);
                self.apply_notification_effects(effects);
            }
        }
    }

    /// A calendar update from the `org.gnome.Shell.CalendarServer` watcher (see
    /// `dbus::calendar_server`): mutate the store and refresh the open Events
    /// section. Ports `DBusEventSource`'s signal handlers (`js/ui/calendar.js`).
    pub fn on_calendar_events_msg(&mut self, msg: crate::calendar_events::CalendarToSynoik) {
        use crate::calendar_events::CalendarToSynoik;
        let changed = match msg {
            CalendarToSynoik::EventsAddedOrUpdated(batch) => {
                self.synoik.calendar_events.add_or_update(batch)
            }
            CalendarToSynoik::EventsRemoved(ids) => self.synoik.calendar_events.remove(&ids),
            CalendarToSynoik::ClientDisappeared(uid) => {
                self.synoik.calendar_events.client_disappeared(&uid)
            }
            CalendarToSynoik::CacheReset => {
                // A range change wipes the cache before the new range loads,
                // like GNOME's forced `_loadEvents` (`js/ui/calendar.js:356-360`).
                self.synoik.calendar_events.reset();
                true
            }
            CalendarToSynoik::HasCalendars(has) => {
                self.synoik.calendar_events.set_has_calendars(has)
            }
            CalendarToSynoik::OwnerAppeared => {
                // Reset cache; the watcher re-requests the range forcefully.
                self.synoik.calendar_events.reset();
                true
            }
            CalendarToSynoik::OwnerVanished => {
                self.synoik.calendar_events.reset();
                self.synoik.calendar_events.set_has_calendars(false);
                true
            }
        };
        if changed {
            self.synoik.refresh_popover_calendar_events();
        }
    }

    /// An MPRIS watcher update (see [`crate::mpris`]). Resolving `DesktopEntry` to an app is the
    /// one part gnome-shell does inside `_updateState` (`mpris.js:167-172`) that we cannot do on
    /// the watcher's side of the seam: the app system lives here.
    pub fn on_mpris_msg(&mut self, msg: crate::mpris::MprisToSynoik) {
        use crate::mpris::MprisToSynoik;

        let changed = match msg {
            MprisToSynoik::PlayerUpdated { bus_name, state } => {
                // `mpris.js:167-172`: the `DesktopEntry` property, and nothing else — no pid or
                // identity fallback, unlike a notification's `_getApp`.
                let app = state
                    .desktop_entry
                    .as_deref()
                    .and_then(|entry| self.synoik.app_system.lookup_desktop_id(entry));
                self.synoik.mpris.update(bus_name, *state, app)
            }
            MprisToSynoik::PlayerRemoved { bus_name } => self.synoik.mpris.remove(&bus_name),
        };

        if changed {
            // Art first: GNOME resolves the message's icon as the player appears, so the fetch
            // starts now rather than when the popover is opened.
            self.synoik.refresh_media_art();
            self.synoik.refresh_popover_media();
        }
    }

    /// An app indicator appeared, changed or went away.
    pub fn on_status_notifier_msg(&mut self, msg: crate::status_notifier::StatusNotifierToSynoik) {
        use crate::status_notifier::StatusNotifierToSynoik as Msg;

        let changed = match msg {
            Msg::ItemUpdated { item, props } => self.synoik.status_notifier.upsert(item, *props),
            Msg::ItemUnregistered { id } => self.synoik.status_notifier.remove(&id),
            Msg::ActivationUnsupported { item_id } => {
                // Nothing drawn changes — the *next* left click on this icon opens its menu
                // instead of vanishing into a method the client does not have.
                self.synoik
                    .status_notifier
                    .set_activation_unsupported(&item_id);
                return;
            }
            Msg::MenuLayout { item_id, root } => {
                // Straight to the popover, not into a store: a remote menu has no life outside
                // the menu that is showing it, and the *next* open re-reads it anyway.
                if self
                    .synoik
                    .panel_popover
                    .set_indicator_layout(&item_id, &root)
                {
                    self.synoik.queue_redraw_all();
                }
                return;
            }
        };

        // Only a change the panel would *show* is worth a redraw: a client that repaints a
        // passive item's icon costs nothing here.
        if changed {
            self.synoik.refresh_panel_indicators();
        }
    }

    /// Ask an indicator's client to do something to its open menu.
    ///
    /// Dropped unless the watcher is following *that* item's menu right now: a request for a menu
    /// that is no longer open would be delivered to whatever menu replaced it, and node ids are
    /// only meaningful within one client's tree.
    pub fn send_indicator_menu_request(
        &mut self,
        item_id: &str,
        request: crate::status_notifier::SynoikToStatusNotifier,
    ) {
        if self.synoik.indicator_menu_open.as_deref() != Some(item_id) {
            return;
        }
        if let Some(tx) = self.synoik.status_notifier_emit.as_ref() {
            let _ = tx.send_blocking(request);
        }
    }

    /// Place a just-mapped window under the tray icon that opened it, if one did.
    ///
    /// Matched by activation token only — see [`IndicatorActivation`] for why the PID is not an
    /// option. A window that arrives without our token is left where the floating layout put it,
    /// which is the behavior we had before; there is nothing to fall back to that would not be a
    /// guess about which window belongs to which icon.
    ///
    /// The rule is a panel menu's: the window's top edge sits below the panel by the same margin a
    /// popover keeps, and its right edge lines up with the icon's, clamped into the work area.
    /// Invented, because GNOME has no tray to be faithful to.
    pub fn place_indicator_window(&mut self, surface: &WlSurface, token: Option<&str>) {
        let now = get_monotonic_time();
        self.synoik
            .indicator_activations
            .retain(|a| a.expires > now);

        // Whether a client passes our token on to the window it opens is the one thing this
        // mechanism hangs on, and it is not knowable from the spec — so say what happened while
        // any activation is outstanding. A window that arrives with `token=None` while one is
        // waiting is a client that dropped it, and that is the finding, not a silent no-op.
        if !self.synoik.indicator_activations.is_empty() {
            debug!(
                "status-notifier: a window mapped with token={token:?} while {} indicator \
                 activation(s) were waiting",
                self.synoik.indicator_activations.len(),
            );
        }

        let Some(token) = token else {
            return;
        };
        let Some(idx) = self
            .synoik
            .indicator_activations
            .iter()
            .position(|a| a.token == token)
        else {
            return;
        };
        // Single use: the click opened this window and no other.
        let activation = self.synoik.indicator_activations.remove(idx);

        let Some((mapped, window_output)) = self.synoik.layout.find_window_and_output(surface)
        else {
            return;
        };
        // A window that landed on another output has its own reason to be there (a window rule, a
        // workspace target); moving it under an icon it cannot see would be worse than leaving it.
        if window_output != Some(&activation.output) {
            return;
        }
        let window = mapped.window.clone();
        let width = f64::from(mapped.expected_size().map_or(0, |size| size.w));

        let Some(area) = self
            .synoik
            .layout
            .monitor_for_output(&activation.output)
            .map(|mon| mon.working_area())
        else {
            return;
        };

        // Right-aligned under the icon, like the menu the other button opens.
        let anchor = activation.anchor;
        let x = (anchor.loc.x + anchor.size.w - width - area.loc.x)
            .clamp(0., (area.size.w - width).max(0.));
        let y = crate::ui::popover::POPOVER_MARGIN;

        debug!(
            "status-notifier: placing a window from {:?} at {x},{y} in the work area",
            anchor
        );
        self.synoik.layout.move_floating_window(
            Some(&window),
            synoik_ipc::PositionChange::SetFixed(x),
            synoik_ipc::PositionChange::SetFixed(y),
            false,
        );
    }

    /// Keep the watcher's idea of which menu is open in step with the popover's.
    ///
    /// Reconciled rather than driven from the open/close call sites: a popover is dismissed from
    /// half a dozen places (Escape, an outside click, another menu opening, the overview, a lock),
    /// and a client left believing its menu is still up is a bug that only shows in that client.
    /// Run every cycle, so it must be idempotent — it is: it only acts on a difference.
    pub fn reconcile_indicator_menu(&mut self) {
        let want = self
            .synoik
            .panel_popover
            .indicator_menu_item()
            .map(str::to_owned);
        if want == self.synoik.indicator_menu_open {
            return;
        }

        let Some(tx) = self.synoik.status_notifier_emit.as_ref() else {
            self.synoik.indicator_menu_open = want;
            return;
        };

        match &want {
            // `OpenMenu` supersedes whatever the watcher was following, so a *swap* needs no
            // separate close — the dispatcher tells the old client for us.
            Some(id) => match self
                .synoik
                .status_notifier
                .address(id)
                .filter(|a| a.menu_path.is_some())
            {
                Some(address) => {
                    let _ = tx.send_blocking(
                        crate::status_notifier::SynoikToStatusNotifier::OpenMenu {
                            item_id: id.clone(),
                            dest: address.dest,
                            item_path: address.item_path,
                            menu_path: address.menu_path.expect("filtered above"),
                        },
                    );
                }
                // The item went away between the click and here, or never had a menu. Nothing to
                // follow; the menu stays empty until the user dismisses it.
                None => {
                    let _ =
                        tx.send_blocking(crate::status_notifier::SynoikToStatusNotifier::CloseMenu);
                }
            },
            None => {
                let _ = tx.send_blocking(crate::status_notifier::SynoikToStatusNotifier::CloseMenu);
            }
        }

        self.synoik.indicator_menu_open = want;
    }

    /// A media card's transport control (`mpris.js:73-91`). The player is addressed by bus name,
    /// so a card whose player vanished between the click and the call is simply ignored by the
    /// watcher, not an error here.
    pub fn mpris_control(&mut self, command: crate::mpris::SynoikToMpris) {
        let Some(tx) = self.synoik.mpris_emit.as_ref() else {
            return;
        };
        let _ = tx.send_blocking(command);
    }

    /// See [`Synoik::apply_notification_effects`].
    pub fn apply_notification_effects(&mut self, effects: crate::notifications::Effects) {
        self.synoik.apply_notification_effects(effects);
    }

    /// gnome-session's `EndSessionDialog.Open`/`Close` land here (see `dbus::gnome_session`): raise
    /// or dismiss the logout/shutdown/restart confirmation. Confirm/cancel come from input, not the
    /// bus (`confirm_end_session`/`cancel_end_session`).
    pub fn on_end_session_msg(
        &mut self,
        msg: crate::dbus::gnome_session::EndSessionDialogToSynoik,
    ) {
        use crate::dbus::gnome_session::EndSessionDialogToSynoik;
        use crate::end_session::EndSessionType;

        let now = self.synoik.clock.now_unadjusted();
        match msg {
            EndSessionDialogToSynoik::Open { kind, seconds } => {
                let kind = EndSessionType::from_u32(kind);
                self.synoik.end_session.open(kind, seconds, now);
                // Before gnome-software answers the presentation is just the wire type; a restart
                // with an already-scheduled update is promoted when the reply lands.
                let presentation = self
                    .synoik
                    .end_session
                    .presentation()
                    .expect("a dialog was just opened");
                self.synoik.end_session_dialog.show(presentation);
                self.synoik.end_session_dialog.set_content(
                    presentation,
                    self.synoik.end_session.seconds_left(now),
                    self.synoik.update_checkbox(),
                );
                // Ask gnome-software what is pending. Asynchronous: the dialog is already up, and
                // the checkbox appears if and when the answer says there is something to install.
                self.synoik.query_offline_updates();
                self.synoik.reschedule_end_session_timer();
                self.synoik.queue_redraw_all();
            }
            EndSessionDialogToSynoik::Close => {
                if self.synoik.end_session.close() {
                    self.synoik.end_session_dialog.hide();
                    self.synoik.reschedule_end_session_timer();
                    self.synoik.queue_redraw_all();
                }
            }
        }
    }

    /// Register an external accelerator grab, mirroring
    /// `meta_display_grab_accelerator`: an unparseable accelerator or one that
    /// already resolves to an existing keybinding or grab is refused with 0
    /// (first grabber wins), otherwise the new dynamic action id is returned.
    pub fn grab_accelerator(
        &mut self,
        accelerator: &str,
        mode_flags: u32,
        grab_flags: u32,
        sender: String,
    ) -> u32 {
        let Ok(Some(accel)) = crate::gnome::parse_accelerator(accelerator) else {
            warn!("refusing to grab unparseable accelerator {accelerator:?}");
            return 0;
        };

        // Mutter keeps builtins and external grabs in one table and refuses a grab whose
        // combo already resolves (`meta_display_grab_accelerator`, keybindings.c:1297).
        // That is first-come-first-served, and our keybindings always exist before anything
        // can connect to D-Bus — so a naive port would let *any* key of ours outrank a
        // session component. Keys GNOME itself names may do that, as in mutter. Keys only
        // we have may not: they are inherited-from-niri extra capability, and
        // gnome-settings-daemon owns lock, logout and the media keys. So the grab is
        // refused only by GNOME's own keys and by earlier grabs; ours yield, here and in
        // `find_bind`.
        let conflict = self
            .synoik
            .gnome_settings
            .keybindings
            .iter()
            .find(|kb| {
                matches!(kb.action, crate::gnome::KeybindingAction::Gnome(_))
                    && kb.accels.contains(&accel)
            })
            .map(|kb| format!("GNOME's own {:?}", kb.action))
            .or_else(|| {
                self.synoik
                    .accel_grabs
                    .iter()
                    .find(|g| g.accel == accel)
                    .map(|g| format!("a grab held by {}", g.owner))
            });
        if let Some(conflict) = conflict {
            warn!(
                "refusing {sender}'s grab of {accelerator:?}: already taken by {conflict}. \
                 That key will not reach {sender}."
            );
            return 0;
        }

        // Ours lose the chord rather than the grabber. Still worth saying: the settings key
        // is now dead, which is what `synoik_accels_do_not_collide_with_anything_gnome_ships`
        // exists to keep from shipping.
        if let Some(ours) = self
            .synoik
            .gnome_settings
            .keybindings
            .iter()
            .find(|kb| kb.accels.contains(&accel))
        {
            warn!(
                "{sender} grabbed {accelerator:?}, which our own {:?} also wants; \
                 ours yields and that key of ours now does nothing",
                ours.action
            );
        }

        let action = self.synoik.next_accel_grab_action;
        self.synoik.next_accel_grab_action += 1;
        debug!("grabbed accelerator {accelerator:?} for {sender}: action {action}");
        self.synoik.accel_grabs.push(AccelGrab {
            action,
            accel,
            mode_flags,
            grab_flags,
            owner: sender,
        });
        action
    }

    /// Remove an accelerator grab. Grabbers can only ungrab their own
    /// actions, like gnome-shell; returns whether a grab was removed.
    pub fn ungrab_accelerator(&mut self, action: u32, sender: &str) -> bool {
        let grabs = &mut self.synoik.accel_grabs;
        let index = grabs
            .iter()
            .position(|g| g.action == action && g.owner == sender);
        if let Some(index) = index {
            grabs.remove(index);
        }
        index.is_some()
    }
}

impl Synoik {
    pub fn new(
        config: Rc<RefCell<Config>>,
        event_loop: LoopHandle<'static, State>,
        stop_signal: LoopSignal,
        display: Display<State>,
        backend: &Backend,
        wayland_socket: WaylandSocket,
        is_session_instance: bool,
    ) -> Self {
        let _span = tracy_client::span!("Synoik::new");

        let (executor, scheduler) = calloop::futures::executor().unwrap();
        event_loop.insert_source(executor, |_, _, _| ()).unwrap();

        let display_handle = display.handle();
        let config_ = config.borrow();
        let config_file_output_config = config_.outputs.clone();

        let mut animation_clock = Clock::default();

        let rate = 1.0 / config_.animations.slowdown.max(0.001);
        animation_clock.set_rate(rate);
        animation_clock.set_complete_instantly(config_.animations.off);

        let layout = Layout::new(animation_clock.clone(), &config_);

        let (blocker_cleared_tx, blocker_cleared_rx) = mpsc::channel();

        fn client_is_unrestricted(client: &Client) -> bool {
            !client.get_data::<ClientState>().unwrap().restricted
        }

        let compositor_state = CompositorState::new_v6::<State>(&display_handle);
        let xdg_shell_state = XdgShellState::new_with_capabilities::<State>(
            &display_handle,
            [WmCapabilities::Fullscreen, WmCapabilities::Maximize],
        );
        let xdg_decoration_state =
            XdgDecorationState::new_with_filter::<State, _>(&display_handle, |client| {
                client
                    .get_data::<ClientState>()
                    .unwrap()
                    .can_view_decoration_globals
            });
        let kde_decoration_state = KdeDecorationState::new_with_filter::<State, _>(
            &display_handle,
            // If we want CSD we will hide the global.
            KdeDecorationsMode::Server,
            |client| {
                client
                    .get_data::<ClientState>()
                    .unwrap()
                    .can_view_decoration_globals
            },
        );
        let layer_shell_state = WlrLayerShellState::new_with_filter::<State, _>(
            &display_handle,
            client_is_unrestricted,
        );
        let session_lock_state =
            SessionLockManagerState::new::<State, _>(&display_handle, client_is_unrestricted);
        let shm_state = ShmState::new::<State>(
            &display_handle,
            vec![wl_shm::Format::Xbgr8888, wl_shm::Format::Abgr8888],
        );
        let output_manager_state =
            OutputManagerState::new_with_xdg_output::<State>(&display_handle);
        let dmabuf_state = DmabufState::new();
        // Created lazily by the tty backend once the primary GPU is known (needs a DRM device).
        let drm_syncobj_state = None;
        let fractional_scale_manager_state =
            FractionalScaleManagerState::new::<State>(&display_handle);
        let mut seat_state = SeatState::new();
        let tablet_state = TabletManagerState::new::<State>(&display_handle);
        let pointer_gestures_state = PointerGesturesState::new::<State>(&display_handle);
        let relative_pointer_state = RelativePointerManagerState::new::<State>(&display_handle);
        let pointer_constraints_state = PointerConstraintsState::new::<State>(&display_handle);
        let idle_notifier_state = IdleNotifierState::new(&display_handle, event_loop.clone());
        let idle_inhibit_manager_state = IdleInhibitManagerState::new::<State>(&display_handle);
        let idle_monitor = crate::idle_monitor::IdleMonitor::new(animation_clock.now_unadjusted());
        let data_device_state = DataDeviceState::new::<State>(&display_handle);
        let primary_selection_state =
            PrimarySelectionState::new_with_filter::<State, _>(&display_handle, |client| {
                !client
                    .get_data::<ClientState>()
                    .unwrap()
                    .primary_selection_disabled
            });
        let wlr_data_control_state = WlrDataControlState::new::<State, _>(
            &display_handle,
            Some(&primary_selection_state),
            client_is_unrestricted,
        );
        let ext_data_control_state = ExtDataControlState::new::<State, _>(
            &display_handle,
            Some(&primary_selection_state),
            client_is_unrestricted,
        );
        let presentation_state =
            PresentationState::new::<State>(&display_handle, Monotonic::ID as u32);
        let security_context_state =
            SecurityContextState::new::<State, _>(&display_handle, client_is_unrestricted);

        let text_input_state = TextInputManagerState::new::<State>(&display_handle);
        let input_method_state =
            InputMethodManagerState::new::<State, _>(&display_handle, client_is_unrestricted);
        let keyboard_shortcuts_inhibit_state =
            KeyboardShortcutsInhibitState::new::<State>(&display_handle);
        let virtual_keyboard_state =
            VirtualKeyboardManagerState::new::<State, _>(&display_handle, client_is_unrestricted);
        let virtual_pointer_state =
            VirtualPointerManagerState::new::<State, _>(&display_handle, client_is_unrestricted);
        let foreign_toplevel_state =
            ForeignToplevelManagerState::new::<State, _>(&display_handle, client_is_unrestricted);
        let ext_workspace_state =
            ExtWorkspaceManagerState::new::<State, _>(&display_handle, client_is_unrestricted);
        let mut output_management_state =
            OutputManagementManagerState::new::<State, _>(&display_handle, client_is_unrestricted);
        output_management_state.on_config_changed(config_.outputs.clone());
        let screencopy_state =
            ScreencopyManagerState::new::<State, _>(&display_handle, client_is_unrestricted);
        let viewporter_state = ViewporterState::new::<State>(&display_handle);
        let background_effect_state = BackgroundEffectState::new::<State>(&display_handle);
        let xdg_foreign_state = XdgForeignState::new::<State>(&display_handle);

        let is_tty = matches!(backend, Backend::Tty(_));
        let gamma_control_manager_state =
            GammaControlManagerState::new::<State, _>(&display_handle, move |client| {
                is_tty && !client.get_data::<ClientState>().unwrap().restricted
            });
        let activation_state = XdgActivationState::new::<State>(&display_handle);
        event_loop
            .insert_source(
                Timer::from_duration(XDG_ACTIVATION_TOKEN_TIMEOUT),
                |_, _, state| {
                    state
                        .synoik
                        .activation_state
                        .retain_tokens(|_, token_data| {
                            token_data.timestamp.elapsed() < XDG_ACTIVATION_TOKEN_TIMEOUT
                        });
                    TimeoutAction::ToDuration(XDG_ACTIVATION_TOKEN_TIMEOUT)
                },
            )
            .unwrap();

        let mutter_x11_interop_state =
            MutterX11InteropManagerState::new::<State, _>(&display_handle, move |_| true);
        // Starts empty and in memory; `State::new` loads the real file, except in tests.
        let session_manager_state = SessionManagerState::new::<State, _>(
            &display_handle,
            crate::session_state::SessionStore::in_memory(),
            move |_| true,
        );

        #[cfg(test)]
        let single_pixel_buffer_state = SinglePixelBufferState::new::<State>(&display_handle);

        let mut seat: Seat<State> = seat_state.new_wl_seat(&display_handle, backend.seat_name());
        let keyboard = match seat.add_keyboard(
            config_.input.keyboard.xkb.to_xkb_config(),
            config_.input.keyboard.repeat_delay.into(),
            config_.input.keyboard.repeat_rate.into(),
        ) {
            Err(err) => {
                if let smithay::input::keyboard::Error::BadKeymap = err {
                    warn!("error loading the configured xkb keymap, trying default");
                } else {
                    warn!("error adding keyboard: {err:?}");
                }
                seat.add_keyboard(
                    Default::default(),
                    config_.input.keyboard.repeat_delay.into(),
                    config_.input.keyboard.repeat_rate.into(),
                )
                .unwrap()
            }
            Ok(keyboard) => keyboard,
        };
        if config_.input.keyboard.numlock {
            let mut modifier_state = keyboard.modifier_state();
            modifier_state.num_lock = true;
            keyboard.set_modifier_state(modifier_state);
        }
        seat.add_pointer();

        let cursor_shape_manager_state = CursorShapeManagerState::new::<State>(&display_handle);
        let cursor_manager =
            CursorManager::new(&config_.cursor.xcursor_theme, config_.cursor.xcursor_size);

        let mod_key = backend.mod_key(&config.borrow());
        // The compiled-in model, which a live session replaces right after
        // construction (and then calls `refresh_keybinding_state`). Headless tests keep
        // exactly this one, so the fast-path sets have to be built from it here.
        let gnome_settings = GnomeSettings::default();
        let keybindings = &gnome_settings.keybindings;
        let mods_with_mouse_binds = mods_with_mouse_binds(keybindings);
        let mods_with_wheel_binds = mods_with_wheel_binds(keybindings);
        let mods_with_finger_scroll_binds = mods_with_finger_scroll_binds(keybindings);
        let mods_with_tablet_stylus_binds = mods_with_tablet_stylus_binds(keybindings);

        let screenshot_ui = ScreenshotUi::new(animation_clock.clone(), config.clone());
        let notification_banner = crate::ui::notification_banner::NotificationBanner::new(
            animation_clock.clone(),
            config.clone(),
        );
        let osd = crate::ui::osd::OsdManager::new(animation_clock.clone(), config.clone());
        let switcher =
            crate::ui::switcher::ui::SwitcherUi::new(animation_clock.clone(), config.clone());

        // No "Important Hotkeys" card on login: GNOME shows nothing over a fresh
        // session, and a modal cheat-sheet in front of the desktop is niri's
        // welcome, not GNOME's. The overlay itself stays available on demand
        // (`Action::ShowHotkeyOverlay`).
        let hotkey_overlay = HotkeyOverlay::new(config.clone(), mod_key, keybindings);

        let exit_confirm_dialog = ExitConfirmDialog::new(animation_clock.clone(), config.clone());
        let end_session_dialog = EndSessionDialog::new(animation_clock.clone(), config.clone());
        let polkit_ui =
            crate::ui::polkit_dialog::PolkitDialogUi::new(animation_clock.clone(), config.clone());
        let panel_popover = PanelPopover::new(animation_clock.clone(), config.clone());
        let panel = Panel::new(animation_clock.clone(), config.clone());

        let a11y = A11y::new(event_loop.clone());

        event_loop
            .insert_source(
                Timer::from_duration(Duration::from_secs(1)),
                |_, _, state| {
                    state.synoik.send_frame_callbacks_on_fallback_timer();
                    TimeoutAction::ToDuration(Duration::from_secs(1))
                },
            )
            .unwrap();

        let socket_name = match &wayland_socket {
            WaylandSocket::None => None,
            socket => {
                let source = match socket {
                    WaylandSocket::Auto => ListeningSocketSource::new_auto(),
                    WaylandSocket::Named(name) => ListeningSocketSource::with_name(name),
                    WaylandSocket::None => unreachable!(),
                };
                // Name the directory and the socket: a bare `BindError` says neither, and the two
                // ways this fails are both about *where* — a name already taken by another
                // instance, or an `XDG_RUNTIME_DIR` whose path plus the socket name overflows the
                // 108-byte `sockaddr_un` limit. Guessing that from "invalid argument" costs an
                // afternoon.
                let source = source.unwrap_or_else(|err| {
                    panic!(
                        "cannot bind the Wayland socket ({socket:?}) in {}: {err}",
                        std::env::var("XDG_RUNTIME_DIR")
                            .unwrap_or_else(|_| "$XDG_RUNTIME_DIR (unset)".to_owned()),
                    )
                });
                let socket_name = source.socket_name().to_os_string();
                event_loop
                    .insert_source(source, move |client, _, state| {
                        state.synoik.insert_client(NewClient {
                            client,
                            restricted: false,
                            credentials_unknown: false,
                        });
                    })
                    .unwrap();
                Some(socket_name)
            }
        };

        let ipc_server = match IpcServer::start(&event_loop, socket_name.as_deref()) {
            Ok(server) => Some(server),
            Err(err) => {
                warn!("error starting IPC server: {err:?}");
                None
            }
        };

        #[cfg(feature = "xdp-gnome-screencast")]
        let screencasting = Screencasting::new(&event_loop);

        let display_source = Generic::new(display, Interest::READ, Mode::Level);
        event_loop
            .insert_source(display_source, |_, display, state| {
                // SAFETY: we don't drop the display.
                unsafe {
                    display.get_mut().dispatch_clients(state).unwrap();
                }
                Ok(PostAction::Continue)
            })
            .unwrap();

        // Census the live `VkDeviceMemory` into the log, so a long session leaves a time series a
        // leak's slope is readable from. It is a *timer* and not a per-frame hook on purpose:
        // memory this instrument exists to find (see `synoik_vk::devmem`) lives on the host, in the
        // VMM, where no guest process accounting reaches it — an idle compositor that is quietly
        // retaining is exactly the case a frame-driven sample would miss.
        //
        // On its own `devmem` target so a seat can turn the census on *alone*
        // (`RUST_LOG=synoik=info,devmem=debug`) instead of running a whole desktop at debug for the
        // days a slow leak takes to show — which is noisy enough that nobody would leave it on, and
        // an instrument nobody leaves on measures nothing.
        if tracing::enabled!(target: DEVICE_MEMORY_CENSUS_TARGET, tracing::Level::DEBUG) {
            event_loop
                .insert_source(Timer::immediate(), |_, _, _state| {
                    debug!(target: DEVICE_MEMORY_CENSUS_TARGET, "{}", synoik_vk::devmem::census(8));
                    TimeoutAction::ToDuration(DEVICE_MEMORY_CENSUS_PERIOD)
                })
                .unwrap();
        }

        event_loop
            .insert_source(
                Timer::from_duration(Duration::from_secs(60)),
                |_, _, state| {
                    let _span = tracy_client::span!("startup timeout");
                    state.synoik.is_at_startup = false;
                    state.synoik.recompute_window_rules();
                    state.synoik.recompute_layer_rules();
                    TimeoutAction::Drop
                },
            )
            .unwrap();

        // Tick the panel clock on each minute boundary. The timer is the wake
        // source, so this works even when input is idle (no frame starvation).
        event_loop
            .insert_source(
                Timer::from_duration(Duration::from_secs(
                    crate::ui::panel::secs_until_next_minute(),
                )),
                |_, _, state| {
                    // The shield's curtain shows the same clock and is the only thing on screen
                    // while it is down, so it rides this tick too — explicitly, rather than
                    // relying on the panel's label happening to change on the same boundary.
                    if state.synoik.panel.update_clock() || state.synoik.screen_shield.is_active() {
                        state.synoik.queue_redraw_all();
                    }
                    // ...and so does the unlock prompt's two-minute escape back to the clock.
                    // Riding the clock tick makes that granular to a minute rather than exact,
                    // which is the right trade for a timeout whose only job is to not leave a
                    // half-typed password on an unattended screen.
                    if state.synoik.unlock_dialog.is_waiting_to_escape() {
                        let now = crate::utils::get_monotonic_time();
                        let effects = state.synoik.unlock_dialog.tick(now);
                        state.apply_unlock_effects(effects);
                    }
                    // Refresh the open World Clocks section's live times/offsets on
                    // the same tick (gnome-shell's `WallClock notify::clock`).
                    state.synoik.refresh_popover_world_clocks();
                    // Re-arm for the next second (when showing seconds) or the next
                    // minute boundary, per the current clock format.
                    TimeoutAction::ToDuration(state.synoik.panel.clock_tick_interval())
                },
            )
            .unwrap();

        drop(config_);
        let mut synoik = Self {
            input_method: None,
            im_surrounding: None,
            im_key_timer: None,
            config,
            config_file_output_config,

            event_loop,
            scheduler,
            stop_signal,
            socket_name,
            display_handle,
            is_session_instance,
            start_time: Instant::now(),
            is_at_startup: true,
            clock: animation_clock.clone(),

            layout,
            global_space: Space::default(),
            sorted_outputs: Vec::default(),
            output_state: HashMap::new(),
            unmapped_windows: HashMap::new(),
            unmapped_layer_surfaces: HashSet::new(),
            mapped_layer_surfaces: HashMap::new(),
            root_surface: HashMap::new(),
            dmabuf_pre_commit_hook: HashMap::new(),
            blocker_cleared_tx,
            blocker_cleared_rx,
            monitors_active: true,
            is_lid_closed: false,

            devices: HashSet::new(),
            tablets: HashMap::new(),
            touch: HashSet::new(),

            compositor_state,
            xdg_shell_state,
            xdg_decoration_state,
            kde_decoration_state,
            layer_shell_state,
            session_lock_state,
            foreign_toplevel_state,
            ext_workspace_state,
            output_management_state,
            screencopy_state,
            viewporter_state,
            background_effect_state,
            xdg_foreign_state,
            text_input_state,
            input_method_state,
            keyboard_shortcuts_inhibit_state,
            virtual_keyboard_state,
            virtual_pointer_state,
            shm_state,
            output_manager_state,
            dmabuf_state,
            drm_syncobj_state,
            fractional_scale_manager_state,
            seat_state,
            tablet_state,
            pointer_gestures_state,
            relative_pointer_state,
            pointer_constraints_state,
            idle_notifier_state,
            idle_monitor,
            idle_monitor_timer: None,
            end_session: crate::end_session::EndSession::new(),
            end_session_timer: None,
            offline_update_tx: None,
            recording_tick: None,
            idle_inhibit_manager_state,
            data_device_state,
            clipboard_mime_types: Vec::new(),
            clipboard_paste_pending: false,
            primary_selection_state,
            wlr_data_control_state,
            ext_data_control_state,
            popups: PopupManager::default(),
            popup_grab: None,
            gnome_settings,
            gnome_settings_writer: None,
            app_system: crate::app_system::AppSystem::disconnected(),
            app_catalog_reload_at: None,
            audio: None,
            mic: crate::audio::MicStatus::default(),
            sink_list: crate::audio::SinkList::default(),
            source_list: crate::audio::SourceList::default(),
            audio_cards: crate::audio::AudioCards::default(),
            headphones: None,
            audio_backend: None,
            system_status: SystemStatus::default(),
            battery_override: None,
            notifications: crate::notifications::NotificationStore::default(),
            notifications_emit: None,
            gtk_notifications_emit: None,
            calendar_events: crate::calendar_events::CalendarEventStore::default(),
            calendar_range_emit: None,
            mpris: crate::mpris::MprisStore::new(),
            mpris_emit: None,
            status_notifier: crate::status_notifier::IndicatorStore::new(),
            status_notifier_emit: None,
            indicator_menu_open: None,
            indicator_activations: Vec::new(),
            notification_banner,
            notification_banner_timer: None,
            osd,
            osd_timer: None,
            osd_timer_at: None,
            dock_timer: None,
            dock_timer_at: None,
            switcher_timer: None,
            switcher_timer_at: None,
            pending_switcher_outcome: None,
            cycler_highlight: None,
            switcher_ws_preview: Vec::new(),
            last_power_profile: "power-saver".to_string(),
            system_status_tx: None,
            backlight: crate::backlight::BacklightSnapshot::default(),
            brightness: crate::brightness::BrightnessManager::default(),
            brightness_emit: None,
            screen_shield: crate::screen_shield::ScreenShield::new(Default::default()),
            lock_screen: Default::default(),
            unlock_dialog: crate::unlock_dialog::UnlockDialog::new(
                crate::unlock_dialog::session_user(),
            ),
            caps_lock: false,
            lock_replies: Vec::new(),
            published_active: false,
            user_account: Default::default(),
            fingerprint_reader: Default::default(),
            smartcard_detected: false,
            can_switch_user: false,
            switch_user_hovered: false,
            multiple_users: false,
            gdm_requests: None,
            lock_timer: None,
            fade_timer: None,
            unlock_message_timer: None,
            session_active: true,
            sleep_inhibitor: None,
            shield_frames_owed: HashSet::new(),
            shield_present_deadline: None,
            shield_snapshot: Default::default(),
            screen_saver_emit: None,
            login1_tx: None,
            wallpaper: Wallpaper::default(),
            accel_grabs: Vec::new(),
            accel_grab_release_pending: HashMap::new(),
            next_accel_grab_action: 100,
            last_user_action_time: None,
            suppressed_keys: HashSet::new(),
            overlay_key_armed: None,
            overlay_key_last_fired: None,
            suppressed_buttons: HashSet::new(),
            overview_pressed: None,
            app_grid_pan: None,
            bind_cooldown_timers: HashMap::new(),
            bind_repeat_timer: Option::default(),
            presentation_state,
            security_context_state,
            gamma_control_manager_state,
            activation_state,
            mutter_x11_interop_state,
            session_manager_state,
            session_save_timer: None,
            #[cfg(test)]
            single_pixel_buffer_state,

            seat,
            keyboard_focus: KeyboardFocus::Layout { surface: None },
            layer_shell_on_demand_focus: None,
            idle_inhibiting_surfaces: HashSet::new(),
            is_fdo_idle_inhibited: Arc::new(AtomicBool::new(false)),
            keyboard_shortcuts_inhibiting_surfaces: HashMap::new(),
            xkb_from_locale1: None,
            cursor_manager,
            cursor_texture_cache: Default::default(),
            cursor_shape_manager_state,
            dnd_icon: None,
            pointer_contents: PointContents::default(),
            pointer_visibility: PointerVisibility::Visible,
            pointer_inactivity_timer: None,
            pointer_inactivity_timer_got_reset: false,
            notified_activity_this_iteration: false,
            hot_corner_barrier: Barrier::hot_corner(),
            hot_corner_output: None,
            tablet_cursor_location: None,
            gesture_swipe_3f_cumulative: None,
            overview_scroll_swipe_gesture: ScrollSwipeGesture::new(),
            app_grid_scroll_swipe: ScrollSwipeGesture::new(),
            vertical_wheel_tracker: ScrollTracker::new(120),
            horizontal_wheel_tracker: ScrollTracker::new(120),
            mods_with_mouse_binds,
            mods_with_wheel_binds,
            mods_with_tablet_stylus_binds,

            // 10 is copied from Clutter: DISCRETE_SCROLL_STEP.
            vertical_finger_scroll_tracker: ScrollTracker::new(10),
            horizontal_finger_scroll_tracker: ScrollTracker::new(10),
            mods_with_finger_scroll_binds,

            lock_state: LockState::Unlocked,
            locked_hint: None,

            screenshot_ui,
            hotkey_overlay,
            exit_confirm_dialog,
            run_dialog: RunDialog::new(),
            end_session_dialog,
            pending_capture: None,
            capture_countdown: Default::default(),
            cast_area_indicator: Default::default(),
            select_area_reply: None,
            interactive_screenshot_reply: None,
            flashspot: crate::ui::flashspot::FlashSpot::new(),
            ripples: crate::ui::ripples::Ripples::new(),
            dock: crate::ui::dock::Dock::new(animation_clock.clone()),
            polkit_dialog: crate::polkit_dialog::PolkitDialog::new(),
            polkit_ui,
            polkit_requests: None,
            polkit_deferred: None,
            polkit_reset_timer: None,
            panel,
            panel_popover,
            overview_was_open: false,
            dash: Dash::new(animation_clock.clone()),
            overview_search: OverviewSearch::new(),
            app_grid: AppGrid::new(animation_clock.clone()),
            folder_dialog: crate::ui::folder_dialog::FolderDialog::new(animation_clock.clone()),
            preview_chrome: PreviewChrome::new(),
            preview_close_hovered: None,
            thumbnail_chrome: ThumbnailChrome::new(),
            thumbnail_hovered: None,
            thumbnail_close_hovered: None,
            app_drag: None,
            app_menu_source: None,
            app_menu_from_dock: false,
            app_icon_uploads: crate::ui::widget::SharedAppIconUploads::default(),
            app_drag_bg: RefCell::new(
                crate::render_helpers::rounded_solid::RoundedSolidBuffer::new(),
            ),
            grid_pending_move: None,
            grid_move_timer: None,
            grid_drop_target: None,
            grid_drop_timer: None,
            folder_popdown_timer: None,
            folder_pending_move: None,
            folder_move_timer: None,
            grid_page_switch_timer: None,
            grid_page_switch_overshoot: None,
            app_grid_last_page_flip: None,
            overview_search_was_visible: false,
            overview_search_fade: None,
            overview_search_fade_target: false,
            overview_search_expand: None,
            overview_search_expand_target: false,
            picker_offscreen: OffscreenBuffer::default(),
            thumbnails_offscreen: OffscreenBuffer::default(),
            icon_cache: IconCache::new("Adwaita"),
            symbolic_icon_tx: None,
            app_icon_cache: AppIconCache::new("Adwaita"),
            image_cache: ImageCache::new(),

            switcher,

            pick_window: None,
            pick_color: None,

            debug_draw_opaque_regions: false,
            debug_draw_damage: false,
            dump_scanout_next_frame: false,

            frame_log: FrameLog::from_env(),

            dbus: None,
            a11y_keyboard_monitor: None,
            a11y,
            inhibit_power_key_fd: None,

            ipc_server,
            ipc_outputs_changed: false,
            applied_display_config: HashMap::new(),

            satellite: None,

            #[cfg(feature = "xdp-gnome-screencast")]
            casting: screencasting,

            recordings_base: None,
        };

        synoik.reset_pointer_inactivity_timer();

        // One GPU upload map for every surface that draws app icons, as gnome-shell keeps
        // one Cogl texture per gicon+size shell-wide (`st-texture-cache.c:998`). The dash's
        // is the one they all take, so the drag proxy's map is the same object too.
        let shared = synoik.dash.icon_uploads();
        synoik.app_grid.share_icon_uploads(&shared);
        synoik.overview_search.share_icon_uploads(&shared);
        synoik.folder_dialog.share_icon_uploads(&shared);
        synoik.app_icon_uploads = shared;

        synoik
    }

    pub fn insert_client(&mut self, client: NewClient) {
        let NewClient {
            client,
            restricted,
            credentials_unknown,
        } = client;

        let config = self.config.borrow();
        let data = Arc::new(ClientState {
            compositor_state: Default::default(),
            can_view_decoration_globals: config.prefer_no_csd,
            primary_selection_disabled: config.clipboard.disable_primary,
            restricted,
            credentials_unknown,
        });

        if let Err(err) = self.display_handle.insert_client(client, data) {
            warn!("error inserting client: {err}");
        }
    }

    /// Queues a session store write if anything changed since the last one.
    ///
    /// Called from the debounce timer. Only the serialization happens here; the write itself is on
    /// the store's worker thread, since an `fsync` on the compositor thread is dropped frames.
    pub fn save_session_store(&mut self) {
        let store = &mut self.session_manager_state.store;
        if !store.is_dirty() {
            return;
        }
        if let Err(err) = store.save() {
            warn!("error serializing the session store: {err}");
        }
    }

    /// Cancels a pending debounced save and writes synchronously. For the shutdown path.
    ///
    /// This is the one blocking write, so a clean shutdown never loses state. A `SIGKILL` still
    /// costs up to `SESSION_SAVE_DELAY`, same as mutter.
    pub fn flush_session_store(&mut self) {
        if let Some(token) = self.session_save_timer.take() {
            self.event_loop.remove(token);
        }
        self.session_manager_state.store.flush();
    }

    pub fn inhibit_power_key(&mut self) -> anyhow::Result<()> {
        use smithay::reexports::rustix::io::{fcntl_setfd, FdFlags};

        let conn = zbus::blocking::Connection::system()?;

        let message = conn.call_method(
            Some("org.freedesktop.login1"),
            "/org/freedesktop/login1",
            Some("org.freedesktop.login1.Manager"),
            "Inhibit",
            &("handle-power-key", "synoik", "Power key handling", "block"),
        )?;

        let fd: zbus::zvariant::OwnedFd = message.body().deserialize()?;

        // Don't leak the fd to child processes.
        if let Err(err) = fcntl_setfd(&fd, FdFlags::CLOEXEC) {
            warn!("error setting CLOEXEC on inhibit fd: {err:?}");
        };

        self.inhibit_power_key_fd = Some(fd);

        Ok(())
    }

    /// Repositions all outputs, optionally adding a new output.
    pub fn reposition_outputs(&mut self, new_output: Option<&Output>) {
        let _span = tracy_client::span!("Synoik::reposition_outputs");

        #[derive(Debug)]
        struct Data {
            output: Output,
            name: OutputName,
            position: Option<Point<i32, Logical>>,
            config: Option<synoik_config::Position>,
        }

        let config = self.config.borrow();
        let mut outputs = vec![];
        for output in self.global_space.outputs().chain(new_output) {
            let name = output.user_data().get::<OutputName>().unwrap();
            let position = self.global_space.output_geometry(output).map(|geo| geo.loc);
            let config = config.outputs.find(name).and_then(|c| c.position);

            outputs.push(Data {
                output: output.clone(),
                name: name.clone(),
                position,
                config,
            });
        }
        drop(config);

        for Data { output, .. } in &outputs {
            self.global_space.unmap_output(output);
        }

        // Connectors can appear in udev in any order. If we sort by name then we get output
        // positioning that does not depend on the order they appeared.
        //
        // This sorting first compares by make/model/serial so that it is stable regardless of the
        // connector name. However, if make/model/serial is equal or unknown, then it does fall
        // back to comparing the connector name, which should always be unique.
        outputs.sort_unstable_by(|a, b| a.name.compare(&b.name));

        // Place all outputs with explicitly configured position first, then the unconfigured ones.
        outputs.sort_by_key(|d| d.config.is_none());

        trace!(
            "placing outputs in order: {:?}",
            outputs.iter().map(|d| &d.name.connector)
        );

        self.sorted_outputs = outputs
            .iter()
            .map(|Data { output, .. }| output.clone())
            .collect();

        for data in outputs.into_iter() {
            let Data {
                output,
                name,
                position,
                config,
            } = data;

            let size = output_size(&output).to_i32_round();

            let new_position = config
                .map(|pos| Point::from((pos.x, pos.y)))
                .filter(|pos| {
                    // Ensure that the requested position does not overlap any existing output.
                    let target_geom = Rectangle::new(*pos, size);

                    let overlap = self
                        .global_space
                        .outputs()
                        .map(|output| self.global_space.output_geometry(output).unwrap())
                        .find(|geom| geom.overlaps(target_geom));

                    if let Some(overlap) = overlap {
                        warn!(
                            "output {} at x={} y={} sized {}x{} \
                             overlaps an existing output at x={} y={} sized {}x{}, \
                             falling back to automatic placement",
                            name.connector,
                            pos.x,
                            pos.y,
                            size.w,
                            size.h,
                            overlap.loc.x,
                            overlap.loc.y,
                            overlap.size.w,
                            overlap.size.h,
                        );

                        false
                    } else {
                        true
                    }
                })
                .unwrap_or_else(|| {
                    let x = self
                        .global_space
                        .outputs()
                        .map(|output| self.global_space.output_geometry(output).unwrap())
                        .map(|geom| geom.loc.x + geom.size.w)
                        .max()
                        .unwrap_or(0);

                    Point::from((x, 0))
                });

            self.global_space.map_output(&output, new_position);

            // By passing new_output as an Option, rather than mapping it into a bogus location
            // in global_space, we ensure that this branch always runs for it.
            if Some(new_position) != position {
                debug!(
                    "putting output {} at x={} y={}",
                    name.connector, new_position.x, new_position.y
                );
                output.change_current_state(None, None, None, Some(new_position));
                self.ipc_outputs_changed = true;
                self.queue_redraw(&output);
            }
        }
    }

    /// Derives an output's scale and transform from the precedence chain described in
    /// [`State::reload_output_config`].
    ///
    /// Both the store lookup and the DPI guess read the output's *current mode*, so this must be
    /// re-run whenever the mode changes — see the mode-change branch in
    /// `Tty::on_output_config_changed`.
    pub fn derive_output_scale_transform(
        &self,
        output: &Output,
        monitors_config: Option<&crate::monitors_xml::MonitorsConfig>,
    ) -> (f64, Transform) {
        let name = output.user_data().get::<OutputName>().unwrap();
        let applied = self.applied_display_config.get(&name.connector);
        let config = self.config.borrow();
        let c = config.outputs.find(name);
        let saved = monitors_config.and_then(|m| m.setting_for(name, output.current_mode()));

        let scale = applied
            .and_then(|a| a.scale)
            .or_else(|| saved.map(|s| s.scale))
            .or_else(|| c.and_then(|c| c.scale).map(|s| s.0))
            .unwrap_or_else(|| {
                let size_mm = output.physical_properties().size;
                let resolution = output.current_mode().unwrap().size;
                guess_monitor_scale(size_mm, resolution)
            });
        let scale = closest_representable_scale(scale.clamp(0.1, 10.));

        let base_transform = applied
            .and_then(|a| a.transform)
            .or_else(|| saved.map(|s| s.transform))
            .unwrap_or_else(|| {
                c.map(|c| ipc_transform_to_smithay(c.transform))
                    .unwrap_or(Transform::Normal)
            });

        (scale, panel_orientation(output) + base_transform)
    }

    pub fn add_output(&mut self, output: Output, refresh_interval: Option<Duration>, vrr: bool) {
        let global = output.create_global::<State>(&self.display_handle);

        let name = output.user_data().get::<OutputName>().unwrap();

        // Same precedence as `reload_output_config` (see the rationale there): live-applied
        // session config, then GNOME's `monitors.xml` store (so a saved scale is honored from the
        // first frame, not just on reload), then the KDL config, then the DPI guess.
        let monitors_config = crate::monitors_xml::MonitorsConfig::load();
        let (scale, transform) =
            self.derive_output_scale_transform(&output, monitors_config.as_ref());

        let config = self.config.borrow();
        let c = config.outputs.find(name);

        let mut backdrop_color = c
            .and_then(|c| c.backdrop_color)
            .unwrap_or(config.overview.backdrop_color)
            .to_array_unpremul();
        backdrop_color[3] = 1.;

        let mut layout_config = c.and_then(|c| c.layout.clone());
        // Support the deprecated non-layout background-color key.
        if let Some(layout) = &mut layout_config {
            if layout.background_color.is_none() {
                layout.background_color = c.and_then(|c| c.background_color);
            }
        }
        drop(config);

        // Set scale and transform before adding to the layout since that will read the output size.
        output.change_current_state(
            None,
            Some(transform),
            Some(output::Scale::Fractional(scale)),
            None,
        );

        self.layout.add_output(output.clone(), layout_config);

        // A banner orphaned when its output (and every fallback) disappeared
        // adopts the first output that returns.
        self.notification_banner.adopt_output(&output);
        // `_monitorsChanged` (`js/ui/osdWindow.js:151-163`): one OSD per monitor.
        self.osd.add_output(&output);

        let lock_render_state = if self.is_locked() {
            // We haven't rendered anything yet so it's as good as locked.
            LockRenderState::Locked
        } else {
            LockRenderState::Unlocked
        };

        let size = output_size(&output);
        let state = OutputState {
            global,
            redraw_state: RedrawState::Idle,
            pending_aim: None,
            on_demand_vrr_enabled: false,
            unfinished_animations_remain: false,
            last_frame_scanout: ScanoutTally::default(),
            frame_clock: FrameClock::new(refresh_interval, vrr),
            last_drm_sequence: None,
            vblank_throttle: VBlankThrottle::new(self.event_loop.clone(), name.connector.clone()),
            frame_callback_sequence: 0,
            backdrop_buffer: SolidColorBuffer::new(size, backdrop_color),
            xray: Xray::new(),
            lock_render_state,
            lock_surface: None,
            lock_color_buffer: SolidColorBuffer::new(size, CLEAR_COLOR_LOCKED),
            shield_dim_buffer: SolidColorBuffer::new(size, SHIELD_DIM_COLOR),
            shield_backstop_buffer: SolidColorBuffer::new(size, [0., 0., 0., 1.]),
            screen_transition: None,
            debug_damage_tracker: OutputDamageTracker::from_output(&output),
            last_frame_elements: 0,
            last_frame_full_damage: false,
            last_frame_anim_causes: AnimCauses::empty(),
            shield_frame_queued: false,
        };
        let rv = self.output_state.insert(output.clone(), state);
        assert!(rv.is_none(), "output was already tracked");

        // Must be last since it will call queue_redraw(output) which needs things to be filled-in.
        self.reposition_outputs(Some(&output));

        // A new output means a new (scale, icon size) pair to warm. This does **not** cover
        // startup on a TTY seat: `backend.init` reaches here before the decode worker is
        // spawned, so this call finds no worker and warms nothing. `State::new` warms again
        // once the worker is up — see there. Idempotent, so both are safe.
        self.prewarm_app_icons();
        // ...and the same for the account picture, which is keyed on the scale too. The account
        // usually answers before any output exists, so without this the warm at that point has
        // nowhere to land and the first lock on the new output draws the fallback.
        self.warm_avatar();
    }

    pub fn output_exists(&self, output: &Output) -> bool {
        self.output_state.contains_key(output)
    }

    /// Converts a `WlOutput` to a corresponding `Output` if it exists.
    ///
    /// Compared to raw `Output::from_resource`, this method also verifies that the output still
    /// exists in synoik. Right after the output global is disabled, but before it is removed for
    /// good, `Output::from_resource` will succeed, but since synoik already forgot the output,
    /// accessing it can cause logic bugs.
    pub fn output_from_resource(&self, wl_output: &WlOutput) -> Option<Output> {
        Output::from_resource(wl_output).filter(|output| self.output_exists(output))
    }

    pub fn remove_output(&mut self, output: &Output) {
        for layer in layer_map_for_output(output).layers() {
            layer.layer_surface().send_close();
        }

        self.layout.remove_output(output);
        self.global_space.unmap_output(output);
        self.reposition_outputs(None);
        self.gamma_control_manager_state.output_removed(output);

        let state = self.output_state.remove(output).unwrap();

        match state.redraw_state {
            RedrawState::Idle => (),
            RedrawState::Queued => (),
            RedrawState::WaitingForVBlank { .. } => (),
            RedrawState::ScheduledDispatch { token, .. } => self.event_loop.remove(token),
            RedrawState::WaitingForEstimatedVBlank(token) => self.event_loop.remove(token),
            RedrawState::WaitingForEstimatedVBlankAndQueued(token) => self.event_loop.remove(token),
        }

        self.stop_casts_for_target(CastTarget::output(output));
        self.stop_native_recordings_for_output(output);
        self.screencopy_state.remove_output(output);

        // A banner shown on the removed output moves to the new active output
        // (GNOME re-parents to the new primary) — a marooned CRITICAL banner
        // would otherwise be unclickable and jam the queue forever.
        let fallback = self.layout.active_output().cloned();
        self.notification_banner.retarget_output(output, fallback);
        // An OSD does not migrate — GNOME destroys the departing monitor's window
        // (`js/ui/osdWindow.js:157-160`).
        self.osd.remove_output(output);

        // Disable the output global and remove some time later to give the clients some time to
        // process it.
        let global = state.global;
        self.display_handle.disable_global::<State>(global.clone());
        self.event_loop
            .insert_source(
                Timer::from_duration(Duration::from_secs(10)),
                move |_, _, state| {
                    state
                        .synoik
                        .display_handle
                        .remove_global::<State>(global.clone());
                    TimeoutAction::Drop
                },
            )
            .unwrap();

        match mem::take(&mut self.lock_state) {
            LockState::Locking(confirmation) => {
                // We're locking and an output was removed, check if the requirements are now met.
                let all_locked = self
                    .output_state
                    .values()
                    .all(|state| state.lock_render_state == LockRenderState::Locked);

                if all_locked {
                    let lock = confirmation.ext_session_lock().clone();
                    confirmation.lock();
                    self.lock_state = LockState::Locked(lock);
                } else {
                    // Still waiting.
                    self.lock_state = LockState::Locking(confirmation);
                }
            }
            lock_state => {
                self.lock_state = lock_state;
                self.maybe_continue_to_locking();
            }
        }

        if self.close_screenshot_ui() {
            self.cursor_manager
                .set_cursor_image(CursorImageStatus::default_named());
            self.queue_redraw_all();
        }

        if self.switcher.output() == Some(output) {
            self.switcher.cancel();
        }
    }

    pub fn output_resized(&mut self, output: &Output) {
        let output_size = output_size(output);
        let scale = output.current_scale();
        let transform = output.current_transform();

        {
            let mut layer_map = layer_map_for_output(output);
            for layer in layer_map.layers() {
                layer.with_surfaces(|surface, data| {
                    send_scale_transform(surface, data, scale, transform);
                });

                if let Some(mapped) = self.mapped_layer_surfaces.get_mut(layer) {
                    mapped.update_sizes(output_size, scale.fractional_scale());
                }
            }
            layer_map.arrange();
        }

        self.layout.update_output_size(output);

        // Every icon decode is keyed on its *physical* size, so a scale or resolution
        // change invalidates the whole startup warm — and the app grid then decodes each
        // icon on the frame it first draws it, which is what a first open after changing
        // the display looks like: icons arriving a few at a time. Re-warm off-thread for
        // the new geometry (the size also picks the grid's icon rung, so this is not just
        // the scale). Idempotent, and outputs resize rarely.
        self.prewarm_app_icons();
        self.warm_avatar();

        if let Some(state) = self.output_state.get_mut(output) {
            state.backdrop_buffer.resize(output_size);

            state.lock_color_buffer.resize(output_size);
            state.shield_dim_buffer.resize(output_size);
            state.shield_backstop_buffer.resize(output_size);
            if let Some(lock_surface) = &state.lock_surface {
                configure_lock_surface(lock_surface, output);
            }
        }

        // If the output size changed with an open screenshot UI, close the screenshot UI.
        if let Some((old_size, old_scale, old_transform)) = self.screenshot_ui.output_size(output) {
            let output_mode = output.current_mode().unwrap();
            let size = transform.transform_size(output_mode.size);
            let scale = output.current_scale().fractional_scale();
            // FIXME: scale changes and transform flips shouldn't matter but they currently do since
            // I haven't quite figured out how to draw the screenshot textures in
            // physical coordinates.
            if old_size != size || old_scale != scale || old_transform != transform {
                self.close_screenshot_ui();
                self.cursor_manager
                    .set_cursor_image(CursorImageStatus::default_named());
                self.queue_redraw_all();
                return;
            }
        }

        self.queue_redraw(output);
    }

    pub fn deactivate_monitors(&mut self, backend: &mut Backend) {
        if !self.monitors_active {
            return;
        }

        self.monitors_active = false;
        backend.set_monitors_active(false);
    }

    pub fn activate_monitors(&mut self, backend: &mut Backend) {
        if self.monitors_active {
            return;
        }

        self.monitors_active = true;
        backend.set_monitors_active(true);

        self.queue_redraw_all();
    }

    /// A snapshot of the overview, keyboard-focus and input-method state, for `synoik msg
    /// debug-focus-state`.
    ///
    /// Every field here had to be inferred from side effects during a 2026-08-15 wedge, where
    /// unfocusing a fullscreen client left the compositor presenting that client's last frame
    /// forever. Reading them together is one command instead of an afternoon.
    pub fn debug_focus_state(&self) -> synoik_ipc::DebugFocusState {
        let progress = self.layout.overview_progress_debug();
        let im = self.input_method.as_ref();
        let (pending, unanswered) = im.map_or(
            (0, 0),
            crate::input_method::InputMethod::pending_and_unanswered,
        );
        synoik_ipc::DebugFocusState {
            overview_open: self.layout.is_overview_open(),
            overview_progress: progress.map(|(value, _)| value),
            overview_progress_kind: progress.map(|(_, kind)| kind.to_owned()),
            render_above_top_layer: self.layout.active_monitor_renders_above_top_layer(),
            keyboard_focus: self.keyboard_focus.debug_name(),
            input_method: im.is_some(),
            im_focus: im.map_or_else(|| String::from("-"), |im| format!("{:?}", im.focus())),
            im_connected: im.is_some_and(crate::input_method::InputMethod::is_connected),
            im_client_enabled: im.is_some_and(crate::input_method::InputMethod::is_enabled),
            im_pending_keys: pending,
            im_unanswered: unanswered,
            im_unresponsive: im.is_some_and(crate::input_method::InputMethod::is_unresponsive),
            outputs: self
                .output_state
                .iter()
                .map(|(output, state)| synoik_ipc::DebugOutputState {
                    name: output.name(),
                    redraw_state: state.redraw_state.debug_name().to_owned(),
                    unfinished_animations: state.unfinished_animations_remain,
                    elements: state.last_frame_elements,
                    zero_copy: state.last_frame_scanout.zero_copy,
                    format_unsupported: state.last_frame_scanout.format_unsupported,
                    scanout_failed: state.last_frame_scanout.scanout_failed,
                    rendered: state.last_frame_scanout.rendered,
                })
                .collect(),
        }
    }

    pub fn output_under(&self, pos: Point<f64, Logical>) -> Option<(&Output, Point<f64, Logical>)> {
        let output = self.global_space.output_under(pos).next()?;
        let pos_within_output = pos
            - self
                .global_space
                .output_geometry(output)
                .unwrap()
                .loc
                .to_f64();

        Some((output, pos_within_output))
    }

    /// Whether the top panel shows on `output` — false over a fullscreen window.
    ///
    /// GNOME registers `panelBox` as chrome with `trackFullscreen: true`
    /// (`js/ui/layout.js:285`), and `_updateActorVisibility` (`:983`) sets
    /// `visible = !(global.window_group.visible && monitor.inFullscreen)`. The window group is
    /// hidden while the overview is up, so the panel comes back there even over a fullscreen
    /// window — which for us is just `is_overview_open`. (GNOME's other conjunct,
    /// `sessionMode.hasWindows`, covers the lock screen; the panel render path already returns
    /// before that.)
    ///
    /// `inFullscreen` is `render_above_top_layer`, the same predicate the hot corner uses for the
    /// same reason (`js/ui/layout.js:1247`) — one source of truth so the two cannot drift.
    ///
    /// **This gates input as well as drawing.** In Clutter `visible = false` takes the actor out
    /// of the pick, so a hidden panel cannot be hovered or clicked; a version of this that only
    /// skipped rendering would leave an invisible 40px strip eating clicks along the top of every
    /// fullscreen window.
    pub fn panel_visible_on(&self, output: &Output) -> bool {
        if self.layout.is_overview_open() {
            return true;
        }

        !self
            .layout
            .monitor_for_output(output)
            .is_some_and(|mon| mon.render_above_top_layer())
    }

    /// The workspace snapshot for `output`'s monitor, driving that panel's dot indicator.
    pub fn workspace_state_for(&self, output: &Output) -> crate::ui::panel::WorkspaceState {
        let (count, active) = self
            .layout
            .monitor_for_output(output)
            .map(|mon| (mon.n_workspaces(), mon.active_workspace_idx()))
            .unwrap_or((0, 0));
        crate::ui::panel::WorkspaceState { count, active }
    }

    /// The live (fractional) active-workspace index for `output` — gnome-shell's
    /// `WorkspacesAdjustment.value`. Equals `active` at rest and slides between indices
    /// while a workspace switch animates, driving the panel dots' expansion.
    pub fn workspace_position_for(&self, output: &Output) -> f64 {
        self.layout
            .monitor_for_output(output)
            .map(|mon| mon.workspace_render_idx())
            .unwrap_or(0.)
    }

    /// The two edge segments this output's hot corner listens on, in the output's local logical
    /// coordinates: `panel_height` px down the left edge and the same along the top
    /// (`HotCorner.setBarrierSize`, `layout.js:1195-1233`, sized from `panelBox.height`).
    ///
    /// gnome-shell gives the top-left corner of every monitor a hot corner (top-right under RTL,
    /// which we do not implement yet), skipping a non-primary monitor whose corner is *interior* —
    /// another monitor sits directly above it or to its left (`layout.js:452-490`). We skip any
    /// interior corner, the primary's included: mutter holds the pointer at an interior edge with
    /// a real barrier until it pushes through, and we have none. Our pressure is the motion the
    /// output clamp discards, and the clamp only bites at the outer edge of the global space.
    pub fn hot_corner_segments(&self, output: &Output) -> Option<[Segment; 2]> {
        if !self.gnome_settings.enable_hot_corners {
            return None;
        }

        let geom = self.global_space.output_geometry(output)?;
        let corner = geom.loc.to_f64();

        // Interior corner: some other output owns the pixel just left of, or just above, ours.
        let outside = |p: Point<f64, Logical>| self.global_space.output_under(p).next().is_none();
        if !outside(corner - Point::new(1., 0.)) || !outside(corner - Point::new(0., 1.)) {
            return None;
        }

        let size = crate::ui::panel::panel_height();
        Some([
            Segment::from_start(Edge::Left, size),
            Segment::from_start(Edge::Top, size),
        ])
    }

    /// The hot corner for an absolute pointing device: the corner, when the pointer lands on the
    /// corner pixel itself.
    ///
    /// An absolute device cannot build pressure. Its position is mapped into the output, so the
    /// clamp never discards anything and [`Self::push_hot_corner`] would never fire — a tablet or
    /// a VM's absolute pointer would simply have no hot corner. So we fall back to the corner
    /// pixel, the way this compositor triggered before pressure existed, and let the barrier's
    /// latch keep it from re-firing while the pointer rests there. Deliberately just the pixel,
    /// not the full L: without a push to qualify it, a 32 px strip of the top edge would toggle
    /// the overview every time the pointer crossed it.
    pub fn touch_hot_corner(&mut self, pos: Point<f64, Logical>) -> Option<Point<f64, Logical>> {
        let output = self
            .output_under(pos)
            .map(|(output, p)| (output.clone(), p));
        let Some((output, pos_within_output)) = output else {
            self.hot_corner_barrier.leave();
            self.hot_corner_output = None;
            return None;
        };

        let on_corner = self.hot_corner_segments(&output).is_some()
            && Rectangle::new(Point::from((0., 0.)), Size::from((1., 1.)))
                .contains(pos_within_output);
        if !on_corner {
            self.hot_corner_barrier.leave();
            return None;
        }

        if self.hot_corner_output.as_ref() != Some(&output) {
            self.hot_corner_barrier.leave();
            self.hot_corner_output = Some(output.clone());
        }

        let fullscreen = self
            .layout
            .monitor_for_output(&output)
            .is_some_and(|mon| mon.render_above_top_layer());
        if fullscreen && !self.layout.is_overview_open() {
            return None;
        }

        self.hot_corner_barrier
            .shove(self.clock.now_unadjusted())
            .then(|| self.corner_of(&output))
    }

    /// Feed one motion to the dock's bottom-edge barrier, sliding it out if it trips.
    ///
    /// Same pressure as the hot corner's — the motion the output clamp discarded. The dock
    /// belongs to the output the pointer is on, so pushing the bottom edge of one monitor never
    /// opens it on another.
    pub fn push_dock(
        &mut self,
        pos: Point<f64, Logical>,
        unclamped: Point<f64, Logical>,
        delta: Point<f64, Logical>,
        time: Duration,
    ) {
        // Locked or behind the screenshot UI, the dash is not on screen and must not be
        // summonable — `dash_area` refuses to place it, so a dock shown here would be an
        // invisible click-eater.
        if self.is_locked() || self.screenshot_ui.is_open() {
            return;
        }

        let output = self
            .output_under(pos)
            .map(|(output, p)| (output.clone(), p));
        let Some((output, pos_within_output)) = output else {
            return;
        };

        let discarded = unclamped - pos;
        if self
            .dock
            .push(&output, pos_within_output, discarded, delta, time)
        {
            self.dock.show(&output);
            self.queue_redraw_all();
        }
    }

    /// Track the pointer for the dock's auto-hide.
    pub fn dock_pointer_motion(&mut self, pos: Point<f64, Logical>) {
        if !self.dock.is_visible() {
            return;
        }
        match self
            .output_under(pos)
            .map(|(output, p)| (output.clone(), p))
        {
            Some((output, pos_within_output)) => {
                self.dock.pointer_motion(&output, pos_within_output);
            }
            None => self.dock.pointer_left(),
        }
    }

    /// The global position of an output's hot corner.
    fn corner_of(&self, output: &Output) -> Point<f64, Logical> {
        self.global_space
            .output_geometry(output)
            .map(|geom| geom.loc.to_f64())
            .unwrap_or_default()
    }

    /// Feed one motion to the hot-corner barrier; `Some(corner)` means it just tripped.
    ///
    /// `pos` is the pointer's position after clamping, `unclamped` where the motion wanted to take
    /// it, and `delta` the raw motion. The difference between the first two is the pressure: see
    /// [`crate::input::pressure`].
    pub fn push_hot_corner(
        &mut self,
        pos: Point<f64, Logical>,
        unclamped: Point<f64, Logical>,
        delta: Point<f64, Logical>,
        time: Duration,
    ) -> Option<Point<f64, Logical>> {
        let output = self
            .output_under(pos)
            .map(|(output, p)| (output.clone(), p));
        let Some((output, pos_within_output)) = output else {
            self.hot_corner_barrier.leave();
            self.hot_corner_output = None;
            return None;
        };

        let Some(segments) = self.hot_corner_segments(&output) else {
            self.hot_corner_barrier.leave();
            self.hot_corner_output = None;
            return None;
        };

        // Pressure built against one monitor's corner doesn't carry to another's.
        if self.hot_corner_output.as_ref() != Some(&output) {
            self.hot_corner_barrier.leave();
            self.hot_corner_output = Some(output.clone());
        }

        // A fullscreen window owns the corner, unless the overview is already up
        // (`HotCorner._toggleOverview`, `layout.js:1249-1251`).
        let fullscreen = self
            .layout
            .monitor_for_output(&output)
            .is_some_and(|mon| mon.render_above_top_layer());
        if fullscreen && !self.layout.is_overview_open() {
            self.hot_corner_barrier.leave();
            return None;
        }

        let size = output_size(&output);
        let discarded = unclamped - pos;

        let mut triggered = false;
        let mut engaged = false;
        for segment in segments {
            if segment.contains(size, pos_within_output) {
                engaged = true;
            }
            if let Some(push) = segment.push(size, pos_within_output, discarded, delta, time) {
                triggered |= self.hot_corner_barrier.push(push);
            }
        }

        // Leaving the L re-arms the corner; resting on it, having triggered, does not.
        if !engaged {
            self.hot_corner_barrier.leave();
        }

        triggered.then(|| self.corner_of(&output))
    }

    pub fn is_sticky_obscured_under(
        &self,
        output: &Output,
        pos_within_output: Point<f64, Logical>,
    ) -> bool {
        // The ordering here must be consistent with the ordering in render() so that input is
        // consistent with the visuals.

        // Check if some layer-shell surface is on top.
        let layers = layer_map_for_output(output);
        let layer_surface_under = |layer, popup| {
            layers
                .layers_on(layer)
                .rev()
                .find_map(|layer| {
                    let mapped = self.mapped_layer_surfaces.get(layer)?;

                    let mut layer_pos_within_output =
                        layers.layer_geometry(layer).unwrap().loc.to_f64();
                    layer_pos_within_output += mapped.bob_offset();

                    let surface_type = if popup {
                        WindowSurfaceType::POPUP
                    } else {
                        WindowSurfaceType::TOPLEVEL
                    } | WindowSurfaceType::SUBSURFACE;
                    layer.surface_under(pos_within_output - layer_pos_within_output, surface_type)
                })
                .is_some()
        };

        let layer_toplevel_under = |layer| layer_surface_under(layer, false);
        let layer_popup_under = |layer| layer_surface_under(layer, true);

        if layer_popup_under(Layer::Overlay) || layer_toplevel_under(Layer::Overlay) {
            return true;
        }

        let mon = self.layout.monitor_for_output(output).unwrap();
        if mon.render_above_top_layer() {
            return false;
        }

        if layer_popup_under(Layer::Top) || layer_toplevel_under(Layer::Top) {
            return true;
        }

        false
    }

    pub fn is_layout_obscured_under(
        &self,
        output: &Output,
        pos_within_output: Point<f64, Logical>,
    ) -> bool {
        if self.layout.is_overview_open() {
            return false;
        }

        // Check if some layer-shell surface is on top.
        let layers = layer_map_for_output(output);
        let layer_popup_under = |layer| {
            layers
                .layers_on(layer)
                .rev()
                .find_map(|layer_surface| {
                    let mapped = self.mapped_layer_surfaces.get(layer_surface)?;
                    if mapped.place_within_backdrop() {
                        return None;
                    }

                    let mut layer_pos_within_output =
                        layers.layer_geometry(layer_surface).unwrap().loc.to_f64();
                    layer_pos_within_output += mapped.bob_offset();

                    // Background and bottom layers move together with the workspaces.
                    let mon = self.layout.monitor_for_output(output)?;
                    let (_, geo) = mon.workspace_under(pos_within_output)?;
                    layer_pos_within_output += geo.loc;

                    let surface_type = WindowSurfaceType::POPUP | WindowSurfaceType::SUBSURFACE;
                    layer_surface
                        .surface_under(pos_within_output - layer_pos_within_output, surface_type)
                })
                .is_some()
        };

        if layer_popup_under(Layer::Bottom) || layer_popup_under(Layer::Background) {
            return true;
        }

        false
    }

    /// Returns the workspace under the position to be activated.
    ///
    /// The return value is an output and a workspace index on it.
    pub fn workspace_under(
        &self,
        extended_bounds: bool,
        pos: Point<f64, Logical>,
    ) -> Option<(Output, &Workspace<Mapped>)> {
        if self.exit_confirm_dialog.is_open()
            || self.run_dialog.is_open()
            || self.end_session_dialog.is_open()
            || self.polkit_is_open()
            || self.is_locked()
            || self.screenshot_ui.is_open()
        {
            return None;
        }

        let (output, pos_within_output) = self.output_under(pos)?;

        if self.is_sticky_obscured_under(output, pos_within_output) {
            return None;
        }

        if self.is_layout_obscured_under(output, pos_within_output) {
            return None;
        }

        let ws = self
            .layout
            .workspace_under(extended_bounds, output, pos_within_output)?;
        Some((output.clone(), ws))
    }

    pub fn workspace_under_cursor(
        &self,
        extended_bounds: bool,
    ) -> Option<(Output, &Workspace<Mapped>)> {
        let pos = self.seat.get_pointer().unwrap().current_location();
        self.workspace_under(extended_bounds, pos)
    }

    /// The workspace whose overview strip thumbnail is under the cursor.
    pub fn thumbnail_workspace_under_cursor(&self) -> Option<(Output, &Workspace<Mapped>)> {
        let pos = self.seat.get_pointer().unwrap().current_location();
        self.thumbnail_workspace_under(pos)
    }

    /// The overview strip thumbnail under the position: its output, the position in that
    /// output's coordinates, and the index of the workspace it stands for.
    pub fn thumbnail_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(Output, Point<f64, Logical>, usize)> {
        let (output, pos_within_output) = self.thumbnail_strip_under(pos)?;
        let idx = self
            .layout
            .monitor_for_output(&output)?
            .thumbnail_strip()?
            .thumb_under(pos_within_output)?;
        Some((output, pos_within_output, idx))
    }

    /// The workspace whose thumbnail close button is under the position — tested *before*
    /// the thumbnail itself, since the button sits inside its thumbnail's body.
    pub fn thumbnail_close_under(&self, pos: Point<f64, Logical>) -> Option<WorkspaceId> {
        let (output, pos_within_output) = self.thumbnail_strip_under(pos)?;
        self.layout
            .thumbnail_close_under(&output, pos_within_output)
    }

    /// The workspace whose overview strip thumbnail is under the position.
    pub fn thumbnail_workspace_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(Output, &Workspace<Mapped>)> {
        let (output, pos_within_output) = self.thumbnail_strip_under(pos)?;
        let ws = self
            .layout
            .thumbnail_workspace_under(&output, pos_within_output)?;
        Some((output, ws))
    }

    /// The output whose strip is reactive at this position, and the position in its
    /// coordinates — the gating both strip hit tests share.
    fn thumbnail_strip_under(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(Output, Point<f64, Logical>)> {
        if self.exit_confirm_dialog.is_open()
            || self.run_dialog.is_open()
            || self.end_session_dialog.is_open()
            || self.polkit_is_open()
            || self.is_locked()
            || self.screenshot_ui.is_open()
            // Faded out under the search results: gnome-shell drops the strip's
            // reactivity alongside the picker's (`overviewControls.js:550-580`).
            || self.overview_search.is_active()
        {
            return None;
        }

        let (output, pos_within_output) = self.output_under(pos)?;

        if self.is_sticky_obscured_under(output, pos_within_output) {
            return None;
        }
        if self.is_layout_obscured_under(output, pos_within_output) {
            return None;
        }

        Some((output.clone(), pos_within_output))
    }

    /// Returns the window under the position to be activated.
    ///
    /// Whether anything is covering the screen — our own shield, or an `ext-session-lock` client.
    ///
    /// GNOME's equivalent is `Main.sessionMode.isLocked`. [`Self::is_locked`] is **not** it: that
    /// is only the Wayland lock protocol's state, so a screensaver-only shield (`lock-enabled =
    /// false`) reads as unlocked while still covering everything. Anything that must not draw over
    /// a covered screen wants this one.
    pub fn screen_is_covered(&self) -> bool {
        self.screen_shield.is_active() || self.is_locked()
    }

    /// Whether the polkit dialog is on screen. Always false without `dbus`, where there is no
    /// agent to raise one.
    pub fn polkit_is_open(&self) -> bool {
        {
            self.polkit_ui.is_open()
        }
    }

    /// The cursor may be inside the window's activation region, but not within the window's input
    /// region.
    pub fn window_under(&self, pos: Point<f64, Logical>) -> Option<&Mapped> {
        if self.exit_confirm_dialog.is_open()
            || self.run_dialog.is_open()
            || self.end_session_dialog.is_open()
            || self.polkit_is_open()
            || self.is_locked()
            || self.screenshot_ui.is_open()
            // The window picker is faded out under the search results, so its
            // previews must not activate — gnome-shell drops `reactive` on the
            // workspaces display while searching (`overviewControls.js:636-641`).
            || (self.layout.is_overview_open() && self.overview_search.is_active())
        {
            return None;
        }

        let (output, pos_within_output) = self.output_under(pos)?;

        if self.is_sticky_obscured_under(output, pos_within_output) {
            return None;
        }

        if let Some((window, _loc)) = self
            .layout
            .interactive_moved_window_under(output, pos_within_output)
        {
            return Some(window);
        }

        if self.is_layout_obscured_under(output, pos_within_output) {
            return None;
        }

        let (window, _loc) = self.layout.window_under(output, pos_within_output)?;
        Some(window)
    }

    /// Returns the window under the cursor to be activated.
    ///
    /// The cursor may be inside the window's activation region, but not within the window's input
    /// region.
    pub fn window_under_cursor(&self) -> Option<&Mapped> {
        let pos = self.seat.get_pointer().unwrap().current_location();
        self.window_under(pos)
    }

    /// Returns contents under the given point.
    ///
    /// We don't have a proper global space for all windows, so this function converts window
    /// locations to global space according to where they are rendered.
    ///
    /// This function does not take pointer or touch grabs into account.
    pub fn contents_under(&self, pos: Point<f64, Logical>) -> PointContents {
        let mut rv = PointContents::default();

        let Some((output, pos_within_output)) = self.output_under(pos) else {
            return rv;
        };
        rv.output = Some(output.clone());
        let output_pos_in_global_space = self.global_space.output_geometry(output).unwrap().loc;

        // The ordering here must be consistent with the ordering in render() so that input is
        // consistent with the visuals.

        if self.exit_confirm_dialog.is_open()
            || self.run_dialog.is_open()
            || self.end_session_dialog.is_open()
            || self.polkit_is_open()
        {
            return rv;
        } else if self.is_locked() {
            let Some(state) = self.output_state.get(output) else {
                return rv;
            };
            let Some(surface) = state.lock_surface.as_ref() else {
                return rv;
            };

            rv.surface = under_from_surface_tree(
                surface.wl_surface(),
                pos_within_output,
                // We put lock surfaces at (0, 0).
                (0, 0),
                WindowSurfaceType::ALL,
            )
            .map(|(surface, pos_within_output)| {
                (
                    surface,
                    (pos_within_output + output_pos_in_global_space).to_f64(),
                )
            });

            return rv;
        }

        if self.screenshot_ui.is_open()
            || self.panel_popover.grabs_input()
            || self.switcher.is_open()
        {
            // These grab input modally (clicks and motion): while one is open no window under it
            // should receive pointer focus, so the app can't keep driving the cursor image (e.g. a
            // maximized terminal's I-beam over the clock popover).
            //
            // The switcher's grab starts with the popup, not with its *drawing*: `pushModal` is
            // the first thing `show` does and the [`POPUP_DELAY`] that follows only sets opacity
            // (`switcherPopup.js:122-168`), so this is `is_open`, not `is_visible`.
            //
            // [`POPUP_DELAY`]: crate::ui::switcher::POPUP_DELAY
            return rv;
        }

        // A visible notification banner sits above the windows and takes the pointer: while it's
        // under the cursor, suppress the window beneath it so the app can't keep pointer focus and
        // paint its own cursor (e.g. an I-beam) over the banner. Its own clicks/hover are handled
        // separately (`NotificationBanner::hit_test`), and it is blocked while a popover is open.
        if self
            .notification_banner
            .pointer_inside(output, pos_within_output)
        {
            return rv;
        }

        // While the overview is open, the dash sits above the zoomed workspaces and
        // takes the pointer — suppress the window beneath it (same reason as the
        // banner). Its own clicks/hover are handled in the input path.
        if self.layout.is_overview_open()
            && self
                .layout
                .controls_layout_for_output(output)
                .and_then(|l| self.dash.hit_test(pos_within_output, l.dash))
                .is_some()
        {
            return rv;
        }

        let layers = layer_map_for_output(output);
        let layer_surface_under = |layer, popup| {
            layers
                .layers_on(layer)
                .rev()
                .find_map(|layer_surface| {
                    let mapped = self.mapped_layer_surfaces.get(layer_surface)?;
                    if mapped.place_within_backdrop() {
                        return None;
                    }

                    let mut layer_pos_within_output =
                        layers.layer_geometry(layer_surface).unwrap().loc.to_f64();
                    layer_pos_within_output += mapped.bob_offset();

                    // Background and bottom layers move together with the workspaces.
                    if matches!(layer, Layer::Background | Layer::Bottom) {
                        let mon = self.layout.monitor_for_output(output)?;
                        let (_, geo) = mon.workspace_under(pos_within_output)?;
                        layer_pos_within_output += geo.loc;
                        // Don't need to deal with zoom here because in the overview background and
                        // bottom layers don't receive input.
                    }

                    let surface_type = if popup {
                        WindowSurfaceType::POPUP
                    } else {
                        WindowSurfaceType::TOPLEVEL
                    } | WindowSurfaceType::SUBSURFACE;

                    layer_surface
                        .surface_under(pos_within_output - layer_pos_within_output, surface_type)
                        .map(|(surface, pos_within_layer)| {
                            (
                                (surface, pos_within_layer.to_f64() + layer_pos_within_output),
                                layer_surface,
                            )
                        })
                })
                .map(|(s, l)| (Some(s), (None, Some(l.clone()))))
        };

        let layer_toplevel_under = |layer| layer_surface_under(layer, false);
        let layer_popup_under = |layer| layer_surface_under(layer, true);

        let mapped_hit_data = |(mapped, hit): (&Mapped, HitType)| {
            let window = &mapped.window;
            let surface_and_pos = if let HitType::Input { win_pos } = hit {
                let win_pos_within_output = win_pos;
                window
                    .surface_under(
                        pos_within_output - win_pos_within_output,
                        WindowSurfaceType::ALL,
                    )
                    .map(|(s, pos_within_window)| {
                        (s, pos_within_window.to_f64() + win_pos_within_output)
                    })
            } else {
                None
            };
            (surface_and_pos, (Some((window.clone(), hit)), None))
        };

        let interactive_moved_window_under = || {
            self.layout
                .interactive_moved_window_under(output, pos_within_output)
                .map(mapped_hit_data)
        };
        let window_under = || {
            self.layout
                .window_under(output, pos_within_output)
                .map(mapped_hit_data)
        };

        let mon = self.layout.monitor_for_output(output).unwrap();

        let mut under =
            layer_popup_under(Layer::Overlay).or_else(|| layer_toplevel_under(Layer::Overlay));

        let is_overview_open = self.layout.is_overview_open();

        // When rendering above the top layer, we put the regular monitor elements first.
        // Otherwise, we will render all layer-shell pop-ups and the top layer on top.
        if mon.render_above_top_layer() {
            under = under
                .or_else(interactive_moved_window_under)
                .or_else(window_under)
                .or_else(|| layer_popup_under(Layer::Top))
                .or_else(|| layer_toplevel_under(Layer::Top))
                .or_else(|| layer_popup_under(Layer::Bottom))
                .or_else(|| layer_popup_under(Layer::Background))
                .or_else(|| layer_toplevel_under(Layer::Bottom))
                .or_else(|| layer_toplevel_under(Layer::Background));
        } else {
            under = under
                .or_else(|| layer_popup_under(Layer::Top))
                .or_else(|| layer_toplevel_under(Layer::Top));

            under = under.or_else(interactive_moved_window_under);

            if !is_overview_open {
                under = under
                    .or_else(|| layer_popup_under(Layer::Bottom))
                    .or_else(|| layer_popup_under(Layer::Background));
            }

            under = under.or_else(window_under);

            if !is_overview_open {
                under = under
                    .or_else(|| layer_toplevel_under(Layer::Bottom))
                    .or_else(|| layer_toplevel_under(Layer::Background));
            }
        }

        let Some((mut surface_and_pos, (window, layer))) = under else {
            return rv;
        };

        if let Some((_, surface_pos)) = &mut surface_and_pos {
            *surface_pos += output_pos_in_global_space.to_f64();
        }

        rv.surface = surface_and_pos;
        rv.window = window;
        rv.layer = layer;
        rv
    }

    pub fn output_under_cursor(&self) -> Option<Output> {
        let pos = self.seat.get_pointer().unwrap().current_location();
        self.global_space.output_under(pos).next().cloned()
    }

    pub fn output_left_of(&self, current: &Output) -> Option<Output> {
        let current_geo = self.global_space.output_geometry(current)?;
        let extended_geo = Rectangle::new(
            Point::from((i32::MIN / 2, current_geo.loc.y)),
            Size::from((i32::MAX, current_geo.size.h)),
        );

        self.global_space
            .outputs()
            .map(|output| (output, self.global_space.output_geometry(output).unwrap()))
            .filter(|(_, geo)| center(*geo).x < center(current_geo).x && geo.overlaps(extended_geo))
            .min_by_key(|(_, geo)| center(current_geo).x - center(*geo).x)
            .map(|(output, _)| output)
            .cloned()
    }

    pub fn output_right_of(&self, current: &Output) -> Option<Output> {
        let current_geo = self.global_space.output_geometry(current)?;
        let extended_geo = Rectangle::new(
            Point::from((i32::MIN / 2, current_geo.loc.y)),
            Size::from((i32::MAX, current_geo.size.h)),
        );

        self.global_space
            .outputs()
            .map(|output| (output, self.global_space.output_geometry(output).unwrap()))
            .filter(|(_, geo)| center(*geo).x > center(current_geo).x && geo.overlaps(extended_geo))
            .min_by_key(|(_, geo)| center(*geo).x - center(current_geo).x)
            .map(|(output, _)| output)
            .cloned()
    }

    pub fn output_up_of(&self, current: &Output) -> Option<Output> {
        let current_geo = self.global_space.output_geometry(current)?;
        let extended_geo = Rectangle::new(
            Point::from((current_geo.loc.x, i32::MIN / 2)),
            Size::from((current_geo.size.w, i32::MAX)),
        );

        self.global_space
            .outputs()
            .map(|output| (output, self.global_space.output_geometry(output).unwrap()))
            .filter(|(_, geo)| center(*geo).y < center(current_geo).y && geo.overlaps(extended_geo))
            .min_by_key(|(_, geo)| center(current_geo).y - center(*geo).y)
            .map(|(output, _)| output)
            .cloned()
    }

    pub fn output_down_of(&self, current: &Output) -> Option<Output> {
        let current_geo = self.global_space.output_geometry(current)?;
        let extended_geo = Rectangle::new(
            Point::from((current_geo.loc.x, i32::MIN / 2)),
            Size::from((current_geo.size.w, i32::MAX)),
        );

        self.global_space
            .outputs()
            .map(|output| (output, self.global_space.output_geometry(output).unwrap()))
            .filter(|(_, geo)| center(*geo).y > center(current_geo).y && geo.overlaps(extended_geo))
            .min_by_key(|(_, geo)| center(*geo).y - center(current_geo).y)
            .map(|(output, _)| output)
            .cloned()
    }

    pub fn output_previous_of(&self, current: &Output) -> Option<Output> {
        self.sorted_outputs
            .iter()
            .rev()
            .skip_while(|&output| output != current)
            .nth(1)
            .or(self.sorted_outputs.last())
            .filter(|&output| output != current)
            .cloned()
    }

    pub fn output_next_of(&self, current: &Output) -> Option<Output> {
        self.sorted_outputs
            .iter()
            .skip_while(|&output| output != current)
            .nth(1)
            .or(self.sorted_outputs.first())
            .filter(|&output| output != current)
            .cloned()
    }

    pub fn output_left(&self) -> Option<Output> {
        let active = self.layout.active_output()?;
        self.output_left_of(active)
    }

    pub fn output_right(&self) -> Option<Output> {
        let active = self.layout.active_output()?;
        self.output_right_of(active)
    }

    pub fn output_up(&self) -> Option<Output> {
        let active = self.layout.active_output()?;
        self.output_up_of(active)
    }

    pub fn output_down(&self) -> Option<Output> {
        let active = self.layout.active_output()?;
        self.output_down_of(active)
    }

    pub fn output_previous(&self) -> Option<Output> {
        let active = self.layout.active_output()?;
        self.output_previous_of(active)
    }

    pub fn output_next(&self) -> Option<Output> {
        let active = self.layout.active_output()?;
        self.output_next_of(active)
    }

    pub fn find_output_and_workspace_index(
        &self,
        workspace_reference: WorkspaceReference,
    ) -> Option<(Option<Output>, usize)> {
        let (target_workspace_index, target_workspace) = match workspace_reference {
            WorkspaceReference::Index(index) => {
                return Some((None, index.saturating_sub(1) as usize));
            }
            WorkspaceReference::Name(name) => self.layout.find_workspace_by_name(&name)?,
            WorkspaceReference::Id(id) => {
                let id = WorkspaceId::specific(id);
                self.layout.find_workspace_by_id(id)?
            }
        };

        let target_output = target_workspace.current_output();
        Some((target_output.cloned(), target_workspace_index))
    }

    pub fn find_window_by_id(&self, id: MappedId) -> Option<Window> {
        self.layout
            .windows()
            .find(|(_, m)| m.id() == id)
            .map(|(_, m)| m.window.clone())
    }

    pub fn output_for_tablet(&self) -> Option<&Output> {
        let config = self.config.borrow();
        if config.input.tablet.map_to_focused_output {
            self.layout.active_output()
        } else {
            let map_to_output = config.input.tablet.map_to_output.as_ref();
            map_to_output.and_then(|name| self.output_by_name_match(name))
        }
    }

    pub fn output_for_touch(&self) -> Option<&Output> {
        let config = self.config.borrow();
        let map_to_output = config.input.touch.map_to_output.as_ref();
        map_to_output
            .and_then(|name| self.output_by_name_match(name))
            .or_else(|| self.global_space.outputs().next())
    }

    pub fn output_by_name_match(&self, target: &str) -> Option<&Output> {
        self.global_space
            .outputs()
            .find(|output| output_matches_name(output, target))
    }

    pub fn output_for_root(&self, root: &WlSurface) -> Option<&Output> {
        // Check the main layout.
        let win_out = self.layout.find_window_and_output(root);
        let layout_output = win_out.map(|(_, output)| output);
        if let Some(output) = layout_output {
            return output;
        }

        // Check layer-shell.
        let has_layer_surface = |o: &&Output| {
            layer_map_for_output(o)
                .layer_for_surface(root, WindowSurfaceType::TOPLEVEL)
                .is_some()
        };
        self.layout.outputs().find(has_layer_surface)
    }

    pub fn lock_surface_focus(&self) -> Option<WlSurface> {
        let output_under_cursor = self.output_under_cursor();
        let output = output_under_cursor
            .as_ref()
            .or_else(|| self.layout.active_output())
            .or_else(|| self.global_space.outputs().next())?;

        let state = self.output_state.get(output)?;
        state.lock_surface.as_ref().map(|s| s.wl_surface()).cloned()
    }

    /// Schedules an immediate redraw on all outputs if one is not already scheduled.
    /// Recompute everything derived from the two keybinding sources: the modifier
    /// fast-path sets the pointer, wheel, touchpad and stylus handlers gate on, and
    /// the hotkey overlay's baked content.
    ///
    /// Both sources feed them — the config binds and the GSettings keybinding
    /// model — so this runs whenever either changes. A stale set does not make a
    /// binding slow, it makes it *invisible*: the handlers never look it up, and the
    /// overlay never re-bakes.
    pub fn refresh_keybinding_state(&mut self) {
        let keybindings = &self.gnome_settings.keybindings;
        self.mods_with_mouse_binds = mods_with_mouse_binds(keybindings);
        self.mods_with_wheel_binds = mods_with_wheel_binds(keybindings);
        self.mods_with_tablet_stylus_binds = mods_with_tablet_stylus_binds(keybindings);
        self.mods_with_finger_scroll_binds = mods_with_finger_scroll_binds(keybindings);
        self.hotkey_overlay.set_keybindings(keybindings);
    }

    /// Take ownership of the clipboard, offering `mime_types` for `bytes`.
    ///
    /// The only way the compositor should set the selection: it keeps
    /// [`Self::clipboard_mime_types`] in step, which is what a later paste consults to decide
    /// whether there is any *text* to paste.
    pub fn set_clipboard(&mut self, mime_types: Vec<String>, bytes: Arc<[u8]>) {
        set_data_device_selection(&self.display_handle, &self.seat, mime_types.clone(), bytes);
        self.clipboard_mime_types = mime_types;
    }

    /// Which appearance the shell's own plates are drawn in — `org.gnome.desktop.interface
    /// color-scheme`, the same key the quick-settings Dark Style tile writes.
    ///
    /// See [`crate::ui::widget::style::Appearance`] for why this is the *only* thing in the
    /// shell that follows it.
    pub fn appearance(&self) -> crate::ui::widget::style::Appearance {
        crate::ui::widget::style::Appearance::from_dark_style(
            self.gnome_settings.quick_toggles.dark_style,
        )
    }

    pub fn queue_redraw_all(&mut self) {
        for state in self.output_state.values_mut() {
            state.redraw_state = mem::take(&mut state.redraw_state).queue_redraw();
        }
    }

    /// Schedules an immediate redraw if one is not already scheduled.
    pub fn queue_redraw(&mut self, output: &Output) {
        let state = self.output_state.get_mut(output).unwrap();
        state.redraw_state = mem::take(&mut state.redraw_state).queue_redraw();
    }

    pub fn redraw_queued_outputs(&mut self, backend: &mut Backend) {
        let _span = tracy_client::span!("Synoik::redraw_queued_outputs");

        while let Some((output, _)) = self.output_state.iter().find(|(_, state)| {
            matches!(
                state.redraw_state,
                RedrawState::Queued | RedrawState::WaitingForEstimatedVBlankAndQueued(_)
            )
        }) {
            let output = output.clone();
            let state = self.output_state.get_mut(&output).unwrap();

            // A deadline that already fired brought its aim with it.
            if let Some(aim) = state.pending_aim.take() {
                trace!("redrawing output at its dispatch deadline");
                self.redraw(backend, &output, aim);
                continue;
            }

            // Only a plain `Queued` can be held: the estimated-vblank states are already holding a
            // timer of their own, and they are the states where nothing was submitted anyway, so
            // there is no vblank to be early for.
            let dispatch = match state.redraw_state {
                RedrawState::Queued => state.frame_clock.next_dispatch(),
                _ => Dispatch::Now {
                    target: state.frame_clock.next_presentation_time(),
                },
            };

            match dispatch {
                Dispatch::At { at, target } => {
                    trace!("holding redraw until its dispatch deadline");
                    self.schedule_dispatch(&output, at, target);
                }
                Dispatch::Now { target } => {
                    trace!("redrawing output");
                    self.redraw(backend, &output, FrameAim::immediate(target));
                }
            }
        }
    }

    /// Hold this output's redraw until `at`, then run it aimed at `target`.
    ///
    /// The timer only flips the state back to `Queued`; the redraw itself happens in the
    /// end-of-turn drain, the same way [`Tty::on_estimated_vblank_timer`] hands an output back.
    /// It is a no-op unless the output is still holding *this* deadline, so a token dropped
    /// without being removed (the rogue-vblank error path in `Tty::on_vblank`) costs one wasted
    /// wakeup and nothing else.
    fn schedule_dispatch(&mut self, output: &Output, at: Duration, target: Duration) {
        let now = get_monotonic_time();
        let timer_output = output.clone();
        let token = self
            .event_loop
            .insert_source(
                Timer::from_duration(at.saturating_sub(now)),
                move |_, _, data| {
                    let synoik = &mut data.synoik;
                    let Some(state) = synoik.output_state.get_mut(&timer_output) else {
                        // The output went away while the deadline was pending.
                        return TimeoutAction::Drop;
                    };

                    // Only *this* deadline may release the output: a token dropped without being
                    // removed (the rogue-vblank error path in `Tty::on_vblank`) would otherwise
                    // fire into whatever deadline was armed after it and release that one early.
                    if let RedrawState::ScheduledDispatch { aim, .. } = state.redraw_state {
                        if aim.scheduled_at == Some(at) {
                            state.pending_aim = Some(aim);
                            state.redraw_state = RedrawState::Queued;
                        }
                    }

                    TimeoutAction::Drop
                },
            )
            .unwrap();

        let state = self.output_state.get_mut(output).unwrap();
        state.redraw_state = RedrawState::ScheduledDispatch {
            token,
            aim: FrameAim {
                target,
                scheduled_at: Some(at),
            },
        };
    }

    /// Hotspot of the current cursor image for `output`, in physical pixels.
    ///
    /// Matches the offset `render_pointer` bakes into the cursor element position, so it can be
    /// handed to `DrmCompositor::set_cursor_hotspot`: on para-virtualized drivers (virtio-gpu) the
    /// hotspot is written to the cursor plane's HOTSPOT_X/Y properties so the host composites our
    /// cursor plane as its pointer instead of drawing a second host cursor. Returns `(0, 0)` when
    /// the cursor is hidden — the value is unused then, as no cursor plane is assigned.
    ///
    /// Reported in the cursor image's own (unrotated) space; `DrmCompositor` rotates it into the
    /// cursor plane buffer for the output transform, so this stays transform-agnostic.
    pub fn cursor_plane_hotspot(&self, output: &Output) -> Point<i32, Physical> {
        let output_scale = Scale::from(output.current_scale().fractional_scale());
        let cursor_scale = output.current_scale().integer_scale();
        match self.cursor_manager.get_render_cursor(cursor_scale) {
            RenderCursor::Hidden => Point::default(),
            RenderCursor::Surface { hotspot, .. } => {
                hotspot.to_physical_precise_round(output_scale)
            }
            RenderCursor::Named { scale, cursor, .. } => {
                let (_, frame) = cursor.frame(self.start_time.elapsed().as_millis() as u32);
                XCursor::hotspot(frame)
                    .to_logical(scale)
                    .to_physical_precise_round(output_scale)
            }
        }
    }

    pub fn render_pointer(
        &self,
        renderer: &mut VulkanRenderer,
        output: &Output,
        push: &mut dyn FnMut(PointerRenderElements),
    ) {
        let _span = tracy_client::span!("Synoik::render_pointer");
        let output_scale = output.current_scale();
        let output_pos = self.global_space.output_geometry(output).unwrap().loc;

        // Check whether we need to draw the tablet cursor or the regular cursor.
        let pointer_pos = self
            .tablet_cursor_location
            .unwrap_or_else(|| self.seat.get_pointer().unwrap().current_location());
        let pointer_pos = pointer_pos - output_pos.to_f64();

        // Get the render cursor to draw.
        let cursor_scale = output_scale.integer_scale();
        let render_cursor = self.cursor_manager.get_render_cursor(cursor_scale);

        let output_scale = Scale::from(output.current_scale().fractional_scale());

        match render_cursor {
            RenderCursor::Hidden => (),
            RenderCursor::Surface { surface, hotspot } => {
                let pointer_pos =
                    (pointer_pos - hotspot.to_f64()).to_physical_precise_round(output_scale);

                push_elements_from_surface_tree(
                    renderer,
                    &surface,
                    pointer_pos,
                    output_scale,
                    1.,
                    Kind::Cursor,
                    &mut |elem| push(elem.into()),
                );
            }
            RenderCursor::Named {
                icon,
                scale,
                cursor,
            } => {
                let (idx, frame) = cursor.frame(self.start_time.elapsed().as_millis() as u32);
                let hotspot = XCursor::hotspot(frame).to_logical(scale);
                let pointer_pos =
                    (pointer_pos - hotspot.to_f64()).to_physical_precise_round(output_scale);

                let texture = self.cursor_texture_cache.get(icon, scale, &cursor, idx);
                match MemoryRenderBufferRenderElement::from_buffer(
                    renderer,
                    pointer_pos,
                    &texture,
                    None,
                    None,
                    None,
                    Kind::Cursor,
                ) {
                    Ok(element) => push(element.into()),
                    Err(err) => {
                        warn!("error importing a cursor texture: {err:?}");
                    }
                }
            }
        }

        if let Some(dnd_icon) = self.dnd_icon.as_ref() {
            let pointer_pos =
                (pointer_pos + dnd_icon.offset.to_f64()).to_physical_precise_round(output_scale);
            push_elements_from_surface_tree(
                renderer,
                &dnd_icon.surface,
                pointer_pos,
                output_scale,
                1.,
                Kind::ScanoutCandidate,
                &mut |elem| push(elem.into()),
            );
        }
    }

    /// Checks if the pointer should be included on a window cast or screenshot.
    ///
    /// Returns `(cursor_global_pos, win_pos)` if the pointer should be included, or `None`
    /// otherwise.
    pub fn pointer_pos_for_window_cast(
        &self,
        mapped: &Mapped,
    ) -> Option<(Point<f64, Logical>, Point<f64, Logical>)> {
        // Tablet cursor.
        if let Some(tablet_pos) = self.tablet_cursor_location {
            let contents = self.contents_under(tablet_pos);
            if let Some((w, HitType::Input { win_pos })) = contents.window {
                if w == mapped.window {
                    // Tablet tools don't currently expose current focus, and don't currently
                    // have grabs. When those are implemented, this branch should be adjusted
                    // to look more similar to the branch below.
                    return Some((tablet_pos, win_pos));
                }
            }
        }
        // Regular cursor.
        else if let Some((w, HitType::Input { win_pos })) = &self.pointer_contents.window {
            if w == &mapped.window {
                // Grabs can modify the pointer focus, making it different from
                // pointer_contents. Notably, gestures like a Mod+MMB resize will remove the pointer
                // focus, and ClickGrab will keep pointer focus on the clicked window even
                // while it's moving over a different window.
                //
                // So, double-check that current_focus() (after grabs) also matches the pointer
                // contents.
                let pointer = self.seat.get_pointer().unwrap();

                // The DnD grab is a bit special because it has its own focus (data device)
                // while the pointer focus is cleared. That focus is not currently exposed from
                // Smithay, and showing DnD icons on window screenshots seems useful, so let's
                // just allow it during DnD grabs.
                let is_dnd_grab = pointer
                    .with_grab(|_, grab| State::is_dnd_grab(grab.as_any()))
                    .unwrap_or(false);

                let current_focus_matches = is_dnd_grab
                    || pointer
                        .current_focus()
                        .map(|focused| self.find_root_shell_surface(&focused))
                        .is_some_and(|focused| mapped.is_wl_surface(&focused));
                if current_focus_matches {
                    // We don't check for pointer visibility because it can only be Visible or
                    // Hidden, and never Disabled (then it wouldn't have focus). Even when the
                    // pointer is Hidden, we want to render it, since the user explicitly
                    // requested show_pointer = true, and otherwise there's no easy way to
                    // screenshot a window with pointer with hide-when-typing because pressing
                    // the screenshot bind will hide the pointer.
                    return Some((pointer.current_location(), *win_pos));
                }
            }
        }

        None
    }

    pub fn refresh_pointer_outputs(&mut self) {
        if !self.pointer_visibility.is_visible() {
            return;
        }

        let _span = tracy_client::span!("Synoik::refresh_pointer_outputs");

        // Check whether we need to draw the tablet cursor or the regular cursor.
        let pointer_pos = self
            .tablet_cursor_location
            .unwrap_or_else(|| self.seat.get_pointer().unwrap().current_location());

        match self.cursor_manager.cursor_image() {
            CursorImageStatus::Surface(ref surface) => {
                let hotspot = with_states(surface, |states| {
                    states
                        .data_map
                        .get::<CursorImageSurfaceData>()
                        .unwrap()
                        .lock()
                        .unwrap()
                        .hotspot
                });

                let surface_pos = pointer_pos.to_i32_round() - hotspot;
                let bbox = bbox_from_surface_tree(surface, surface_pos);

                let dnd = self
                    .dnd_icon
                    .as_ref()
                    .map(|icon| &icon.surface)
                    .map(|surface| (surface, bbox_from_surface_tree(surface, surface_pos)));

                // FIXME we basically need to pick the largest scale factor across the overlapping
                // outputs, this is how it's usually done in clients as well.
                let mut cursor_scale = 1.;
                let mut cursor_transform = Transform::Normal;
                let mut dnd_scale = 1.;
                let mut dnd_transform = Transform::Normal;
                for output in self.global_space.outputs() {
                    let geo = self.global_space.output_geometry(output).unwrap();

                    // Compute pointer surface overlap.
                    if let Some(mut overlap) = geo.intersection(bbox) {
                        overlap.loc -= surface_pos;
                        cursor_scale =
                            f64::max(cursor_scale, output.current_scale().fractional_scale());
                        // FIXME: using the largest overlapping or "primary" output transform would
                        // make more sense here.
                        cursor_transform = output.current_transform();
                        output_update(output, Some(overlap), surface);
                    } else {
                        output_update(output, None, surface);
                    }

                    // Compute DnD icon surface overlap.
                    if let Some((surface, bbox)) = dnd {
                        if let Some(mut overlap) = geo.intersection(bbox) {
                            overlap.loc -= surface_pos;
                            dnd_scale =
                                f64::max(dnd_scale, output.current_scale().fractional_scale());
                            // FIXME: using the largest overlapping or "primary" output transform
                            // would make more sense here.
                            dnd_transform = output.current_transform();
                            output_update(output, Some(overlap), surface);
                        } else {
                            output_update(output, None, surface);
                        }
                    }
                }

                with_states(surface, |data| {
                    send_scale_transform(
                        surface,
                        data,
                        output::Scale::Fractional(cursor_scale),
                        cursor_transform,
                    )
                });
                if let Some((surface, _)) = dnd {
                    with_states(surface, |data| {
                        send_scale_transform(
                            surface,
                            data,
                            output::Scale::Fractional(dnd_scale),
                            dnd_transform,
                        );
                    });
                }
            }
            cursor_image => {
                // There's no cursor surface, but there might be a DnD icon.
                let Some(surface) = self.dnd_icon.as_ref().map(|icon| &icon.surface) else {
                    return;
                };

                let icon = if let CursorImageStatus::Named(icon) = cursor_image {
                    *icon
                } else {
                    Default::default()
                };

                let mut dnd_scale = 1.;
                let mut dnd_transform = Transform::Normal;
                for output in self.global_space.outputs() {
                    let geo = self.global_space.output_geometry(output).unwrap();

                    // The default cursor is rendered at the right scale for each output, which
                    // means that it may have a different hotspot for each output.
                    let output_scale = output.current_scale().integer_scale();
                    let cursor = self
                        .cursor_manager
                        .get_cursor_with_name(icon, output_scale)
                        .unwrap_or_else(|| self.cursor_manager.get_default_cursor(output_scale));

                    // For simplicity, we always use frame 0 for this computation. Let's hope the
                    // hotspot doesn't change between frames.
                    let hotspot = XCursor::hotspot(&cursor.frames()[0]).to_logical(output_scale);

                    let surface_pos = pointer_pos.to_i32_round() - hotspot;
                    let bbox = bbox_from_surface_tree(surface, surface_pos);

                    if let Some(mut overlap) = geo.intersection(bbox) {
                        overlap.loc -= surface_pos;
                        dnd_scale = f64::max(dnd_scale, output.current_scale().fractional_scale());
                        // FIXME: using the largest overlapping or "primary" output transform would
                        // make more sense here.
                        dnd_transform = output.current_transform();
                        output_update(output, Some(overlap), surface);
                    } else {
                        output_update(output, None, surface);
                    }
                }

                with_states(surface, |data| {
                    send_scale_transform(
                        surface,
                        data,
                        output::Scale::Fractional(dnd_scale),
                        dnd_transform,
                    );
                });
            }
        }
    }

    pub fn refresh_layout(&mut self) {
        let layout_is_active = match &self.keyboard_focus {
            KeyboardFocus::Layout { .. } => true,
            KeyboardFocus::LayerShell { .. } => false,

            // Draw layout as active in these cases to reduce unnecessary window animations.
            // There's no confusion because these are both fullscreen modes.
            //
            // FIXME: when going into the screenshot UI from a layer-shell focus, and then back to
            // layer-shell, the layout will briefly draw as active, despite never having focus.
            KeyboardFocus::LockScreen { .. } => true,
            KeyboardFocus::ScreenshotUi => true,
            KeyboardFocus::ExitConfirmDialog => true,
            KeyboardFocus::RunDialog => true,
            KeyboardFocus::EndSessionDialog => true,
            KeyboardFocus::PolkitDialog => true,
            KeyboardFocus::Popover => true,
            KeyboardFocus::Overview => true,
            KeyboardFocus::Switcher => true,
        };

        self.layout.refresh(layout_is_active);
    }

    pub fn refresh_idle_inhibit(&mut self) {
        let _span = tracy_client::span!("Synoik::refresh_idle_inhibit");

        self.idle_inhibiting_surfaces.retain(|s| s.is_alive());

        let is_inhibited = self.is_fdo_idle_inhibited.load(Ordering::SeqCst)
            || self.idle_inhibiting_surfaces.iter().any(|surface| {
                with_states(surface, |states| {
                    surface_primary_scanout_output(surface, states).is_some()
                })
            });
        self.idle_notifier_state.set_is_inhibited(is_inhibited);
    }

    pub fn refresh_window_states(&mut self) {
        let _span = tracy_client::span!("Synoik::refresh_window_states");

        let config = self.config.borrow();
        self.layout.with_windows_mut(|mapped, _output| {
            mapped.update_tiled_state(config.prefer_no_csd);
        });
        drop(config);
    }

    pub fn refresh_window_rules(&mut self) {
        let _span = tracy_client::span!("Synoik::refresh_window_rules");

        let config = self.config.borrow();
        let window_rules = &config.window_rules;

        let mut windows = vec![];
        let mut outputs = HashSet::new();
        self.layout.with_windows_mut(|mapped, output| {
            if mapped.recompute_window_rules_if_needed(window_rules, self.is_at_startup) {
                windows.push(mapped.window.clone());

                if let Some(output) = output {
                    outputs.insert(output.clone());
                }

                // Since refresh_window_rules() is called after refresh_layout(), we need to update
                // the tiled state right here, so that it's picked up by the following
                // send_pending_configure().
                mapped.update_tiled_state(config.prefer_no_csd);
            }
        });
        drop(config);

        for win in windows {
            self.layout.update_window(&win, None);
            win.toplevel()
                .expect("no X11 support")
                .send_pending_configure();
        }
        for output in outputs {
            self.queue_redraw(&output);
        }
    }

    pub fn advance_animations(&mut self) {
        let _span = tracy_client::span!("Synoik::advance_animations");

        self.layout.advance_animations();
        self.update_overview_search_fade();
        self.update_overview_search_expand();

        // Banners are blocked while a panel popover is open; syncing here (once per
        // frame) covers every open/close path with a single site. The drain check
        // below re-shows queued banners once the popover closes.
        // An app icon's context menu cannot outlive the overview it was opened from:
        // gnome-shell closes it on the overview's `hiding` (`appDisplay.js:3039-3040`).
        // Synced here, once per frame, because the overview closes from a dozen places
        // (a keybind, a click, an app launching) and this covers all of them.
        // A menu opened off the dock has no overview behind it to hide, so the rule does not
        // apply to it; the dock is held open underneath it instead (`Dock::set_menu_open`).
        if self.panel_popover.is_app_menu()
            && !self.layout.is_overview_open()
            && !self.app_menu_from_dock
        {
            self.panel_popover.close();
        }
        if !self.panel_popover.is_app_menu() {
            self.app_menu_from_dock = false;
        }

        self.notification_banner
            .set_blocked(self.panel_popover.is_open());
        if self.notification_banner.can_show() && !self.notifications.banner_queue.is_empty() {
            self.maybe_show_banner();
        }
        let banner_wakeup = self.notification_banner.next_wakeup();
        if let Some(event) = self.notification_banner.advance_animations() {
            use crate::ui::notification_banner::BannerEvent;
            if event == BannerEvent::HiddenNaturally {
                // The natural-hide path destroys transient notifications
                // (reason EXPIRED); a model-removed hide must not double-close.
                let effects = self.notifications.banner_hidden();
                self.apply_notification_effects(effects);
            }
            self.maybe_show_banner();
        }
        // The expiry deadline is armed inside the banner's `advance_animations`
        // (at the Showing→Shown transition) — the ONLY site that knows about it
        // is this comparison, so without it the wake-up timer is never armed and
        // a banner over a static (damage-free) desktop would outlive its 4 s.
        if self.notification_banner.next_wakeup() != banner_wakeup {
            self.reschedule_notification_banner_timer();
        }

        self.osd.advance_animations();

        // The switcher advances here like every other UI, not only from its event-loop timer: a
        // frame can arrive between timer arming and firing, and a test driving
        // `advance_animations` directly must see the same reveal a real frame would.
        let switcher_now = self.clock.now_unadjusted();
        if let Some(outcome) = self.switcher.advance(switcher_now) {
            self.pending_switcher_outcome = Some(outcome);
        }
        if self.switcher.take_just_shown() {
            self.osd.hide_all();
        }
        // Unlike the banner above, this compares against what the timer is armed for
        // rather than against the pre-advance deadline: the OSD's deadline is set in
        // `show()`, which runs between frames, so a before/after diff is always equal
        // and would never re-arm. See `osd_timer_at`.
        if self.osd.next_wakeup() != self.osd_timer_at {
            self.reschedule_osd_timer();
        }

        // Same shape as the OSD's, and for the same reason: the switcher's deadlines are set
        // between frames (by `show`, by every keypress re-arming the hover timer), so a
        // before/after diff around `advance` would always be equal and never re-arm.
        if self.switcher.next_deadline() != self.switcher_timer_at {
            self.reschedule_switcher_timer();
        }

        // The preview raise and the cycler's border are re-derived every frame rather than pushed
        // from each key: the selection moves under the keys, the popup reveals itself on a timer,
        // and the windows themselves can move or resize while the switcher is up. One writer,
        // driven by the frame, is what keeps every one of those in step.
        self.sync_switcher_preview();

        self.exit_confirm_dialog.advance_animations();
        self.end_session_dialog.advance_animations();
        self.polkit_ui.advance_animations();
        self.flashspot.advance(self.clock.now_unadjusted());
        self.ripples.advance(self.clock.now_unadjusted());
        // A drag out of the dock holds it open until the drop, wherever the pointer goes.
        // Derived from the drag itself rather than hooked into each of the five paths that end
        // one, none of which could then forget.
        self.dock.set_dragging(self.app_drag.is_some());
        // Same shape for the context menu: derived every frame from the popover, so no path
        // that closes the menu has to remember to release the dock.
        self.dock
            .set_menu_open(self.app_menu_from_dock && self.panel_popover.is_app_menu());
        self.dock.advance_animations();
        if self.dock.next_wakeup() != self.dock_timer_at {
            self.reschedule_dock_timer();
        }
        self.screenshot_ui.advance_animations();
        self.panel_popover.advance_animations();
        self.panel.advance_animations();

        for state in self.output_state.values_mut() {
            if let Some(transition) = &mut state.screen_transition {
                if transition.is_done() {
                    state.screen_transition = None;
                }
            }
        }
    }

    /// Answer any `Lock` callers whose shield is now settled, one way or the other.
    ///
    /// **Level-triggered, deliberately.** GNOME hangs its `LockAsync` on the `lock-screen-shown`
    /// *edge* (`shellDBus.js:538-545`), and `_resetLockScreen` returns early unless the shield is
    /// hidden (`screenShield.js:440-445`) — so a `Lock` arriving at an already-covered screen
    /// waits for a signal that will never come again. Asking "is the curtain down?" instead
    /// answers that case immediately, and needs no special-casing for the idle path's instantly
    /// settled curtain either.
    ///
    /// A shield that stopped being active before it landed resolves too: the caller wanted to know
    /// the screen was covered, and the honest answer is that it is not going to be.
    /// Whether the curtain is down and done moving.
    pub(crate) fn shield_curtain_landed(&self) -> bool {
        let now = crate::utils::get_monotonic_time();
        self.lock_screen.is_covering(now) && !self.lock_screen.is_sliding(now)
    }

    /// Tell the session bus whether the screensaver is on — but not before the curtain has landed.
    ///
    /// **Divergence, by decision (2026-08-01): no "beat", and no second lightbox.** GNOME defers
    /// `ActiveChanged` to the completion of a *short fade to black* that it runs on every manual
    /// lock — 300 ms of curtain, then a 300 ms dim (`screenShield.js:479-486`, `:316-319`), so the
    /// signal lands ~600 ms late. Its stated reason is not the look: gnome-settings-daemon blanks
    /// the display on `ActiveChanged`, and GNOME does not want that happening mid-animation
    /// (`:604-614`).
    ///
    /// We keep the reason and drop the mechanism. Blanking policy belongs to power management, not
    /// to the lock screen, so we do not dim on its behalf; what the lock screen owes is that its
    /// animation is *seen* rather than replaced by an immediate blank. Deferring the signal until
    /// the curtain has landed buys exactly that, and nothing else. The idle path is unaffected —
    /// its curtain settles instantly, so this publishes at once, as GNOME's non-animated branch
    /// does (`:487-490`).
    ///
    /// Rises wait; **falls do not**. Unlocking should stop telling the session the screensaver is
    /// on the moment it is untrue, and GNOME's `_setActive(false)` is likewise immediate
    /// (`:539`, `:581`).
    pub(crate) fn publish_shield_active(&mut self) {
        let active = self.screen_shield.is_active() && self.shield_curtain_landed();
        if active == self.published_active {
            return;
        }
        self.published_active = active;

        {
            self.shield_snapshot.lock().unwrap().active = active;
            if let Some(tx) = self.screen_saver_emit.as_ref() {
                let _ = tx.send_blocking(
                    crate::dbus::gnome_screen_saver::SynoikToScreenSaver::ActiveChanged(active),
                );
            }
        }

        // The same edge is what releases the sleep inhibitor, as it is in GNOME — `_setActive`
        // ends in `_syncInhibitor` (`:156-164`), and `_isActive` is the condition (`:202-207`).
        //
        // Only in the *releasing* direction, and only while the fd is actually held: this runs
        // from the render path, and taking the inhibitor is a blocking D-Bus round trip that has
        // no business inside a frame. Nothing is lost — every edge that has to *take* it (a shield
        // going away, a settings change, the VT coming back) arrives through
        // `State::apply_shield_effects` or its own handler, which sync there.
        if active && self.sleep_inhibitor.is_some() {
            self.sync_sleep_inhibitor();
        }
    }

    /// How long a suspend may be held waiting for the shield to reach the screen.
    ///
    /// The wait it bounds is a 250 ms slide plus one flip, so this is already several times the
    /// honest cost; what it is really sized against is the case where the frame never comes at all
    /// — a blanked output, a wedged GPU, a monitor unplugged mid-suspend. Without it those ride the
    /// suspend all the way to logind's `InhibitDelayMaxSec` (5 s by default) and *then* suspend
    /// anyway, so the only thing a longer bound buys is a longer lid-close.
    const SHIELD_PRESENT_DEADLINE: Duration = Duration::from_secs(1);

    /// Hold or release logind's `delay` sleep inhibitor to match the shield's state
    /// (`_syncInhibitor`, `screenShield.js:202-231`).
    ///
    /// On [`Synoik`] rather than `State` because the release edge is now a *presentation*: the
    /// backends reach it from a vblank, where there is no `State` to be had.
    pub fn sync_sleep_inhibitor(&mut self) {
        let want = self
            .screen_shield
            .wants_sleep_inhibitor(self.session_active, self.curtain_frame_owed());
        if want == self.sleep_inhibitor.is_some() {
            return;
        }

        if !want {
            // Dropping the fd is the "go ahead" logind is blocked on.
            self.sleep_inhibitor = None;
            return;
        }

        let Some(conn) = self.dbus.as_ref().and_then(|d| d.conn_login1.as_ref()) else {
            return;
        };
        self.sleep_inhibitor = crate::dbus::freedesktop_login1::take_sleep_inhibitor(conn);
    }

    /// Whether the screen still owes a picture of the shield, in either of the two senses that can
    /// hold the inhibitor.
    ///
    /// The second half is the suspend's, and it is the stronger one. The first is GNOME's, and it
    /// has to be there *whether or not a suspend has been announced*: the fd must already be held
    /// when `PrepareForSleep` arrives, because taking it afterwards buys no delay at all. Lock the
    /// screen and close the lid a tenth of a second later and the whole 250 ms slide is a window
    /// where nothing is held — which is what `!this._isActive` covers for GNOME, `_isActive` being
    /// flipped at the end of the ease (`screenShield.js:479-490`, `:316-319`).
    pub fn curtain_frame_owed(&self) -> bool {
        (self.screen_shield.is_active() && !self.shield_curtain_landed())
            || self.shield_frame_owed()
    }

    /// Whether a pending suspend is still waiting for the shield to reach a scanout buffer.
    pub fn shield_frame_owed(&self) -> bool {
        self.shield_frames_owed
            .iter()
            .any(|output| self.output_state.contains_key(output))
    }

    /// `PrepareForSleep(true)`: wait for the curtain to be *on screen* before releasing the
    /// inhibitor, on every output that is currently drawing.
    ///
    /// Only called when the curtain still has to travel — a suspend arriving at an already-covered
    /// screen owes no frame, and would otherwise wait for a flip that nothing is going to ask for.
    pub fn arm_shield_present_wait(&mut self) {
        self.shield_frames_owed = self.output_state.keys().cloned().collect();
        if self.shield_frames_owed.is_empty() {
            return;
        }

        if let Some(token) = self.shield_present_deadline.take() {
            self.event_loop.remove(token);
        }
        let timer = calloop::timer::Timer::from_duration(Self::SHIELD_PRESENT_DEADLINE);
        self.shield_present_deadline = self
            .event_loop
            .insert_source(timer, move |_, _, state| {
                if !state.synoik.shield_frames_owed.is_empty() {
                    warn!("the shield never reached the screen; letting the suspend go ahead");
                    state.synoik.clear_shield_present_wait();
                }
                calloop::timer::TimeoutAction::Drop
            })
            .map_err(|err| warn!("error arming the shield presentation deadline: {err:?}"))
            .ok();
    }

    /// Stop waiting, whatever the reason — the frame landed, the deadline passed, or the suspend
    /// was called off. Releases the inhibitor if that was the last thing holding it.
    pub fn clear_shield_present_wait(&mut self) {
        self.shield_frames_owed.clear();
        if let Some(token) = self.shield_present_deadline.take() {
            self.event_loop.remove(token);
        }
        for state in self.output_state.values_mut() {
            state.shield_frame_queued = false;
        }
        self.sync_sleep_inhibitor();
    }

    /// A frame carrying the settled curtain has been presented on `output`.
    ///
    /// Called from the backends' presentation edge — the point after which the buffer this names is
    /// what a snapshot of the machine would contain.
    pub fn note_shield_frame_presented(&mut self, output: &Output) {
        if !self.shield_frames_owed.remove(output) {
            return;
        }
        if self.shield_frames_owed.is_empty() {
            self.clear_shield_present_wait();
        }
    }

    pub(crate) fn settle_lock_replies(&mut self) {
        if self.lock_replies.is_empty() {
            return;
        }
        if self.shield_curtain_landed() || !self.screen_shield.is_active() {
            for reply in self.lock_replies.drain(..) {
                reply.answer();
            }
        }
    }

    /// Reflect the overview's open state on the panel chrome, and dismiss a panel menu the
    /// overview opened over.
    ///
    /// Runs from `State::refresh`, i.e. where the state it reads actually changes — **not** from
    /// `update_render_elements`, which is where it used to live. Arming the Activities fade inside
    /// the render meant the frame that opened the overview drew the button unlit and the highlight
    /// only latched on the next advance+render: one frame late on the seat, and a state no test
    /// could observe without rendering first (the tests here used to call
    /// `update_render_elements` purely to make the panel catch up).
    pub(crate) fn refresh_overview_panel_state(&mut self) {
        let overview_open = self.layout.is_overview_open();
        self.panel.set_overview_open(overview_open);

        // A menu open when the overview *opens* is dismissed (GNOME's overview modal
        // won't coexist with a held menu grab — `js/ui/overview.js:461`), but a menu
        // opened while the overview is already up pushes its own grab on top and stays
        // (`js/ui/popupMenu.js:1520`) — so this must fire on the closed→open edge only,
        // never level-triggered (that closed popovers the frame after they opened in
        // the overview). Leaving GNOME mode still closes unconditionally, so the modal
        // keyboard grab (`KeyboardFocus::Popover`) can never get stuck with no way to
        // dismiss it.
        let overview_just_opened = overview_open && !self.overview_was_open;
        self.overview_was_open = overview_open;
        if self.panel_popover.is_open() && (overview_just_opened || !self.layout.is_gnome_mode()) {
            self.panel_popover.close();
        }

        // Keep the panel button containers' active state in sync with the open popover
        // (the clock/quick-settings button stays lit while its menu is up).
        let open_role = self.panel_popover.open_role();
        self.panel.set_open_menu(open_role);
    }

    pub fn update_render_elements(&mut self, output: Option<&Output>) {
        self.update_xray_render_elements(output);
        self.layout.update_render_elements(output);

        // Retire a finished curtain slide. Without this the state stays `Hiding` forever and the
        // *next* lock is told the curtain is already on its way, so it never descends.
        self.lock_screen
            .settle_curtain(crate::utils::get_monotonic_time());
        self.settle_lock_replies();
        self.publish_shield_active();

        for (out, state) in self.output_state.iter_mut() {
            if output.is_none_or(|output| out == output) {
                let scale = Scale::from(out.current_scale().fractional_scale());
                let transform = out.current_transform();

                if let Some(transition) = &mut state.screen_transition {
                    transition.update_render_elements(scale, transform);
                }

                let layer_map = layer_map_for_output(out);
                for surface in layer_map.layers() {
                    let Some(mapped) = self.mapped_layer_surfaces.get_mut(surface) else {
                        continue;
                    };
                    let Some(geo) = layer_map.layer_geometry(surface) else {
                        continue;
                    };

                    mapped.update_render_elements(geo.size.to_f64());
                }
            }
        }
    }

    // Updates only those render elements that go in the xray buffer.
    pub fn update_xray_render_elements(&mut self, output: Option<&Output>) {
        for (out, state) in self.output_state.iter_mut() {
            if output.is_none_or(|output| out == output) {
                let scale = Scale::from(out.current_scale().fractional_scale());
                let mode = out.current_mode().unwrap();
                let transform = out.current_transform();
                let size = transform.transform_size(mode.size);

                state.xray.workspaces.clear();
                let mon = self.layout.monitor_for_output(out).unwrap();
                for (ws, geo) in mon.workspaces_with_render_geo() {
                    let bg_color = ws.render_background().color();
                    state.xray.workspaces.push((geo, bg_color));
                }
                state.xray.backdrop_color = state.backdrop_buffer.color();
                let blur_options = BlurOptions::from(self.config.borrow().blur);
                for buf in &state.xray.background {
                    let mut buffer = buf.borrow_mut();
                    buffer.update_size(size, scale);
                    buffer.update_blur_options(blur_options);
                }
                for buf in &state.xray.backdrop {
                    let mut buffer = buf.borrow_mut();
                    buffer.update_size(size, scale);
                    buffer.update_blur_options(blur_options);
                }

                let layer_map = layer_map_for_output(out);
                for surface in layer_map.layers_on(Layer::Background) {
                    let Some(mapped) = self.mapped_layer_surfaces.get_mut(surface) else {
                        continue;
                    };
                    let Some(geo) = layer_map.layer_geometry(surface) else {
                        continue;
                    };

                    mapped.update_render_elements(geo.size.to_f64());
                }
            }
        }
    }

    pub fn update_shaders(&mut self) {
        self.layout.update_shaders();

        for mapped in self.mapped_layer_surfaces.values_mut() {
            mapped.update_shaders();
        }
    }

    pub fn render_to_vec(
        &self,
        ctx: RenderCtx,
        output: &Output,
        include_pointer: bool,
    ) -> Vec<OutputRenderElements> {
        let mut elements = Vec::new();
        self.render(ctx, output, include_pointer, &mut |elem| {
            elements.push(elem)
        });
        elements
    }

    pub fn render(
        &self,
        mut ctx: RenderCtx,
        output: &Output,
        include_pointer: bool,
        push: &mut dyn FnMut(OutputRenderElements),
    ) {
        let _span = tracy_client::span!("Synoik::render");

        if ctx.target == RenderTarget::Output {
            if let Some(preview) = self.config.borrow().debug.preview_render {
                ctx.target = match preview {
                    PreviewRender::Screencast => RenderTarget::Screencast,
                    PreviewRender::ScreenCapture => RenderTarget::ScreenCapture,
                };
            }
        }

        // Fill the xray background/backdrop capture buffers.
        self.fill_xray_elements(ctx.r(), output);

        // Reborrow to shorten lifetime to be able to put in xray.
        let mut ctx = ctx.r();
        let state = self.output_state.get(output).unwrap();
        ctx.xray = Some(&state.xray);

        self.render_inner(ctx, output, include_pointer, push);

        self.clear_xray_elements(output);
    }

    fn render_inner(
        &self,
        mut ctx: RenderCtx,
        output: &Output,
        include_pointer: bool,
        push: &mut dyn FnMut(OutputRenderElements),
    ) {
        let state = self.output_state.get(output).unwrap();
        let output_scale = Scale::from(output.current_scale().fractional_scale());

        let push = if self.debug_draw_opaque_regions {
            &mut move |elem| {
                push_opaque_regions(&elem, output_scale, push);
                push(elem);
            }
        } else {
            push
        };

        // The pointer goes on the top.
        if include_pointer && self.pointer_visibility.is_visible() {
            self.render_pointer(ctx.renderer, output, &mut |elem| push(elem.into()));
        }

        // Next, the screenshot flash: over everything the capture could have contained.
        self.flashspot.render(
            output,
            output.current_location(),
            self.clock.now_unadjusted(),
            &mut |elem| push(elem.into()),
        );

        // Next, the hot-corner ripple, which gnome-shell raises above the rest of the shell UI
        // (`Ripples.playAnimation`, `ripples.js:99-101`).
        self.ripples.render(
            ctx.renderer,
            output,
            output.current_location(),
            self.clock.now_unadjusted(),
            &mut |elem| push(elem.into()),
        );

        // Next, the screen transition texture.
        {
            if let Some(transition) = &state.screen_transition {
                if let Some(elem) = transition.render(ctx.renderer, ctx.target) {
                    push(elem.into());
                }
            }
        }

        // Next, the exit confirm dialog.
        self.exit_confirm_dialog
            .render(ctx.renderer, output, &mut |elem| push(elem.into()));

        // Next, the run dialog.
        self.run_dialog
            .render(ctx.renderer, output, &mut |elem| push(elem.into()));

        // Next, the end-session (logout/shutdown/restart) confirmation dialog.
        self.end_session_dialog.render(
            ctx.renderer,
            &self.icon_cache,
            output,
            self.gnome_settings.accent_color,
            &mut |elem| push(elem.into()),
        );

        // Next, the polkit authentication dialog. Above the three above it and below the lock
        // surface, which is the order it can actually be in: it defers rather than stacking on a
        // shield.
        self.polkit_ui.render(
            ctx.renderer,
            output,
            &self.icon_cache,
            &self.image_cache,
            &self.polkit_dialog,
            self.gnome_settings.accent_color,
            self.clock.now_unadjusted(),
            &mut |elem| push(elem.into()),
        );

        // If the session is locked, draw the lock surface.
        if self.is_locked() {
            if let Some(surface) = state.lock_surface.as_ref() {
                push_elements_from_surface_tree(
                    ctx.renderer,
                    surface.wl_surface(),
                    Point::new(0, 0),
                    output_scale,
                    1.,
                    Kind::ScanoutCandidate,
                    &mut |elem| push(elem.into()),
                );
            }

            // Draw the solid color background.
            push(
                SolidColorRenderElement::from_buffer(
                    &state.lock_color_buffer,
                    (0., 0.),
                    1.,
                    Kind::Unspecified,
                )
                .into(),
            );

            return;
        }

        // The delayed-capture countdown. Below the pointer and the dialogs, and — because it sits
        // *after* the lock branch's early return — never over a lock surface: the tick cancels a
        // capture the lock caught, but not before the frame that locked. `element` refuses any
        // target but `Output`, so the countdown can never reach a shot, a cast or a portal capture,
        // which is the entire point of a delay.
        if let Some(pending) = &self.pending_capture {
            let seconds = pending.seconds_left(self.clock.now_unadjusted());
            if let Some(elem) =
                self.capture_countdown
                    .element(ctx.renderer, ctx.target, output, seconds)
            {
                push(elem.into());
            }
        }

        // The shade marking a running area recording. GNOME parents its copy directly to the stage
        // "so that it's above popup menus" (`js/ui/screenshot.js:1205-1206`), so this goes above
        // everything the shell draws below — and, like the countdown, only on the screen itself.
        self.cast_area_indicator
            .push(ctx.target, output, |elem| push(elem.into()));

        // Next, the screen shield's curtain. Below `ext-session-lock` above — that protocol is a
        // stronger claim on the screen and there is no sense in drawing both — but above
        // everything else, because the whole point is that the desktop is not visible.
        let now = crate::utils::get_monotonic_time();
        // Not `is_active`: the curtain outlives the shield by the length of its slide away, and
        // gating the draw on the model would turn unlocking into a hard cut.
        if self.lock_screen.is_covering(now) {
            let size = output_size(output);
            // `WallClock` is wall time, not the monotonic clock the fade runs on.
            let epoch = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as libc::time_t);
            // Mid-crossfade both pages are on screen at once, each with its own alpha, scale and
            // offset — the pair is what reads as one page giving way to the other, so drawing only
            // the winner would be a hard cut with extra steps.
            //
            // The progress carries its own direction: `_showClock` eases the *same* adjustment back
            // to 0 that `_showPrompt` eased to 1, so leaving the prompt is the same animation run
            // backwards (`:786-810`). Re-deriving "are we on the prompt?" here and forcing 0 when
            // not made Escape a hard cut — the way back has to animate too. Which page the shield
            // is allowed to be on is `sync_lock_page`'s business, decided once when it changes.
            let progress = self.lock_screen.page_progress(now);
            // `_lockDialogGroup.translation_y`: the whole group — backdrop, clock, prompt —
            // slides as one, so this is added to every element below rather than applied to any of
            // them individually.
            let slide = -self.lock_screen.curtain_progress(now) * size.h;

            let page_ctx = crate::ui::lock_screen::PageCtx {
                scale: output_scale.x,
                monitor: Rectangle::from_size(size),
                now,
            };

            let mut prompt_t = crate::ui::lock_screen::PageTransform::prompt(progress);
            let mut clock_t = crate::ui::lock_screen::PageTransform::clock(progress);
            prompt_t.translation_y += slide;
            clock_t.translation_y += slide;

            if prompt_t.is_visible() {
                let d = &self.unlock_dialog;
                let (caret, selection) = match d.entry_mask() {
                    Some(mask) => d.entry().masked_positions(mask, d.is_entry_live()),
                    None if d.is_entry_live() => (Some(d.entry().cursor()), d.entry().selection()),
                    None => (None, None),
                };
                let content = crate::ui::lock_screen::PromptContent {
                    display_name: d.user().display_name().to_owned(),
                    entry: d.entry_display(),
                    // The caret in *masked* coordinates: `d.entry()` never leaves the model.
                    cursor: caret,
                    selection,
                    question: d.question().unwrap_or_default().to_owned(),
                    message: d.message().map(|m| m.text.clone()),
                    message_is_error: d
                        .message()
                        .is_some_and(|m| m.kind == crate::dbus::gdm::MessageKind::Error),
                    entry_live: d.is_entry_live(),
                    peek: d.peek(),
                    avatar: self.avatar_source(),
                };
                for elem in self.lock_screen.render_prompt(
                    ctx.renderer,
                    &self.icon_cache,
                    &self.image_cache,
                    page_ctx,
                    &content,
                    prompt_t,
                ) {
                    match elem {
                        crate::ui::lock_screen::PromptElement::Texture(e) => push(e.into()),
                        crate::ui::lock_screen::PromptElement::Rounded(e) => push(e.into()),
                    }
                }

                // The switch-user button is a sibling of the page stack, not part of it, so it is
                // drawn separately — but only while the prompt is up, which is what its
                // `progress > 0` reactivity and opacity amount to (`unlockDialog.js:811-821`).
                if self.switch_user_reactive(now) {
                    // Same alpha and scale as the prompt, but it does not slide with the page: the
                    // curtain's `translation_y` is the *group's*, so it applies, while the page's
                    // own FADE_OUT_TRANSLATION does not (`:838-842` sets no translation on it).
                    let mut switch_t = crate::ui::lock_screen::PageTransform::prompt(progress);
                    switch_t.translation_y = slide;
                    for elem in self.lock_screen.render_switch_user(
                        ctx.renderer,
                        &self.icon_cache,
                        page_ctx,
                        self.switch_user_hovered,
                        self.gnome_settings.accent_color,
                        switch_t,
                    ) {
                        match elem {
                            crate::ui::lock_screen::PromptElement::Texture(e) => push(e.into()),
                            crate::ui::lock_screen::PromptElement::Rounded(e) => push(e.into()),
                        }
                    }
                }
            }

            if clock_t.is_visible() {
                let content = crate::ui::lock_screen::ClockContent::new(
                    epoch,
                    self.gnome_settings.clock,
                    // Touch mode is a seat property we do not track yet; the pointer wording is
                    // the safe default (it is also what a seat with a pointer reports).
                    false,
                );
                for elem in self
                    .lock_screen
                    .render(ctx.renderer, page_ctx, &content, clock_t)
                {
                    push(elem.into());
                }
            }

            // The backdrop: GNOME blurs the wallpaper itself and multiplies it by
            // `BLUR_BRIGHTNESS` (`unlockDialog.js:706-713`), so the brightness rides *in* the blur
            // rather than being a wash laid over it.
            //
            // `BLUR_RADIUS` is in stage pixels, which for us is output physical pixels — hence the
            // scale, and hence `render_blurred` doing its own conversion into the wallpaper
            // texture's resolution.
            let radius = crate::ui::lock_screen::BLUR_RADIUS * output_scale.x;
            let brightness = crate::ui::lock_screen::BLUR_BRIGHTNESS as f32;
            let origin = Point::<f64, Logical>::from((0., slide));
            let blurred = self.wallpaper.render_blurred(
                ctx.renderer,
                origin,
                size,
                output_scale,
                radius,
                brightness,
            );

            match blurred {
                Some(elem) => push(elem.into()),
                None => {
                    // No blur to be had. The dim still has to happen, or a bright wallpaper would
                    // sit under white 72pt text — so fall back to the flat wash this used to be.
                    push(
                        SolidColorRenderElement::from_buffer(
                            &state.shield_dim_buffer,
                            (0., slide),
                            1.,
                            Kind::Unspecified,
                        )
                        .into(),
                    );
                    if let Some(elem) =
                        self.wallpaper
                            .render(ctx.renderer, origin, size, 0., output_scale)
                    {
                        push(elem.into());
                    }
                }
            }
            // With no wallpaper the dim alone is translucent black over nothing, which is the one
            // way this branch could leave the desktop showing. The backstop closes it.
            push(
                SolidColorRenderElement::from_buffer(
                    &state.shield_backstop_buffer,
                    (0., slide),
                    1.,
                    Kind::Unspecified,
                )
                .into(),
            );

            // Only a curtain that is all the way down hides everything. While it slides it has
            // vacated part of the screen, and returning here would leave that band holding whatever
            // was in the framebuffer instead of the desktop it is uncovering.
            if slide == 0. {
                return;
            }
        }

        // The idle fade to black, over the desktop and under the shield that is about to cover it.
        let fade = self.lock_screen.fade_alpha(now);
        if fade > 0. {
            push(
                SolidColorRenderElement::from_buffer(
                    &state.shield_backstop_buffer,
                    (0., 0.),
                    fade as f32,
                    Kind::Unspecified,
                )
                .into(),
            );
        }

        // Prepare the background elements.
        let backdrop = SolidColorRenderElement::from_buffer(
            &state.backdrop_buffer,
            (0., 0.),
            1.,
            Kind::Unspecified,
        )
        .into();

        // If the screenshot UI is open, draw it.
        if self.screenshot_ui.is_open() {
            let accent = crate::ui::widget::style::accent_rgba(self.gnome_settings.accent_color);
            self.screenshot_ui.render_output(
                ctx.renderer,
                &self.icon_cache,
                accent,
                output,
                ctx.target,
                &mut |elem| push(elem.into()),
            );

            // In Shot mode the frozen screenshot *is* the desktop — the real scene is never
            // rendered, which is what makes the picker a still. Cast mode has no still (see
            // `ScreenshotUi::render_output`), so it must fall through and let the live scene draw
            // underneath the chrome we just pushed; elements pushed first are topmost, so the
            // picker still sits on top.
            if self.screenshot_ui.mode() == CaptureMode::Shot {
                // Add the backdrop for outputs that were connected while the UI was open.
                push(backdrop);
                return;
            }
        }

        // An app icon being dragged onto a workspace rides above everything, like
        // gnome-shell's DND actor in `Main.uiGroup` (`dnd.js:213-216`).
        if let Some(drag) = &self.app_drag {
            if drag.output == *output {
                let scale = output.current_scale().fractional_scale();
                let center = drag.pos + drag.grab_offset;
                let mut uploads = self.app_icon_uploads.borrow_mut();
                let mut icon_at = |icon: &AppIconRef, px: f64, at: Point<f64, Logical>| {
                    crate::ui::widget::app_icon_element(
                        ctx.renderer,
                        &mut uploads,
                        &self.app_icon_cache,
                        icon,
                        px,
                        scale,
                        Point::from((0., 0.)),
                        at,
                        1.,
                    )
                };
                match &drag.folder {
                    // A folder drags as its whole tile: the 2×2 composition over the
                    // raised `.app-folder` fill, laid out by the same `TileMetrics` the
                    // grid uses so the proxy matches the tile it left.
                    Some(members) => {
                        let metrics = crate::ui::widget::TileMetrics {
                            icon_px: APP_DRAG_ICON_PX,
                            ..crate::ui::widget::TileMetrics::overview()
                        };
                        // The tile's caption space is cropped off: our drag proxy carries
                        // no name (GNOME's does), so the fill is the padded icon box, which
                        // leaves `icon_center` exactly on the pointer.
                        let side = APP_DRAG_ICON_PX + 2. * metrics.pad;
                        let tile = Rectangle::new(
                            Point::from((center.x - side / 2., center.y - side / 2.)),
                            Size::from((side, side)),
                        );
                        for (i, icon) in members.iter().take(4).enumerate() {
                            let at = metrics.folder_subicon_center(tile, i);
                            if let Some(el) = icon_at(icon, metrics.folder_subicon_px(), at) {
                                push(el.into());
                            }
                        }
                        drop(uploads);
                        let mut bg = self.app_drag_bg.borrow_mut();
                        bg.update(
                            tile.size,
                            metrics.radius,
                            crate::ui::widget::style::folder_bg(self.appearance()),
                        );
                        push(
                            crate::render_helpers::rounded_solid::RoundedSolidRenderElement::from_buffer(
                                &bg,
                                tile.loc,
                                Scale::from(scale),
                                Kind::Unspecified,
                            )
                            .into(),
                        );
                    }
                    None => {
                        if let Some(element) = icon_at(&drag.icon, APP_DRAG_ICON_PX, center) {
                            push(element.into());
                        }
                    }
                }
            }
        }

        // Draw the hotkey overlay on top.
        if let Some(element) = self.hotkey_overlay.render(ctx.renderer, output) {
            push(element.into());
        }

        // Then, the Alt-Tab switcher. GNOME's popup is pushed into `uiGroup` on top of the
        // windows but below the OSD, which raises itself on show (`switcherPopup.js:178` hides
        // every OSD when the switcher becomes visible, so the two are never up together anyway).
        self.switcher
            .render_output(self, output, ctx.r(), &mut |elem| push(elem.into()));

        // The GNOME top panel sits above the windows (but below the transient
        // overlays above). It stays up during the overview, matching gnome-shell.
        // This is after the lock/screenshot early-returns, so it is hidden there.
        if self.layout.is_gnome_mode() {
            let ws = self.workspace_state_for(output);
            let ws_position = self.workspace_position_for(output);
            // The bar background fades out as the overview opens (`#panel:overview`),
            // so the overview backdrop runs unbroken behind it.
            let overview_fade = self
                .layout
                .monitor_for_output(output)
                .and_then(|mon| mon.expose_progress())
                .unwrap_or(0.);
            // The OSD raises itself above everything else on show
            // (`js/ui/osdWindow.js:98` lifts it to the top of `uiGroup`), so it is
            // pushed before the panel and its popovers — earlier push = higher z.
            for element in self.osd.render(ctx.renderer, &self.icon_cache, output) {
                push(element.into());
            }
            // Hidden over a fullscreen window, and with it its popovers — see
            // `panel_visible_on`. The OSD above is deliberately outside this: it is not
            // `trackFullscreen` chrome and shows over fullscreen windows in GNOME too.
            if self.panel_visible_on(output) {
                for element in self.panel.render(
                    ctx.renderer,
                    output,
                    ws,
                    ws_position,
                    overview_fade,
                    crate::render_helpers::icon::DrawCaches {
                        icons: &self.icon_cache,
                        images: &self.image_cache,
                    },
                ) {
                    push(element.into());
                }
                // A panel popover (dateMenu calendar, quick settings, …) sits above the
                // bar; the quick-settings menu composites several elements (chrome + icons).
                for element in self.panel_popover.render(
                    ctx.renderer,
                    &self.icon_cache,
                    &self.app_icon_cache,
                    &self.image_cache,
                    output,
                ) {
                    push(element.into());
                }
            }
            // The notification banner slides out from under the bar (pushed after
            // the panel = below it in z, like gnome-shell's tray behind the panel).
            for element in self.notification_banner.render(
                ctx.renderer,
                &self.icon_cache,
                &self.app_icon_cache,
                output,
            ) {
                push(element.into());
            }
            // The dock: the same dash, drawn at the bottom edge with the overview shut. Pushed
            // here — after the panel, before the overview block — so it sits under the panel and
            // its popovers like the overview's dash does, and over the windows it overlays.
            if let Some(area) = self
                .dash_area(output)
                .filter(|_| self.dock_owns_dash(output))
            {
                for element in self.dash.render(
                    ctx.renderer,
                    &self.app_icon_cache,
                    &self.icon_cache,
                    output,
                    area,
                    1.,
                    // The dock hangs over the raw desktop, so it brings its own blur; the
                    // overview's dash below sits on a backdrop that is already blurred.
                    true,
                    self.appearance(),
                    self.gnome_settings.accent_color,
                    self.dock.is_poking(),
                ) {
                    push(element.into());
                }
            }

            // The overview dash (favorites) fades in with the overview — above the
            // zoomed workspaces (pushed later, below), below the panel/popover/banner.
            if let Some((progress, controls)) = self
                .layout
                .monitor_for_output(output)
                .and_then(|mon| Some((mon.expose_progress()?, mon.controls_layout())))
            {
                // The app-folder dialog is modal over the whole overview: gnome-shell
                // parents it to the `overviewGroup` and raises it above every sibling
                // (`addFolderDialog` + `popup`, `appDisplay.js:1621-1622,2888`), so it is
                // pushed first — nothing else in the overview draws over it.
                // The zoom animates out of (and back into) the source folder's tile, so the
                // dialog needs that tile's box on *this* output — `None` while the tile is
                // on another page, which draws it untransformed.
                let source = self.folder_dialog.folder_id().and_then(|id| {
                    let i = self.app_grid.index_of(id)?;
                    self.app_grid.entry_rect(i, controls.app_display)
                });
                self.folder_dialog.render(
                    ctx.renderer,
                    &self.app_icon_cache,
                    &self.icon_cache,
                    output,
                    Rectangle::from_size(output_size(output)),
                    source,
                    progress as f32,
                    self.gnome_settings.accent_color,
                    self.appearance(),
                    &mut |element| push(element.into()),
                );
                for element in self.dash.render(
                    ctx.renderer,
                    &self.app_icon_cache,
                    &self.icon_cache,
                    output,
                    controls.dash,
                    progress,
                    false,
                    self.appearance(),
                    self.gnome_settings.accent_color,
                    // The overview always shows the whole dash.
                    false,
                ) {
                    push(element.into());
                }
                // The overview search entry (top) + results grid, faded with the
                // overview like the dash.
                for element in self.overview_search.render(
                    ctx.renderer,
                    &self.app_icon_cache,
                    &self.icon_cache,
                    output,
                    controls.into(),
                    crate::ui::overview_search::SearchFade {
                        overview: progress,
                        search: self.overview_search_fade(),
                    },
                    self.gnome_settings.accent_color,
                    self.appearance(),
                ) {
                    push(element.into());
                }
                // The app grid sits in the `app_display` band, below the dash and the
                // search (child order `overviewControls.js:374-379`) — pushed after
                // them so they draw on top. It slides up from off-screen on the moving
                // `app_display` box, and cross-fades with the window picker behind it:
                // its alpha is the app-grid leg, the picker's is what is left of it.
                //
                // **Divergence.** GNOME's grid does not fade on the state axis at all
                // (its opacity only rides the search cross-fade,
                // `overviewControls.js:582-627`) — it does not need to, because there the
                // picker *becomes* the app-grid row and there is nothing behind the grid to
                // reveal. Ours fades because the picker stays put and fades back in, which
                // only reads as one movement if the two are a true cross-fade.
                let app_grid_leg = self
                    .layout
                    .monitor_for_output(output)
                    .map_or(0., |mon| mon.app_grid_leg());
                if app_grid_leg > 0. {
                    let alpha =
                        (app_grid_leg * progress * (1. - self.overview_search_fade())) as f32;
                    for element in self.app_grid.render(
                        ctx.renderer,
                        &self.app_icon_cache,
                        &self.icon_cache,
                        output,
                        controls.app_display,
                        alpha,
                        self.gnome_settings.accent_color,
                        self.appearance(),
                    ) {
                        push(element.into());
                    }
                }
            }
        }

        // Don't draw the focus ring on the workspaces while interactively moving above those
        // workspaces, since the interactively-moved window already has a focus ring.
        let focus_ring = !self.layout.interactive_move_is_moving_above_output(output);

        // The window picker and the workspace row cross-fade out as a search covers
        // them; outside the overview the fade is 0, so this is a plain pass-through.
        let fade_scale = output.current_scale().fractional_scale();
        let row_alpha = (1. - self.overview_search_fade()) as f32;
        // The picker *also* fades away as the app grid opens — see below. Off the *leg*,
        // not the raw show-apps scalar: the scalar is frozen across a close, so reading it
        // here kept the picker at alpha 0 for the whole way back to the desktop.
        let picker_alpha = row_alpha
            * self
                .layout
                .monitor_for_output(output)
                .map_or(1., |mon| 1. - mon.app_grid_leg()) as f32;

        // Get monitor elements.
        let mon = self.layout.monitor_for_output(output).unwrap();
        let zoom = mon.overview_zoom();

        // In GNOME windowing mode the org.gnome.desktop.background wallpaper
        // backs every workspace. In the overview its corners round like
        // gnome-shell's `.workspace-background`; the workspace shadow rounds on
        // the same radius, so both take it from the one accessor.
        let gnome_mode = self.config.borrow().layout.windowing_mode == WindowingMode::Floating;
        let wallpaper_radius = mon.workspace_background_radius();

        // Get layer-shell elements.
        let layer_map = layer_map_for_output(output);

        // We use macros instead of closures to avoid borrowing issues (renderer and push() go
        // into different functions).
        macro_rules! push_popups_from_layer {
            ($layer:expr, $ns:expr, $xray_pos:expr, $backdrop:expr, $push:expr) => {{
                self.render_layer_popups(
                    ctx.r(),
                    $ns,
                    &layer_map,
                    $layer,
                    $xray_pos,
                    $backdrop,
                    $push,
                );
            }};
            ($layer:expr, true) => {{
                push_popups_from_layer!($layer, None, XrayPos::default(), true, &mut |elem| push(
                    elem.into()
                ));
            }};
            ($layer:expr, $ns:expr, $xray_pos:expr, $push:expr) => {{
                push_popups_from_layer!($layer, $ns, $xray_pos, false, $push);
            }};
            ($layer:expr) => {{
                push_popups_from_layer!($layer, None, XrayPos::default(), false, &mut |elem| push(
                    elem.into()
                ));
            }};
        }
        macro_rules! push_normal_from_layer {
            ($layer:expr, $ns:expr, $xray_pos:expr, $backdrop:expr, $push:expr) => {{
                self.render_layer_normal(
                    ctx.r(),
                    $ns,
                    &layer_map,
                    $layer,
                    $xray_pos,
                    $backdrop,
                    $push,
                );
            }};
            ($layer:expr, true) => {{
                push_normal_from_layer!($layer, None, XrayPos::default(), true, &mut |elem| {
                    push(elem.into())
                });
            }};
            ($layer:expr, $ns:expr, $xray_pos:expr, $push:expr) => {{
                push_normal_from_layer!($layer, $ns, $xray_pos, false, $push);
            }};
            ($layer:expr) => {{
                push_normal_from_layer!($layer, None, XrayPos::default(), false, &mut |elem| {
                    push(elem.into())
                });
            }};
        }

        // The overlay layer elements go next.
        push_popups_from_layer!(Layer::Overlay);
        push_normal_from_layer!(Layer::Overlay);

        // When rendering above the top layer, we put the regular monitor elements first.
        // Otherwise, we will render all layer-shell pop-ups and the top layer on top.
        if mon.render_above_top_layer() {
            self.render_cycler_highlight(output, push);

            self.layout
                .render_interactive_move_for_output(ctx.r(), output, &mut |elem| push(elem.into()));

            mon.render_insert_hint_between_workspaces(&mut |elem| push(elem.into()));

            {
                let mut group = Vec::new();
                mon.render_workspaces(ctx.r(), focus_ring, &mut |elem| group.push(elem.into()));
                Self::push_group_at_alpha(
                    ctx.renderer,
                    &self.picker_offscreen,
                    fade_scale,
                    picker_alpha,
                    group,
                    push,
                );
            }

            push_popups_from_layer!(Layer::Top);
            push_normal_from_layer!(Layer::Top);

            push_popups_from_layer!(Layer::Bottom);
            push_popups_from_layer!(Layer::Background);
            push_normal_from_layer!(Layer::Bottom);
            push_normal_from_layer!(Layer::Background);

            // We don't expect more than one workspace when render_above_top_layer().
            if let Some((ws, geo)) = mon.workspaces_with_render_geo().next() {
                // The GNOME wallpaper draws from an uploaded VkTexture; the solid
                // `render_background` below still backs it.
                if gnome_mode {
                    if let Some(elem) = self.wallpaper.render(
                        ctx.renderer,
                        Default::default(),
                        ws.view_size(),
                        0.,
                        output_scale,
                    ) {
                        if let Some(elem) = scale_relocate_crop(elem, output_scale, zoom, geo) {
                            push(elem.into());
                        }
                    }
                }
                push(ws.render_background().into());
            }

            // No cross-fade group on this path: it renders a single fullscreen
            // workspace above the top layer, not the overview row.
            mon.render_workspace_shadows(&mut |elem| push(elem.into()));
        } else {
            push_popups_from_layer!(Layer::Top);
            push_normal_from_layer!(Layer::Top);

            self.render_cycler_highlight(output, push);

            self.layout
                .render_interactive_move_for_output(ctx.r(), output, &mut |elem| push(elem.into()));

            mon.render_insert_hint_between_workspaces(&mut |elem| push(elem.into()));

            // The small workspace row, above the picker. It cross-fades with the search
            // results alongside the picker, but — unlike the picker — it does **not** fade
            // out as the app grid opens.
            //
            // **Divergence (approved 2026-08-03).** gnome-shell eases the thumbnails box
            // 255 -> 0 across WINDOW_PICKER -> APP_GRID (`overviewControls.js:512-548`)
            // because the app grid brings its own row: the picker shrinks into one. Here
            // the row *is* that row, drawn identically in both states, so it simply stays
            // — and the picker fades away over it instead of travelling into it. That
            // makes the show-apps transition a fade of the big previews, with the row the
            // user is pointing at never moving.
            {
                let thumbnails_alpha = row_alpha;
                let mut group = Vec::new();

                // Topmost in the group (first pushed = topmost): the close button an empty
                // workspace grows while hovered. Inside the group, so it fades with the
                // strip rather than hanging over the picker on the way out.
                let buttons: Vec<_> = mon
                    .thumbnail_close_rects()
                    .into_iter()
                    .filter(|(id, _)| self.thumbnail_hovered == Some(*id))
                    .map(|(id, rect)| ThumbnailClose {
                        rect,
                        alpha: 1.,
                        hovered: self.thumbnail_close_hovered == Some(id),
                    })
                    .collect();
                for element in self.thumbnail_chrome.render(
                    ctx.renderer,
                    &self.icon_cache,
                    fade_scale,
                    crate::ui::widget::style::accent_rgba(self.gnome_settings.accent_color),
                    &buttons,
                ) {
                    group.push(element.into());
                }

                mon.render_thumbnails(ctx.r(), Some(&self.wallpaper), &mut |elem| {
                    group.push(elem.into())
                });
                Self::push_group_at_alpha(
                    ctx.renderer,
                    &self.thumbnails_offscreen,
                    fade_scale,
                    thumbnails_alpha,
                    group,
                    push,
                );
            }

            // gnome-shell fades the whole `workspacesDisplay` out under a search
            // (`_onSearchChanged`, `overviewControls.js:628-637`), and a Workspace
            // actor owns its `WorkspaceBackground` — the rounded wallpaper and its
            // shadow — as well as the window clones. So everything that makes up the
            // row goes into ONE cross-faded group: the workspace-scoped layer-shell
            // surfaces, the window picker, the wallpaper backing each workspace, and
            // the workspace shadows. Fading only the picker (as we did) left the
            // wallpaper rectangles sitting fully opaque under the results, where GNOME
            // shows the bare `#overviewGroup` backdrop.
            //
            // One group rather than a per-element alpha, for the same reason the
            // picker alone needed it: these overlap (a layer surface over the
            // wallpaper, a preview over both), and fading them independently
            // composites the overlap twice.
            let mut group: Vec<OutputRenderElements> = Vec::new();

            // Macro instead of closure to avoid borrowing the sink. The zoom is per
            // workspace: the one the row sits on draws a touch larger than its
            // neighbors (`Monitor::workspace_render_scale`).
            macro_rules! process {
                ($ws_zoom:expr, $geo:expr) => {{
                    &mut |elem| {
                        if let Some(elem) = scale_relocate_crop(elem, output_scale, $ws_zoom, $geo)
                        {
                            group.push(elem.into());
                        }
                    }
                }};
            }

            for ((idx, ws), geo) in mon.workspaces_with_render_geo_idx() {
                let ws_zoom = zoom * mon.workspace_render_scale(idx);
                let ns = Some(ws.id().get() as usize);
                let xray_pos = XrayPos::new(geo.loc, ws_zoom);
                push_popups_from_layer!(Layer::Bottom, ns, xray_pos, process!(ws_zoom, geo));
                push_popups_from_layer!(Layer::Background, ns, xray_pos, process!(ws_zoom, geo));
            }

            // Topmost in the group: each preview's chrome — close button, caption
            // and app icon. They are children of the preview in gnome-shell (so
            // they fade with the search like everything else here), but drawn in
            // screen pixels — the workspace zoom is baked into the previews'
            // allocations there, not applied to them.
            {
                // The icon is not hover-gated, so the source is every drawn
                // preview; `preview_overlays` is the hover-gated subset, and a
                // preview missing from it simply carries alpha 0.
                let hovered: Vec<_> = mon.preview_overlays();
                let icon_scale = mon.preview_icon_scale();
                let overlays: Vec<_> = mon
                    .preview_rects()
                    .into_iter()
                    .map(|(window, preview, _)| {
                        let alpha = hovered
                            .iter()
                            .find(|(w, _, _)| *w == window)
                            .map_or(0., |(_, _, hover)| *hover);
                        let (icon, caption) = self.preview_app_chrome(&window);
                        PreviewOverlay {
                            preview,
                            alpha: alpha as f32,
                            hovered: self.preview_close_hovered.as_ref() == Some(&window),
                            icon,
                            caption,
                            icon_scale,
                        }
                    })
                    .collect();
                for element in self.preview_chrome.render(
                    ctx.renderer,
                    &self.icon_cache,
                    &self.app_icon_cache,
                    fade_scale,
                    &overlays,
                ) {
                    group.push(element.into());
                }
            }

            mon.render_workspaces(ctx.r(), focus_ring, &mut |elem| group.push(elem.into()));

            for ((idx, ws), geo) in mon.workspaces_with_render_geo_idx() {
                let ws_zoom = zoom * mon.workspace_render_scale(idx);
                // The render element namespace. This will be set to the workspace index for
                // elements duplicated across workspaces (i.e. background and bottom layers) in
                // order to have their non-xray framebuffer effects separated from each other.
                //
                // This doesn't have to correspond exactly to workspace id or idx, the only
                // requirement is that there's only one framebuffer effect element with a given id +
                // namespace on the frame at once. Id + namespace is used as the cache key in the
                // damage tracker.
                let ns = Some(ws.id().get() as usize);
                let xray_pos = XrayPos::new(geo.loc, ws_zoom);
                push_normal_from_layer!(Layer::Bottom, ns, xray_pos, process!(ws_zoom, geo));
                push_normal_from_layer!(Layer::Background, ns, xray_pos, process!(ws_zoom, geo));

                let mut wallpapered = false;
                // As above: the GNOME wallpaper draws on GLES and on the owned Vulkan renderer.
                if gnome_mode {
                    if let Some(elem) = self.wallpaper.render(
                        ctx.renderer,
                        Default::default(),
                        ws.view_size(),
                        wallpaper_radius,
                        output_scale,
                    ) {
                        process!(ws_zoom, geo)(elem);
                        wallpapered = true;
                    }
                }
                // The solid color would poke out of the wallpaper's rounded
                // corners, so it only backs workspaces without one.
                if !wallpapered {
                    process!(ws_zoom, geo)(ws.render_background());
                }
            }

            // Bottom of the group: the shadow each workspace casts on the backdrop.
            mon.render_workspace_shadows(&mut |elem| group.push(elem.into()));

            Self::push_group_at_alpha(
                ctx.renderer,
                &self.picker_offscreen,
                fade_scale,
                picker_alpha,
                group,
                push,
            );
        }

        // Then the backdrop.
        push_popups_from_layer!(Layer::Background, true);
        push_normal_from_layer!(Layer::Background, true);

        // The blurred wallpaper standing in for the flat `#overviewGroup` fill — see
        // [`OVERVIEW_BLUR_RADIUS`]. Only while the overview is up: on the desktop a workspace
        // covers the output edge to edge, so the backdrop is invisible and this would be a
        // fullscreen blur nobody can see. Pushed above the solid, which stays as the backstop for
        // "no wallpaper" and for the frames where the blur cannot be built.
        if gnome_mode {
            if let Some(progress) = mon.expose_progress() {
                if progress > 0. {
                    let radius = OVERVIEW_BLUR_RADIUS * output_scale.x;
                    if let Some(elem) = self.wallpaper.render_blurred(
                        ctx.renderer,
                        Default::default(),
                        output_size(output),
                        output_scale,
                        radius,
                        OVERVIEW_BLUR_BRIGHTNESS,
                    ) {
                        push(elem.into());
                    }
                }
            }
        }

        push(backdrop);
    }

    /// Fill the per-target background/backdrop
    /// [`EffectBuffer`](crate::render_helpers::effect_buffer::EffectBuffer)s that the
    /// [`XrayElement`](crate::render_helpers::xray::XrayElement)s sample.
    pub fn fill_xray_elements(&self, mut ctx: RenderCtx, output: &Output) {
        let _span = tracy_client::span!("Synoik::fill_xray_elements");

        // Make sure the xrayed elements themselves cannot use xray by mistake.
        ctx.xray = None;

        let state = self.output_state.get(output).unwrap();
        let xray = &state.xray;
        let layer_map = layer_map_for_output(output);
        let gnome_mode = self.config.borrow().layout.windowing_mode == WindowingMode::Floating;

        let mut buffer = xray.background[ctx.target as usize].borrow_mut();
        {
            let buf_logical = buffer.logical_size();
            let buf_scale = buffer.scale();
            let elements = buffer.elements_vulkan();
            elements.clear();
            self.render_layer_normal(
                ctx.r(),
                None,
                &layer_map,
                Layer::Background,
                XrayPos::default(),
                false,
                &mut |elem| elements.push(elem.into()),
            );
            // Bake the in-compositor GNOME wallpaper into the background buffer (through the
            // Vulkan arm's element storage) so the xray samples the wallpaper behind translucent
            // windows.
            push_gnome_wallpaper_into_xray(
                gnome_mode,
                &self.wallpaper,
                ctx.renderer,
                buf_logical,
                buf_scale,
                elements,
            );
            // Avoid unused capacity remaining forever.
            elements.shrink_to_fit();
        }

        let mut buffer = xray.backdrop[ctx.target as usize].borrow_mut();
        {
            let elements = buffer.elements_vulkan();
            elements.clear();
            self.render_layer_normal(
                ctx.r(),
                None,
                &layer_map,
                Layer::Background,
                XrayPos::default(),
                true,
                &mut |elem| elements.push(elem.into()),
            );
            // Avoid unused capacity remaining forever.
            elements.shrink_to_fit();
        }
    }

    pub fn clear_xray_elements(&self, output: &Output) {
        let state = self.output_state.get(output).unwrap();
        let xray = &state.xray;

        // Clear the xray elements for all render targets after all rendering that could use them
        // did so.
        for buf in &xray.background {
            buf.borrow_mut().elements_vulkan().clear();
        }
        for buf in &xray.backdrop {
            buf.borrow_mut().elements_vulkan().clear();
        }
    }

    /// Checks if any background layer surface has `block_out_from` set.
    pub fn has_blocked_out_background_layers(&self, output: &Output) -> bool {
        let layer_map = layer_map_for_output(output);
        for for_backdrop in [false, true] {
            for (mapped, _geo) in
                self.layers_in_render_order(&layer_map, Layer::Background, for_backdrop)
            {
                if mapped.rules().block_out_from.is_some() {
                    return true;
                }
            }
        }
        false
    }

    fn layers_in_render_order<'a>(
        &'a self,
        layer_map: &'a LayerMap,
        layer: Layer,
        for_backdrop: bool,
    ) -> impl Iterator<Item = (&'a MappedLayer, Rectangle<i32, Logical>)> {
        // LayerMap returns layers in reverse stacking order.
        layer_map.layers_on(layer).rev().filter_map(move |surface| {
            let mapped = self.mapped_layer_surfaces.get(surface)?;

            if for_backdrop != mapped.place_within_backdrop() {
                return None;
            }

            let geo = layer_map.layer_geometry(surface)?;
            Some((mapped, geo))
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn render_layer_normal(
        &self,
        mut ctx: RenderCtx,
        ns: Option<usize>,
        layer_map: &LayerMap,
        layer: Layer,
        xray_pos: XrayPos,
        for_backdrop: bool,
        push: &mut dyn FnMut(LayerSurfaceRenderElement),
    ) {
        for (mapped, geo) in self.layers_in_render_order(layer_map, layer, for_backdrop) {
            let loc = geo.loc.to_f64();
            let xray_pos = xray_pos.offset(loc);
            mapped.render_normal(ctx.r(), ns, loc, xray_pos, push);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_layer_popups(
        &self,
        mut ctx: RenderCtx,
        ns: Option<usize>,
        layer_map: &LayerMap,
        layer: Layer,
        xray_pos: XrayPos,
        for_backdrop: bool,
        push: &mut dyn FnMut(LayerSurfaceRenderElement),
    ) {
        for (mapped, geo) in self.layers_in_render_order(layer_map, layer, for_backdrop) {
            let loc = geo.loc.to_f64();
            let xray_pos = xray_pos.offset(loc);
            mapped.render_popups(ctx.r(), ns, loc, xray_pos, push);
        }
    }

    fn redraw(&mut self, backend: &mut Backend, output: &Output, aim: FrameAim) {
        let _span = tracy_client::span!("Synoik::redraw");

        // Verify our invariant.
        let state = self.output_state.get_mut(output).unwrap();
        assert!(matches!(
            state.redraw_state,
            RedrawState::Queued | RedrawState::WaitingForEstimatedVBlankAndQueued(_)
        ));

        let redraw_start = get_monotonic_time();
        let target_presentation_time = aim.target;
        let refresh_interval = state.frame_clock.refresh_interval();

        // Freeze the clock at the target time.
        self.clock.set_unadjusted(target_presentation_time);

        if let Some(scheduled_at) = aim.scheduled_at {
            self.frame_log
                .record_dispatch_lateness(redraw_start.saturating_sub(scheduled_at));
        }

        self.frame_log.begin(&output.name());
        self.frame_log.phase(Phase::Elements);
        self.update_render_elements(Some(output));

        let mut res = RenderResult::Skipped;
        if self.monitors_active {
            // Accumulated as a *named* set rather than a bare bool: the frame log records
            // it, and "animating" alone could not distinguish a workspace switch from a
            // panel button's fill fade, which is exactly the question a stutter report
            // asks. `unfinished_animations_remain` is derived from this set below, so
            // adding an animation here cannot leave the two disagreeing.
            let mut causes = self.layout.animation_causes(Some(output));
            let now_unadjusted = self.clock.now_unadjusted();
            causes.set(
                AnimCauses::DIALOG,
                self.exit_confirm_dialog.are_animations_ongoing()
                    || self.end_session_dialog.are_animations_ongoing(),
            );
            causes.set(
                AnimCauses::POLKIT,
                self.polkit_ui.are_animations_ongoing(now_unadjusted),
            );
            // The flash is fired from a D-Bus call and is usually the only thing on screen that
            // is moving, so without this it would freeze at full white until something else asked
            // for a frame.
            causes.set(
                AnimCauses::FLASHSPOT,
                self.flashspot.is_animating(now_unadjusted),
            );
            // Same for the hot-corner ripple: the overview toggle it accompanies settles well
            // before the last wave has finished expanding.
            causes.set(
                AnimCauses::RIPPLE,
                self.ripples.is_animating(now_unadjusted),
            );
            causes.set(AnimCauses::DOCK, self.dock.are_animations_ongoing());
            causes.set(
                AnimCauses::SCREENSHOT_UI,
                self.screenshot_ui.are_animations_ongoing(),
            );
            causes.set(
                AnimCauses::PANEL_POPOVER,
                self.panel_popover.are_animations_ongoing(),
            );
            causes.set(
                AnimCauses::NOTIFICATION,
                self.notification_banner.are_animations_ongoing(),
            );
            causes.set(AnimCauses::OSD, self.osd.are_animations_ongoing());
            // The switcher's sub-list fades in and out, and the next event on a switcher is
            // usually the key that ends the session — so without this the fade would only
            // advance when something else happened to force a frame.
            causes.set(AnimCauses::SWITCHER, self.switcher.are_animations_ongoing());
            causes.set(AnimCauses::PANEL, self.panel.are_animations_ongoing());
            // The dash's drop gap eases shut after a drop, with no pointer motion left
            // to generate the frames it needs.
            causes.set(AnimCauses::DASH, self.dash.are_animations_ongoing());
            causes.set(AnimCauses::APP_GRID, self.app_grid.are_animations_ongoing());
            causes.set(
                AnimCauses::FOLDER_DIALOG,
                self.folder_dialog.are_animations_ongoing(),
            );
            // The overview search cross-fade lives on `Synoik` (not the layout), so it
            // must keep the redraw loop alive here too — otherwise the fade only
            // advances when another event (e.g. pointer motion) forces a frame, and
            // the results appear stuck at a partial alpha until the mouse moves.
            causes.set(
                AnimCauses::OVERVIEW_SEARCH,
                self.overview_search_fade.is_some() || self.overview_search_expand.is_some(),
            );
            // The shield's hint fades in four seconds after the last input, and there is by
            // definition no input coming — nothing else would ask for those frames.
            //
            // The clock↔prompt crossfade is the same story: it starts on a keypress and then owes
            // 300 ms of frames nothing else will ask for. So is the curtain's own slide, which
            // additionally runs *after* the shield is gone and so has nothing else to ask at all.
            let now = crate::utils::get_monotonic_time();
            causes.set(
                AnimCauses::LOCK_SCREEN,
                self.lock_screen.is_animating(now)
                    || self.lock_screen.page_is_animating(now)
                    || self.lock_screen.is_sliding(now)
                    || self.lock_screen.is_fading(now)
                    || self.lock_screen.caps_is_animating(now)
                    || self.lock_screen.wiggle_is_animating(now),
            );

            // Also keep redrawing if the current cursor is animated.
            causes.set(
                AnimCauses::CURSOR,
                self.cursor_manager
                    .is_current_cursor_animated(output.current_scale().integer_scale()),
            );

            causes.set(
                AnimCauses::SCREEN_TRANSITION,
                self.output_state[output].screen_transition.is_some(),
            );

            // Also check layer surfaces. Still guarded on the set being otherwise empty:
            // the scan walks every layer surface on the output and the answer cannot
            // change whether we redraw, only why.
            if causes.is_empty() {
                causes.set(
                    AnimCauses::LAYER_SURFACE,
                    layer_map_for_output(output)
                        .layers()
                        .filter_map(|surface| self.mapped_layer_surfaces.get(surface))
                        .any(|mapped| mapped.are_animations_ongoing()),
                );
            }

            // Whether to keep drawing is decided by `causes` alone. The log gets one
            // extra bit that must NOT feed that decision: a workspace switch being
            // *dragged* on a touchpad queues no frames of its own — the input events
            // do — so folding it in would spin the redraw loop for as long as a
            // finger rests on the touchpad. It still needs naming, because a dragged
            // switch is exactly the interaction someone calls janky.
            let drag_switch = self
                .layout
                .monitor_for_output(output)
                .is_some_and(|mon| mon.workspace_switch_in_progress());

            let state = self.output_state.get_mut(output).unwrap();
            state.unfinished_animations_remain = !causes.is_empty();
            if drag_switch {
                causes |= AnimCauses::WORKSPACE_SWITCH;
            }
            state.last_frame_anim_causes = causes;

            // Whether *this* frame is the one a pending suspend is waiting for, decided before it
            // is handed over: `update_render_elements` above has already retired a finished slide,
            // so the curtain state read here is the one the frame draws. Asking the same question
            // again at presentation time would be an off-by-one — the curtain can land while a
            // frame from mid-slide is still in flight, and that frame is the picture that would be
            // left on screen.
            let landed = self.shield_curtain_landed();
            let state = self.output_state.get_mut(output).unwrap();
            state.shield_frame_queued = landed;

            // Render. The backend marks its own sub-phases (collect / submit /
            // queue) as it goes.
            res = backend.render(self, output, target_presentation_time);
        }

        let is_locked = self.is_locked();
        let state = self.output_state.get_mut(output).unwrap();

        // Nothing went to the display, so nothing will come back to say it arrived — and leaving
        // the mark set would let some *later* frame's vblank answer for this one.
        if !matches!(res, RenderResult::Submitted) {
            state.shield_frame_queued = false;
        }

        // What this frame cost, start of redraw to handed-to-KMS — the span the next frame's
        // target has to fit into. Measured here rather than at the end of `redraw`: the frame
        // callbacks and the screencast captures below run *after* the flip is queued, so they
        // are not part of the race against the vblank. Skipped frames drew nothing and would
        // pull the estimate down toward a cost no real frame has.
        //
        // Measured from the *deadline* when there was one, not from the start of this function:
        // a redraw released by a timer still has to wait out the rest of the loop turn, and that
        // wait comes out of the same budget. Charging it is what stops a loop that keeps missing
        // its deadline from re-arming the same too-late deadline every frame.
        if res != RenderResult::Skipped {
            let from = aim.scheduled_at.unwrap_or(redraw_start).min(redraw_start);
            state
                .frame_clock
                .record_render_time(get_monotonic_time().saturating_sub(from));
        }

        if res == RenderResult::Skipped {
            // Update the redraw state on failed render.
            state.redraw_state = if let RedrawState::WaitingForEstimatedVBlank(token)
            | RedrawState::WaitingForEstimatedVBlankAndQueued(token) =
                state.redraw_state
            {
                RedrawState::WaitingForEstimatedVBlank(token)
            } else {
                RedrawState::Idle
            };
        }

        // Update the lock render state on successful render, or if monitors are inactive. When
        // monitors are inactive on a TTY, they have no framebuffer attached, so no sensitive data
        // from a last render will be visible.
        if res != RenderResult::Skipped || !self.monitors_active {
            state.lock_render_state = if is_locked {
                LockRenderState::Locked
            } else {
                LockRenderState::Unlocked
            };
        }

        // If we're in process of locking the session, check if the requirements were met.
        match mem::take(&mut self.lock_state) {
            LockState::Locking(confirmation) => {
                if state.lock_render_state == LockRenderState::Unlocked {
                    // We needed to render a locked frame on this output but failed.
                    self.unlock();
                } else {
                    // Check if all outputs are now locked.
                    let all_locked = self
                        .output_state
                        .values()
                        .all(|state| state.lock_render_state == LockRenderState::Locked);

                    if all_locked {
                        // All outputs are locked, report success.
                        let lock = confirmation.ext_session_lock().clone();
                        confirmation.lock();
                        self.lock_state = LockState::Locked(lock);
                    } else {
                        // Still waiting for other outputs.
                        self.lock_state = LockState::Locking(confirmation);
                    }
                }
            }
            lock_state => self.lock_state = lock_state,
        }

        self.refresh_on_demand_vrr(backend, output);

        // Send the frame callbacks.
        //
        // FIXME: The logic here could be a bit smarter. Currently, during an animation, the
        // surfaces that are visible for the very last frame (e.g. because the camera is moving
        // away) will receive frame callbacks, and the surfaces that are invisible but will become
        // visible next frame will not receive frame callbacks (so they will show stale contents for
        // one frame). We could advance the animations for the next frame and send frame callbacks
        // according to the expected new positions.
        //
        // However, this should probably be restricted to sending frame callbacks to more surfaces,
        // to err on the safe side.
        self.frame_log.phase(Phase::Callbacks);
        self.send_frame_callbacks(output);

        self.frame_log.phase(Phase::Captures);
        let rendered = backend.with_vulkan_renderer(|renderer| {
            self.render_captures_with(renderer, output, target_presentation_time);
        });
        if rendered.is_none() {
            warn!("no renderer to render screencast and screencopy with");
        }

        if self.frame_log.is_enabled() {
            let state = &self.output_state[output];
            self.frame_log.set_context(FrameContext {
                elements: state.last_frame_elements,
                full_damage: state.last_frame_full_damage,
                animating: state.last_frame_anim_causes,
                overview_state: self
                    .layout
                    .monitor_for_output(output)
                    .and_then(|mon| mon.overview_state_value()),
                output_px: output.current_mode().map_or(0, |m| {
                    u64::from(m.size.w.max(0) as u32) * u64::from(m.size.h.max(0) as u32)
                }),
            });
        }
        self.frame_log.end(refresh_interval);
    }

    /// Render this output for everything that captures it as a side effect of the redraw: PipeWire
    /// screencast streams, and screencopy sessions that asked to be woken on damage.
    ///
    /// Generic over the renderer so a Vulkan session captures through the renderer that actually
    /// drew the frame. Capturing through the co-resident GLES renderer instead would quietly
    /// re-render the scene with a *different* renderer than the one on screen, so a stream could
    /// disagree with the display.
    // Two copies because the screencast arm needs an extra `CastRenderElement` bound, and `#[cfg]`
    // is not allowed on a where-clause predicate.
    #[cfg(feature = "xdp-gnome-screencast")]
    fn render_captures_with(
        &mut self,
        renderer: &mut VulkanRenderer,
        output: &Output,
        target_presentation_time: Duration,
    ) {
        // Render and send to PipeWire screencast streams.
        self.render_for_screen_cast(renderer, output, target_presentation_time);

        // FIXME: when a window is hidden, it should probably still receive frame callbacks
        // and get rendered for screen cast. This is currently
        // unimplemented, but happens to work by chance, since output
        // redrawing is more eager than it should be.
        self.render_windows_for_screen_cast(renderer, output, target_presentation_time);

        self.render_area_for_screen_cast(renderer, output, target_presentation_time);

        self.render_for_recorders(renderer, output, target_presentation_time);

        self.render_for_screencopy_with_damage(renderer, output);
    }

    #[cfg(not(feature = "xdp-gnome-screencast"))]
    fn render_captures_with(
        &mut self,
        renderer: &mut VulkanRenderer,
        output: &Output,
        _target_presentation_time: Duration,
    ) {
        self.render_for_screencopy_with_damage(renderer, output);
    }

    pub fn refresh_on_demand_vrr(&mut self, backend: &mut Backend, output: &Output) {
        let _span = tracy_client::span!("Synoik::refresh_on_demand_vrr");

        let name = output.user_data().get::<OutputName>().unwrap();
        let on_demand = self
            .config
            .borrow()
            .outputs
            .find(name)
            .is_some_and(|output| output.is_vrr_on_demand());
        if !on_demand {
            return;
        }

        let current = self.layout.windows_for_output(output).any(|mapped| {
            mapped.rules().variable_refresh_rate == Some(true) && {
                let mut visible = false;
                mapped.window.with_surfaces(|surface, states| {
                    if !visible
                        && surface_primary_scanout_output(surface, states).as_ref() == Some(output)
                    {
                        visible = true;
                    }
                });
                visible
            }
        });

        backend.set_output_on_demand_vrr(self, output, current);
    }

    pub fn update_primary_scanout_output(
        &self,
        output: &Output,
        render_element_states: &RenderElementStates,
    ) {
        // FIXME: potentially tweak the compare function. The default one currently always prefers a
        // higher refresh-rate output, which is not always desirable (i.e. with a very small
        // overlap).
        //
        // While we only have cursors and DnD icons crossing output boundaries though, it doesn't
        // matter all that much.
        if let CursorImageStatus::Surface(surface) = &self.cursor_manager.cursor_image() {
            with_surface_tree_downward(
                surface,
                (),
                |_, _, _| TraversalAction::DoChildren(()),
                |surface, states, _| {
                    update_surface_primary_scanout_output(
                        surface,
                        output,
                        states,
                        None,
                        render_element_states,
                        default_primary_scanout_output_compare,
                    );
                },
                |_, _, _| true,
            );
        }

        if let Some(surface) = self.dnd_icon.as_ref().map(|icon| &icon.surface) {
            with_surface_tree_downward(
                surface,
                (),
                |_, _, _| TraversalAction::DoChildren(()),
                |surface, states, _| {
                    update_surface_primary_scanout_output(
                        surface,
                        output,
                        states,
                        None,
                        render_element_states,
                        default_primary_scanout_output_compare,
                    );
                },
                |_, _, _| true,
            );
        }

        // We're only updating the current output's windows and layer surfaces. This should be fine
        // as in synoik they can only be rendered on a single output at a time.
        //
        // The reason to do this at all is that it keeps track of whether the surface is visible or
        // not in a unified way with the pointer surfaces, which makes the logic elsewhere simpler.

        for mapped in self.layout.windows_for_output(output) {
            let win = &mapped.window;
            let offscreen_data = mapped.offscreen_data();
            let offscreen_data = offscreen_data.as_ref();

            win.with_surfaces(|surface, states| {
                let primary_scanout_output = states
                    .data_map
                    .get_or_insert_threadsafe(Mutex::<PrimaryScanoutOutput>::default);
                let mut primary_scanout_output = primary_scanout_output.lock().unwrap();

                let mut id = Id::from_wayland_resource(surface);

                if let Some(data) = offscreen_data {
                    // We have offscreen data; it's likely that all surfaces are on it.
                    if data.states.element_was_presented(id.clone()) {
                        // If the surface was presented to the offscreen, use the offscreen's id.
                        id = data.id.clone();
                    }

                    // If we the surface wasn't presented to the offscreen it can mean:
                    //
                    // - The surface was invisible. For example, it's obscured by another surface on
                    //   the offscreen, or simply isn't mapped.
                    // - The surface is rendered separately from the offscreen, for example: popups
                    //   during the window resize animation.
                    //
                    // In both of these cases, using the original surface element id and the
                    // original states is the correct thing to do. We may find the surface in the
                    // original states (in the second case). Either way, we definitely know it is
                    // *not* in the offscreen, and we won't miss it.
                    //
                    // There's one edge case: if the surface is both in the offscreen and separate,
                    // and the offscreen itself is invisible, while the separate surface is
                    // visible. In this case we'll currently mark the surface as invisible. We
                    // don't really use offscreens like that however, and if we start, it's easy
                    // enough to fix (need an extra check).
                }

                primary_scanout_output.update_from_render_element_states(
                    id,
                    output,
                    None,
                    render_element_states,
                    |_, _, output, _| output,
                );
            });
        }

        let xray = &self.output_state[output].xray;
        let xray_bg = xray.background[RenderTarget::Output as usize].borrow();
        let xray_bd = xray.backdrop[RenderTarget::Output as usize].borrow();

        for layer in layer_map_for_output(output).layers() {
            let surface = layer.wl_surface();
            let is_background = layer.layer() == Layer::Background;

            with_surfaces_surface_tree(surface, |surface, states| {
                let primary_scanout_output = states
                    .data_map
                    .get_or_insert_threadsafe(Mutex::<PrimaryScanoutOutput>::default);
                let mut primary_scanout_output = primary_scanout_output.lock().unwrap();
                let mut id = Id::from_wayland_resource(surface);

                // Background layers may be invisible normally but visible through an xray
                // background effect. Try to find it and use the xray element's id in this case.
                //
                // FIXME: this won't work if there's another layer of offscreen (e.g. window with
                // an xray background during its opening animation). But hopefully with the
                // refactor to draw background effects outside offscreens it won't be a problem.
                if is_background && !render_element_states.element_was_presented(id.clone()) {
                    // A layer may be present either in background or backdrop, never in both.
                    if xray_bg
                        .render_element_states()
                        .is_some_and(|s| s.element_was_presented(id.clone()))
                    {
                        id = xray_bg.id().clone();
                    } else if xray_bd
                        .render_element_states()
                        .is_some_and(|s| s.element_was_presented(id.clone()))
                    {
                        id = xray_bd.id().clone();
                    }
                }

                primary_scanout_output.update_from_render_element_states(
                    id,
                    output,
                    None,
                    render_element_states,
                    // Layer surfaces are shown only on one output at a time.
                    |_, _, output, _| output,
                );
            });

            // Popups never go into xray buffers.
            for (popup, _) in PopupManager::popups_for_surface(surface) {
                let surface = popup.wl_surface();
                with_surfaces_surface_tree(surface, |surface, states| {
                    update_surface_primary_scanout_output(
                        surface,
                        output,
                        states,
                        None,
                        render_element_states,
                        // Layer surfaces are shown only on one output at a time.
                        |_, _, output, _| output,
                    );
                });
            }
        }

        if let Some(surface) = &self.output_state[output].lock_surface {
            with_surface_tree_downward(
                surface.wl_surface(),
                (),
                |_, _, _| TraversalAction::DoChildren(()),
                |surface, states, _| {
                    update_surface_primary_scanout_output(
                        surface,
                        output,
                        states,
                        None,
                        render_element_states,
                        default_primary_scanout_output_compare,
                    );
                },
                |_, _, _| true,
            );
        }
    }

    pub fn send_dmabuf_feedbacks(
        &self,
        output: &Output,
        feedback: &SurfaceDmabufFeedback,
        render_element_states: &RenderElementStates,
    ) {
        let _span = tracy_client::span!("Synoik::send_dmabuf_feedbacks");

        // We can unconditionally send the current output's feedback to regular and layer-shell
        // surfaces, as they can only be displayed on a single output at a time. Even if a surface
        // is currently invisible, this is the DMABUF feedback that it should know about.
        for mapped in self.layout.windows_for_output(output) {
            mapped.window.send_dmabuf_feedback(
                output,
                |_, _| Some(output.clone()),
                |surface, _| {
                    select_dmabuf_feedback(
                        surface,
                        render_element_states,
                        &feedback.render,
                        &feedback.scanout,
                    )
                },
            );
        }

        for surface in layer_map_for_output(output).layers() {
            surface.send_dmabuf_feedback(
                output,
                |_, _| Some(output.clone()),
                |surface, _| {
                    select_dmabuf_feedback(
                        surface,
                        render_element_states,
                        &feedback.render,
                        &feedback.scanout,
                    )
                },
            );
        }

        if let Some(surface) = &self.output_state[output].lock_surface {
            send_dmabuf_feedback_surface_tree(
                surface.wl_surface(),
                output,
                |_, _| Some(output.clone()),
                |surface, _| {
                    select_dmabuf_feedback(
                        surface,
                        render_element_states,
                        &feedback.render,
                        &feedback.scanout,
                    )
                },
            );
        }

        if let Some(surface) = self.dnd_icon.as_ref().map(|icon| &icon.surface) {
            send_dmabuf_feedback_surface_tree(
                surface,
                output,
                surface_primary_scanout_output,
                |surface, _| {
                    select_dmabuf_feedback(
                        surface,
                        render_element_states,
                        &feedback.render,
                        &feedback.scanout,
                    )
                },
            );
        }

        if let CursorImageStatus::Surface(surface) = &self.cursor_manager.cursor_image() {
            send_dmabuf_feedback_surface_tree(
                surface,
                output,
                surface_primary_scanout_output,
                |surface, _| {
                    select_dmabuf_feedback(
                        surface,
                        render_element_states,
                        &feedback.render,
                        &feedback.scanout,
                    )
                },
            );
        }
    }

    pub fn send_frame_callbacks(&mut self, output: &Output) {
        let _span = tracy_client::span!("Synoik::send_frame_callbacks");

        let state = self.output_state.get(output).unwrap();
        let sequence = state.frame_callback_sequence;

        let should_send = |surface: &WlSurface, states: &SurfaceData| {
            // Do the standard primary scanout output check. For pointer surfaces it deduplicates
            // the frame callbacks across potentially multiple outputs, and for regular windows and
            // layer-shell surfaces it avoids sending frame callbacks to invisible surfaces.
            let current_primary_output = surface_primary_scanout_output(surface, states);
            if current_primary_output.as_ref() != Some(output) {
                return None;
            }

            // Next, check the throttling status.
            let frame_throttling_state = states
                .data_map
                .get_or_insert(SurfaceFrameThrottlingState::default);
            let mut last_sent_at = frame_throttling_state.last_sent_at.borrow_mut();

            let mut send = true;

            // If we already sent a frame callback to this surface this output refresh
            // cycle, don't send one again to prevent empty-damage commit busy loops.
            if let Some((last_output, last_sequence)) = &*last_sent_at {
                if last_output == output && *last_sequence == sequence {
                    send = false;
                }
            }

            if send {
                *last_sent_at = Some((output.clone(), sequence));
                Some(output.clone())
            } else {
                None
            }
        };

        let frame_callback_time = get_monotonic_time();

        for mapped in self.layout.windows_for_output_mut(output) {
            mapped.send_frame(
                output,
                frame_callback_time,
                FRAME_CALLBACK_THROTTLE,
                should_send,
            );
        }

        for surface in layer_map_for_output(output).layers() {
            surface.send_frame(
                output,
                frame_callback_time,
                FRAME_CALLBACK_THROTTLE,
                should_send,
            );
        }

        if let Some(surface) = &self.output_state[output].lock_surface {
            send_frames_surface_tree(
                surface.wl_surface(),
                output,
                frame_callback_time,
                FRAME_CALLBACK_THROTTLE,
                should_send,
            );
        }

        if let Some(surface) = self.dnd_icon.as_ref().map(|icon| &icon.surface) {
            send_frames_surface_tree(
                surface,
                output,
                frame_callback_time,
                FRAME_CALLBACK_THROTTLE,
                should_send,
            );
        }

        if let CursorImageStatus::Surface(surface) = self.cursor_manager.cursor_image() {
            send_frames_surface_tree(
                surface,
                output,
                frame_callback_time,
                FRAME_CALLBACK_THROTTLE,
                should_send,
            );
        }
    }

    pub fn send_frame_callbacks_on_fallback_timer(&mut self) {
        let _span = tracy_client::span!("Synoik::send_frame_callbacks_on_fallback_timer");

        // Make up a bogus output; we don't care about it here anyway, just the throttling timer.
        let output = Output::new(
            String::new(),
            PhysicalProperties {
                size: Size::from((0, 0)),
                subpixel: Subpixel::Unknown,
                make: String::new(),
                model: String::new(),
                serial_number: String::new(),
            },
        );
        let output = &output;

        let frame_callback_time = get_monotonic_time();

        self.layout.with_windows_mut(|mapped, _| {
            mapped.send_frame(
                output,
                frame_callback_time,
                FRAME_CALLBACK_THROTTLE,
                |_, _| None,
            );
        });

        for (output, state) in self.output_state.iter() {
            for surface in layer_map_for_output(output).layers() {
                surface.send_frame(
                    output,
                    frame_callback_time,
                    FRAME_CALLBACK_THROTTLE,
                    |_, _| None,
                );
            }

            if let Some(surface) = &state.lock_surface {
                send_frames_surface_tree(
                    surface.wl_surface(),
                    output,
                    frame_callback_time,
                    FRAME_CALLBACK_THROTTLE,
                    |_, _| None,
                );
            }
        }

        if let Some(surface) = &self.dnd_icon.as_ref().map(|icon| &icon.surface) {
            send_frames_surface_tree(
                surface,
                output,
                frame_callback_time,
                FRAME_CALLBACK_THROTTLE,
                |_, _| None,
            );
        }

        if let CursorImageStatus::Surface(surface) = self.cursor_manager.cursor_image() {
            send_frames_surface_tree(
                surface,
                output,
                frame_callback_time,
                FRAME_CALLBACK_THROTTLE,
                |_, _| None,
            );
        }
    }

    pub fn take_presentation_feedbacks(
        &mut self,
        output: &Output,
        render_element_states: &RenderElementStates,
    ) -> OutputPresentationFeedback {
        let mut feedback = OutputPresentationFeedback::new(output);

        if let CursorImageStatus::Surface(surface) = &self.cursor_manager.cursor_image() {
            take_presentation_feedback_surface_tree(
                surface,
                &mut feedback,
                surface_primary_scanout_output,
                |surface, _| {
                    surface_presentation_feedback_flags_from_states(
                        surface,
                        None,
                        render_element_states,
                    )
                },
            );
        }

        if let Some(surface) = self.dnd_icon.as_ref().map(|icon| &icon.surface) {
            take_presentation_feedback_surface_tree(
                surface,
                &mut feedback,
                surface_primary_scanout_output,
                |surface, _| {
                    surface_presentation_feedback_flags_from_states(
                        surface,
                        None,
                        render_element_states,
                    )
                },
            );
        }

        for mapped in self.layout.windows_for_output(output) {
            mapped.window.take_presentation_feedback(
                &mut feedback,
                surface_primary_scanout_output,
                |surface, _| {
                    surface_presentation_feedback_flags_from_states(
                        surface,
                        None,
                        render_element_states,
                    )
                },
            )
        }

        for surface in layer_map_for_output(output).layers() {
            surface.take_presentation_feedback(
                &mut feedback,
                surface_primary_scanout_output,
                |surface, _| {
                    surface_presentation_feedback_flags_from_states(
                        surface,
                        None,
                        render_element_states,
                    )
                },
            );
        }

        if let Some(surface) = &self.output_state[output].lock_surface {
            take_presentation_feedback_surface_tree(
                surface.wl_surface(),
                &mut feedback,
                surface_primary_scanout_output,
                |surface, _| {
                    surface_presentation_feedback_flags_from_states(
                        surface,
                        None,
                        render_element_states,
                    )
                },
            );
        }

        feedback
    }

    pub fn render_for_screencopy_with_damage(
        &mut self,
        renderer: &mut VulkanRenderer,
        output: &Output,
    ) {
        let _span = tracy_client::span!("Synoik::render_for_screencopy_with_damage");
        let appearance = self.appearance();

        let mut screencopy_state = mem::take(&mut self.screencopy_state);

        screencopy_state.with_queues_mut(|queue| {
            let (damage_tracker, screencopy) = queue.split();
            if let Some(screencopy) = screencopy {
                if screencopy.output() == output {
                    let ctx = RenderCtx {
                        renderer,
                        target: RenderTarget::ScreenCapture,
                        xray: None,
                        appearance: Some(appearance),
                    };
                    let offset = screencopy.region_loc().upscale(-1);
                    let mut elements = Vec::new();
                    self.render(ctx, output, screencopy.overlay_cursor(), &mut |elem| {
                        let elem =
                            RelocateRenderElement::from_element(elem, offset, Relocate::Relative);
                        elements.push(elem);
                    });

                    let (damages, states) = Self::damage_screencopy_internal(
                        output,
                        &elements,
                        damage_tracker,
                        screencopy,
                    );
                    if let Some(damages) = damages {
                        // Convert from Physical coordinates back to Buffer coordinates.
                        let transform = output.current_transform();
                        let physical_size = transform.transform_size(screencopy.buffer_size());
                        let damages = damages.iter().map(|dmg| {
                            dmg.to_logical(1).to_buffer(
                                1,
                                transform.invert(),
                                &physical_size.to_logical(1),
                            )
                        });

                        screencopy.damage(damages);

                        let render_result = Self::render_for_screencopy_internal(
                            renderer,
                            damage_tracker,
                            &elements,
                            states,
                            screencopy,
                        );
                        match render_result {
                            Ok(sync) => {
                                queue.pop().submit_after_sync(false, sync, &self.event_loop);
                            }
                            Err(err) => {
                                // Recreate damage tracker to report full damage next check.
                                *damage_tracker =
                                    OutputDamageTracker::new((0, 0), 1.0, Transform::Normal);
                                queue.pop();
                                warn!("error rendering for screencopy: {err:?}");
                            }
                        }
                    } else {
                        trace!("no damage found, waiting till next redraw");
                    }
                };
            }
        });

        self.screencopy_state = screencopy_state;
    }

    pub fn render_for_screencopy_without_damage(
        &mut self,
        renderer: &mut VulkanRenderer,
        manager: &ZwlrScreencopyManagerV1,
        screencopy: Screencopy,
    ) -> anyhow::Result<()> {
        let _span = tracy_client::span!("Synoik::render_for_screencopy");

        let output = screencopy.output();
        ensure!(
            self.output_state.contains_key(output),
            "screencopy output missing"
        );

        self.update_render_elements(Some(output));

        let ctx = RenderCtx {
            renderer,
            target: RenderTarget::ScreenCapture,
            xray: None,
            appearance: Some(self.appearance()),
        };
        let offset = screencopy.region_loc().upscale(-1);
        let mut elements = Vec::new();
        self.render(ctx, output, screencopy.overlay_cursor(), &mut |elem| {
            let elem = RelocateRenderElement::from_element(elem, offset, Relocate::Relative);
            elements.push(elem);
        });

        let Some(damage_tracker) = self.screencopy_state.damage_tracker(manager) else {
            error!("screencopy queue must not be deleted as long as frames exist");
            bail!("screencopy queue missing");
        };

        let (_damages, states) =
            Self::damage_screencopy_internal(output, &elements, damage_tracker, &screencopy);
        let res = Self::render_for_screencopy_internal(
            renderer,
            damage_tracker,
            &elements,
            states,
            &screencopy,
        );
        let res = res.map(|sync| screencopy.submit_after_sync(false, sync, &self.event_loop));

        if res.is_err() {
            // Recreate damage tracker to report full damage next check.
            *damage_tracker = OutputDamageTracker::new((0, 0), 1.0, Transform::Normal);
        }

        res
    }

    fn damage_screencopy_internal<'a>(
        output: &Output,
        elements: &[impl Element],
        damage_tracker: &'a mut OutputDamageTracker,
        screencopy: &Screencopy,
    ) -> (
        Option<&'a Vec<Rectangle<i32, Physical>>>,
        RenderElementStates,
    ) {
        let OutputModeSource::Static {
            size: last_size,
            scale: last_scale,
            transform: last_transform,
        } = damage_tracker.mode().clone()
        else {
            unreachable!("damage tracker must have static mode");
        };

        let size = screencopy.buffer_size();
        let scale: Scale<f64> = output.current_scale().fractional_scale().into();
        let transform = output.current_transform();

        if size != last_size || scale != last_scale || transform != last_transform {
            *damage_tracker = OutputDamageTracker::new(size, scale, transform);
        }

        // Just checked damage tracker has static mode
        damage_tracker.damage_output(1, elements).unwrap()
    }

    #[allow(clippy::type_complexity)]
    fn render_for_screencopy_internal(
        renderer: &mut VulkanRenderer,
        damage_tracker: &mut OutputDamageTracker,
        elements: &[impl RenderElement<VulkanRenderer>],
        states: RenderElementStates,
        screencopy: &Screencopy,
    ) -> anyhow::Result<Option<SyncPoint>> {
        let sync = match screencopy.buffer() {
            ScreencopyBuffer::Dmabuf(dmabuf) => {
                let sync =
                    render_to_dmabuf(renderer, damage_tracker, dmabuf.clone(), elements, states)
                        .context("error rendering to screencopy dmabuf")?;
                Some(sync)
            }
            ScreencopyBuffer::Shm(wl_buffer) => {
                render_to_shm(renderer, damage_tracker, wl_buffer, elements)
                    .context("error rendering to screencopy shm buffer")?;
                None
            }
        };

        Ok(sync)
    }

    #[cfg(not(feature = "xdp-gnome-screencast"))]
    pub fn stop_casts_for_target(&mut self, _target: CastTarget) {}

    #[cfg(not(feature = "xdp-gnome-screencast"))]
    pub fn stop_cast(&mut self, _session_id: crate::utils::CastSessionId) {}

    // The native recorder is our own — capture, the encoder seam, ffmpeg — but it lives in
    // `screencasting`, so it is gated behind the *portal* feature it otherwise has nothing to do
    // with. These three stubs are what the two above already had; without them the no-portal build
    // does not compile, which is exactly how it rotted unnoticed. Giving the recorder its own
    // feature (or none) is the real fix and belongs with the recorder, not here.
    #[cfg(not(feature = "xdp-gnome-screencast"))]
    pub fn start_native_recording(
        &mut self,
        _output: &Output,
        _path: std::path::PathBuf,
        _framerate: u32,
        _draw_cursor: bool,
        _crop: Option<Rectangle<i32, Logical>>,
    ) -> anyhow::Result<()> {
        anyhow::bail!("this build has no recorder (xdp-gnome-screencast is off)")
    }

    #[cfg(not(feature = "xdp-gnome-screencast"))]
    pub fn stop_screen_recordings(&mut self) -> Vec<std::path::PathBuf> {
        Vec::new()
    }

    #[cfg(not(feature = "xdp-gnome-screencast"))]
    pub fn stop_native_recordings_for_output(&mut self, _output: &Output) {}

    pub fn debug_toggle_damage(&mut self) {
        self.debug_draw_damage = !self.debug_draw_damage;

        if self.debug_draw_damage {
            for (output, state) in &mut self.output_state {
                state.debug_damage_tracker = OutputDamageTracker::from_output(output);
            }
        }

        self.queue_redraw_all();
    }

    /// Vulkan pass for [`Self::capture_screenshots`]: capture each output's screen + pointer
    /// neutrals through the owned Vulkan renderer, one per render target, keeping
    /// `capture_screenshots`' render convention (transform folded into the size, render transform
    /// `Normal`). `capture_screenshots` consumes the map, and on a Vulkan session it is the *only*
    /// source of the frozen screen — an output missing any target's capture is dropped there rather
    /// than baked through GLES. Assumes render elements are already up to date (the caller primes
    /// them before both passes).
    pub fn capture_screenshot_neutrals(
        &self,
        renderer: &mut crate::render_helpers::vulkan::VulkanRenderer,
    ) -> std::collections::HashMap<Output, [ScreenshotNeutral; RenderTarget::COUNT]> {
        let _span = tracy_client::span!("Synoik::capture_screenshot_neutrals");
        self.global_space
            .outputs()
            .cloned()
            .map(|output| {
                // The pointer is the same on every target — a cursor is never blocked out — so
                // capture it once. `MemoryBuffer` is `Arc`-shared, so the clones are cheap.
                let pointer = self.capture_screenshot_pointer_neutral(renderer, &output);

                // One capture per target, not just `Output`: the frozen screen is drawn into
                // screencasts and screen captures too, and those targets differ (block-out rules).
                let neutrals = ALL_RENDER_TARGETS.map(|target| ScreenshotNeutral {
                    screen: self.capture_screenshot_screen_neutral(renderer, &output, target),
                    pointer: pointer.clone(),
                });

                (output, neutrals)
            })
            .collect()
    }

    /// Freeze every window on every output's active workspace, for the picker's Window mode.
    ///
    /// GNOME captures each window's content **at open** and selects from those frozen copies
    /// (`UIWindowSelector.capture`, `js/ui/screenshot.js:1062-1094`) — so what you pick is what you
    /// saw when you opened the picker, not whatever the window has drawn since. Captured through
    /// `RenderTarget::ScreenCapture`, which is what applies the block-out rules: a window that must
    /// not be captured must not become visible in a selector either.
    pub fn capture_screenshot_window_neutrals(
        &self,
        renderer: &mut crate::render_helpers::vulkan::VulkanRenderer,
    ) -> std::collections::HashMap<Output, Vec<crate::ui::screenshot_ui::WindowShot>> {
        let _span = tracy_client::span!("Synoik::capture_screenshot_window_neutrals");

        self.global_space
            .outputs()
            .cloned()
            .map(|output| {
                let scale = output.current_scale().fractional_scale();
                let shots = self
                    .layout
                    .active_workspace_windows_for_output(&output)
                    .into_iter()
                    .filter_map(|(mapped, rect)| {
                        let id = mapped.id().get();
                        // No pointer in the capture: it is composited at save time from the
                        // output's own pointer neutral, so the show-pointer toggle keeps working
                        // after the freeze instead of being baked in here.
                        let (size, pixels) = self
                            .render_window_to_pixels(renderer, &output, mapped, false)
                            .map_err(|err| {
                                warn!("error capturing window {id} for the picker: {err:?}")
                            })
                            .ok()?;
                        let neutral = MemoryBuffer::new(
                            pixels,
                            Fourcc::Abgr8888,
                            Size::from((size.w, size.h)),
                            Scale::from(scale),
                            Transform::Normal,
                        );
                        Some(crate::ui::screenshot_ui::WindowShot::new(id, rect, neutral))
                    })
                    .collect();

                (output, shots)
            })
            .collect()
    }

    /// The screen half of [`Self::capture_screenshot_neutrals`], for one render target.
    fn capture_screenshot_screen_neutral(
        &self,
        renderer: &mut crate::render_helpers::vulkan::VulkanRenderer,
        output: &Output,
        target: RenderTarget,
    ) -> Option<MemoryBuffer> {
        let size = output.current_mode().unwrap().size;
        let size = output.current_transform().transform_size(size);
        let scale = Scale::from(output.current_scale().fractional_scale());

        let ctx = RenderCtx {
            renderer,
            target,
            xray: None,
            appearance: Some(self.appearance()),
        };
        let elements = self.render_to_vec(ctx, output, false);
        let elements = elements.iter().rev();
        match render_to_vec(
            renderer,
            size,
            scale,
            Transform::Normal,
            Fourcc::Abgr8888,
            elements,
        ) {
            Ok(data) => {
                let buffer_size = size.to_logical(1).to_buffer(1, Transform::Normal);
                Some(MemoryBuffer::new(
                    data,
                    Fourcc::Abgr8888,
                    buffer_size,
                    scale,
                    Transform::Normal,
                ))
            }
            Err(err) => {
                warn!(
                    "error capturing {target:?} screenshot neutral for {}: {err:?}",
                    output.name()
                );
                None
            }
        }
    }

    /// The pointer half of [`Self::capture_screenshot_neutrals`]: mirrors
    /// `render_to_encompassing_texture` + `read_texture_to_memory` for the cursor, through Vulkan.
    fn capture_screenshot_pointer_neutral(
        &self,
        renderer: &mut crate::render_helpers::vulkan::VulkanRenderer,
        output: &Output,
    ) -> Option<(MemoryBuffer, Point<f64, Logical>)> {
        let scale = Scale::from(output.current_scale().fractional_scale());

        let mut pointer = Vec::new();
        if self.pointer_visibility != PointerVisibility::Disabled {
            self.render_pointer(renderer, output, &mut |elem| pointer.push(elem));
        }
        if pointer.is_empty() {
            return None;
        }

        let geo = encompassing_geo(scale, pointer.iter());
        if geo.size.is_empty() {
            return None;
        }
        let relocated = pointer.iter().rev().map(|ele| {
            RelocateRenderElement::from_element(ele, geo.loc.upscale(-1), Relocate::Relative)
        });
        match render_to_vec(
            renderer,
            geo.size,
            scale,
            Transform::Normal,
            Fourcc::Abgr8888,
            relocated,
        ) {
            Ok(data) => {
                let buffer_size = geo.size.to_logical(1).to_buffer(1, Transform::Normal);
                let mb = MemoryBuffer::new(
                    data,
                    Fourcc::Abgr8888,
                    buffer_size,
                    scale,
                    Transform::Normal,
                );
                let loc = geo.to_f64().to_logical(scale).loc;
                Some((mb, loc))
            }
            Err(err) => {
                warn!(
                    "error capturing screenshot pointer neutral for {}: {err:?}",
                    output.name()
                );
                None
            }
        }
    }

    /// The frozen screen was already captured into renderer-neutral buffers through the owned
    /// renderer (`capture_screenshot_neutrals`); this just packages them per output.
    pub fn capture_screenshots<'a>(
        &'a self,
        // The per-target neutrals captured up front, keyed by output. Consumed here.
        mut vk_neutrals: std::collections::HashMap<
            Output,
            [ScreenshotNeutral; RenderTarget::COUNT],
        >,
    ) -> impl Iterator<Item = (Output, [OutputScreenshot; 3])> + 'a {
        self.global_space
            .outputs()
            .cloned()
            .filter_map(move |output| {
                // Take this output's neutrals, one per render target.
                let mut vk_neutral = vk_neutrals.remove(&output).unwrap_or_default();

                // An output missing any target's capture is dropped rather than opened with a
                // screen it cannot draw or save (the failure warned there).
                let screenshot = ALL_RENDER_TARGETS.map(|target| {
                    let this = &mut vk_neutral[target as usize];
                    let screen = this.screen.take()?;
                    Some(OutputScreenshot::from_neutrals(screen, this.pointer.take()))
                });

                if screenshot.iter().any(Option::is_none) {
                    warn!(
                        "no screenshot capture for output {}; skipping it",
                        output.name()
                    );
                    return None;
                }

                Some((output, screenshot.map(Option::unwrap)))
            })
    }

    pub fn screenshot(
        &mut self,
        renderer: &mut VulkanRenderer,
        output: &Output,
        write_to_disk: bool,
        include_pointer: bool,
        path: Option<String>,
    ) -> anyhow::Result<()> {
        let _span = tracy_client::span!("Synoik::screenshot");

        self.update_render_elements(Some(output));

        let size = output.current_mode().unwrap().size;
        let transform = output.current_transform();
        let size = transform.transform_size(size);

        let scale = Scale::from(output.current_scale().fractional_scale());
        let ctx = RenderCtx {
            renderer,
            target: RenderTarget::ScreenCapture,
            xray: None,
            appearance: Some(self.appearance()),
        };
        let elements = self.render_to_vec(ctx, output, include_pointer);
        let elements = elements.iter().rev();
        let pixels = render_to_vec(
            renderer,
            size,
            scale,
            Transform::Normal,
            Fourcc::Abgr8888,
            elements,
        )?;

        self.save_screenshot(size, pixels, write_to_disk, path, None)
            .context("error saving screenshot")
    }

    #[allow(clippy::too_many_arguments)]
    /// Capture an output-local **physical** rect of the live screen, saving it the way a keybind
    /// screenshot is saved (clipboard + notification + any waiting D-Bus caller).
    ///
    /// The whole output is rendered and then cropped on the CPU rather than rendered scissored: the
    /// crop is a memcpy over pixels we already have, and it keeps this on exactly the same
    /// block-out path as [`Self::screenshot`].
    pub fn screenshot_area(
        &mut self,
        renderer: &mut VulkanRenderer,
        output: &Output,
        rect: Rectangle<i32, Physical>,
        write_to_disk: bool,
        include_pointer: bool,
        path: Option<String>,
        reply: Option<crate::dbus::gnome_shell_screenshot::InteractiveReply>,
    ) -> anyhow::Result<()> {
        let _span = tracy_client::span!("Synoik::screenshot_area");

        self.update_render_elements(Some(output));

        let size = output.current_mode().unwrap().size;
        let transform = output.current_transform();
        let size = transform.transform_size(size);

        let scale = Scale::from(output.current_scale().fractional_scale());
        let ctx = RenderCtx {
            renderer,
            target: RenderTarget::ScreenCapture,
            xray: None,
            appearance: Some(self.appearance()),
        };
        let elements = self.render_to_vec(ctx, output, include_pointer);
        let elements = elements.iter().rev();
        let pixels = render_to_vec(
            renderer,
            size,
            scale,
            Transform::Normal,
            Fourcc::Abgr8888,
            elements,
        )?;

        // Clamped, not trusted: the rect was chosen against the frozen screen, and a mode change
        // during the delay can leave it hanging off the edge.
        let rect = rect
            .intersection(Rectangle::from_size(size))
            .context("the capture area is off-screen")?;
        let pixels =
            crate::ui::screenshot_ui::crop_rgba(Size::from((size.w, size.h)), &pixels, rect);

        self.save_screenshot(rect.size, pixels, write_to_disk, path, reply)
            .context("error saving screenshot")
    }

    #[allow(clippy::too_many_arguments)]
    pub fn screenshot_window(
        &self,
        renderer: &mut VulkanRenderer,
        output: &Output,
        mapped: &Mapped,
        write_to_disk: bool,
        show_pointer: bool,
        path: Option<String>,
        reply: Option<crate::dbus::gnome_shell_screenshot::InteractiveReply>,
    ) -> anyhow::Result<()> {
        let (size, pixels) =
            self.render_window_to_pixels(renderer, output, mapped, show_pointer)?;
        self.save_screenshot(size, pixels, write_to_disk, path, reply)
            .context("error saving screenshot")
    }

    /// `ScreenshotWindow` — writes the file and nothing else.
    ///
    /// Deliberately *not* built on `save_screenshot`, which also puts the image on the clipboard
    /// and raises a notification. Those belong to the user pressing a key, not to a portal call the
    /// user never sees; a screenshot API that silently replaced the clipboard would be a surprise
    /// with a privacy edge to it.
    pub fn screenshot_window_to_path(
        &self,
        renderer: &mut VulkanRenderer,
        output: &Output,
        mapped: &Mapped,
        show_pointer: bool,
        path: Option<PathBuf>,
        on_done: impl FnOnce(PathBuf) + Send + 'static,
    ) -> anyhow::Result<()> {
        let (size, pixels) =
            self.render_window_to_pixels(renderer, output, mapped, show_pointer)?;
        let path = path
            .or_else(|| make_screenshot_path(&self.config.borrow()).ok().flatten())
            .context("no path to save the screenshot to")?;
        write_png_in_thread(size, pixels, path, on_done);
        Ok(())
    }

    fn render_window_to_pixels(
        &self,
        renderer: &mut VulkanRenderer,
        output: &Output,
        mapped: &Mapped,
        show_pointer: bool,
    ) -> anyhow::Result<(Size<i32, Physical>, Vec<u8>)> {
        let _span = tracy_client::span!("Synoik::screenshot_window");

        let scale = Scale::from(output.current_scale().fractional_scale());
        let alpha =
            if mapped.sizing_mode().is_fullscreen() || mapped.is_ignoring_opacity_window_rule() {
                1.
            } else {
                mapped.rules().opacity.unwrap_or(1.).clamp(0., 1.)
            };

        let mut elements: Vec<WindowScreenshotRenderElement> = Vec::new();

        // Add pointer if requested and it's over this window.
        if show_pointer {
            if let Some((_, win_pos)) = self.pointer_pos_for_window_cast(mapped) {
                // Pointer elements are at output-local physical coords.
                // Relocate by -win_pos to make them window-relative.
                let pos = win_pos.to_physical_precise_round(scale).upscale(-1);
                self.render_pointer(renderer, output, &mut |elem| {
                    let elem = RelocateRenderElement::from_element(elem, pos, Relocate::Relative);
                    elements.push(elem.into());
                });
            }
        }
        let pointer_count = elements.len();

        let ctx = RenderCtx {
            renderer,
            target: RenderTarget::ScreenCapture,
            xray: None,
            appearance: Some(self.appearance()),
        };
        mapped.render(
            ctx,
            mapped.window.geometry().loc.to_f64(),
            scale,
            alpha,
            XrayPos::default(),
            &mut |elem| elements.push(elem.into()),
        );

        // The pointer is not included in encompassing_geo because we don't want it to expand the
        // screenshot size.
        let geo = encompassing_geo(scale, elements.iter().skip(pointer_count));
        let elements = elements.iter().rev().map(|elem| {
            RelocateRenderElement::from_element(elem, geo.loc.upscale(-1), Relocate::Relative)
        });
        let pixels = render_to_vec(
            renderer,
            geo.size,
            scale,
            Transform::Normal,
            Fourcc::Abgr8888,
            elements,
        )?;

        Ok((geo.size, pixels))
    }

    /// `interactive_reply` is threaded through rather than read off `self` in the completion
    /// callback: the caller closes the picker as soon as this returns, and the close answers a
    /// pending reply as a dismissal. Since the encode happens on a thread, the close would always
    /// win the race and every interactive screenshot would report as cancelled.
    pub fn save_screenshot(
        &self,
        size: Size<i32, Physical>,
        pixels: Vec<u8>,
        write_to_disk: bool,
        path_arg: Option<String>,
        interactive_reply: Option<crate::dbus::gnome_shell_screenshot::InteractiveReply>,
    ) -> anyhow::Result<()> {
        let path = write_to_disk
            .then(|| {
                // When given an explicit path, don't try to strftime it or create parents.
                path_arg.map(|p| (PathBuf::from(p), false)).or_else(|| {
                    match make_screenshot_path(&self.config.borrow()) {
                        Ok(path) => path.map(|p| (p, true)),
                        Err(err) => {
                            warn!("error making screenshot path: {err:?}");
                            None
                        }
                    }
                })
            })
            .flatten();

        // Prepare to set the encoded image as our clipboard selection. This must be done from the
        // main thread.
        let (tx, rx) = calloop::channel::sync_channel::<Arc<[u8]>>(1);
        self.event_loop
            .insert_source(rx, move |event, _, state| match event {
                calloop::channel::Event::Msg(buf) => {
                    state
                        .synoik
                        .set_clipboard(vec![String::from("image/png")], buf.clone());
                }
                calloop::channel::Event::Closed => (),
            })
            .unwrap();

        // Prepare to send screenshot completion event back to main thread.
        let mut interactive_reply = interactive_reply;
        // The path plus the shot itself, already downscaled to a notification icon. Built on the
        // encoding thread, which is holding the raw pixels anyway: making the main loop re-read and
        // re-decode the PNG it just wrote would be a full-screen image decode on the frame path.
        let (event_tx, event_rx) =
            calloop::channel::sync_channel::<(Option<String>, Option<Arc<PixelIcon>>)>(1);
        self.event_loop
            .insert_source(event_rx, move |event, _, state| match event {
                calloop::channel::Event::Msg((path, thumbnail)) => {
                    if let Some(tx) = interactive_reply.take() {
                        let _ = tx.send_blocking(path.as_deref().map(|p| format!("file://{p}")));
                    }
                    // Posted here rather than from the encoding thread, and straight into the
                    // store rather than back through `org.freedesktop.Notifications`: we ARE the
                    // notification server, and a notification we send ourselves over the bus comes
                    // back owned by a connection that is already gone — so its buttons would have
                    // nowhere to route to. gnome-shell builds this one in-process for the same
                    // reason (`js/ui/screenshot.js:2386-2420`).
                    state.show_screenshot_notification(
                        path.as_deref().map(PathBuf::from),
                        thumbnail,
                    );
                    state.ipc_screenshot_taken(path);
                }
                calloop::channel::Event::Closed => (),
            })
            .unwrap();

        // Encode and save the image in a thread as it's slow.
        thread::spawn(move || {
            let mut buf = vec![];

            let w = std::io::Cursor::new(&mut buf);
            if let Err(err) = write_png_rgba8(w, size.w as u32, size.h as u32, &pixels) {
                warn!("error encoding screenshot image: {err:?}");
                return;
            }

            let buf: Arc<[u8]> = Arc::from(buf.into_boxed_slice());
            let _ = tx.send(buf.clone());

            // The notification's image. `pixels` is the capture in RGBA already, so this is a
            // downscale and nothing else — no second decode of the PNG above.
            let thumbnail = Some(bounded_pixels(PixelIcon {
                width: size.w.max(0) as u32,
                height: size.h.max(0) as u32,
                rgba: pixels,
            }));

            let mut image_path = None;

            if let Some((path, create_parent)) = path {
                debug!("saving screenshot to {path:?}");

                if create_parent {
                    if let Some(parent) = path.parent() {
                        // Relative paths with one component, i.e. "test.png", have Some("") parent.
                        if !parent.as_os_str().is_empty() {
                            if let Err(err) = std::fs::create_dir_all(parent) {
                                if err.kind() != std::io::ErrorKind::AlreadyExists {
                                    warn!("error creating screenshot directory: {err:?}");
                                }
                            }
                        }
                    }
                }

                match std::fs::write(&path, buf) {
                    Ok(()) => image_path = Some(path),
                    Err(err) => {
                        warn!("error saving screenshot image: {err:?}");
                    }
                }
            } else {
                debug!("not saving screenshot to disk");
            }

            // Send screenshot completion event.
            let path_string = image_path
                .as_ref()
                .and_then(|p| p.to_str())
                .map(|s| s.to_owned());
            let _ = event_tx.send((path_string, thumbnail));
        });

        Ok(())
    }

    pub fn screenshot_all_outputs(
        &mut self,
        renderer: &mut VulkanRenderer,
        include_pointer: bool,
        on_done: impl FnOnce(PathBuf) + Send + 'static,
    ) -> anyhow::Result<()> {
        self.screenshot_to_path(renderer, include_pointer, None, None, on_done)
    }

    /// Capture the screen, optionally cropped to `area` (global logical coordinates), to `path`.
    ///
    /// This is what `org.gnome.Shell.Screenshot`'s `Screenshot` and `ScreenshotArea` are both made
    /// of. `on_done` fires from the encoding thread *after* the file is written — the D-Bus reply
    /// carries the filename, and a caller that gets it before the bytes are on disk is a portal
    /// reading an empty file.
    pub fn screenshot_to_path(
        &mut self,
        renderer: &mut VulkanRenderer,
        include_pointer: bool,
        area: Option<Rectangle<i32, Logical>>,
        path: Option<PathBuf>,
        on_done: impl FnOnce(PathBuf) + Send + 'static,
    ) -> anyhow::Result<()> {
        let _span = tracy_client::span!("Synoik::screenshot_to_path");

        self.update_render_elements(None);

        let outputs: Vec<_> = self.global_space.outputs().cloned().collect();

        // FIXME: support multiple outputs, needs fixing multi-scale handling and cropping.
        anyhow::ensure!(outputs.len() == 1);

        let output = outputs.into_iter().next().unwrap();
        let geom = self.global_space.output_geometry(&output).unwrap();

        let output_scale = output.current_scale().integer_scale();
        // The crop, in the same output-local physical pixels the readback is in.
        let crop = area
            .map(|area| {
                let local = Rectangle::new(area.loc - geom.loc, area.size);
                let local = local.to_physical(output_scale);
                anyhow::ensure!(
                    local.size.w > 0 && local.size.h > 0,
                    "empty screenshot area"
                );
                Ok(local)
            })
            .transpose()?;
        let geom = geom.to_physical(output_scale);

        let size = geom.size;
        let transform = output.current_transform();
        let size = transform.transform_size(size);

        let ctx = RenderCtx {
            renderer,
            target: RenderTarget::ScreenCapture,
            xray: None,
            appearance: Some(self.appearance()),
        };
        let elements = self.render_to_vec(ctx, &output, include_pointer);
        let elements = elements.iter().rev();
        let pixels = render_to_vec(
            renderer,
            size,
            Scale::from(f64::from(output_scale)),
            Transform::Normal,
            Fourcc::Abgr8888,
            elements,
        )?;

        // Crop before encoding: the readback is one full output, and the requested area is a
        // sub-rectangle of it clamped to what actually exists.
        let (size, pixels) = match crop {
            Some(crop) => crop_rgba8(size, &pixels, crop)?,
            None => (size, pixels),
        };

        let path = path
            .or_else(|| make_screenshot_path(&self.config.borrow()).ok().flatten())
            .unwrap_or_else(|| {
                let mut path = env::temp_dir();
                path.push("screenshot.png");
                path
            });
        write_png_in_thread(size, pixels, path, on_done);

        Ok(())
    }

    /// Close the picker, answering any pending `SelectArea` caller.
    ///
    /// Every close goes through here rather than through `ScreenshotUi::close` directly: the
    /// picker can be dismissed from a keybind, a lock, an output change or a session end, and a
    /// `SelectArea` caller that is not answered on *all* of those blocks until its D-Bus timeout.
    pub fn close_screenshot_ui(&mut self) -> bool {
        let was_open = self.screenshot_ui.close();
        // Unconditional, not gated on `was_open`: a pending reply with no picker on screen is
        // precisely the stuck state this exists to prevent, so closing always resolves it.
        self.answer_select_area(None);
        self.answer_interactive_screenshot(None);
        was_open
    }

    /// Hand a `SelectArea` caller its result, once.
    pub fn answer_select_area(&mut self, rect: Option<Rectangle<i32, Logical>>) {
        if let Some(tx) = self.select_area_reply.take() {
            // The channel is bounded(1) and used once, so this cannot block.
            let _ = tx.send_blocking(rect.map(|r| (r.loc.x, r.loc.y, r.size.w, r.size.h)));
        }
    }

    /// Hand an `InteractiveScreenshot` caller its result, once. `None` is a dismissal.
    pub fn answer_interactive_screenshot(&mut self, path: Option<&str>) {
        if let Some(tx) = self.interactive_screenshot_reply.take() {
            // GNOME returns `file.get_uri()`, not a path.
            let uri = path.map(|p| format!("file://{p}"));
            let _ = tx.send_blocking(uri);
        }
    }

    pub fn is_locked(&self) -> bool {
        match self.lock_state {
            LockState::Unlocked | LockState::WaitingForSurfaces { .. } => false,
            LockState::Locking(_) | LockState::Locked(_) => true,
        }
    }

    pub fn lock(&mut self, confirmation: SessionLocker) {
        // Check if another client is in the process of locking.
        if matches!(
            self.lock_state,
            LockState::WaitingForSurfaces { .. } | LockState::Locking(_)
        ) {
            info!("refusing lock as another client is currently locking");
            return;
        }

        // Check if we're already locked with an active client.
        if let LockState::Locked(lock) = &self.lock_state {
            if lock.is_alive() {
                info!("refusing lock as already locked with an active client");
                return;
            }

            // If the client had died, continue with the new lock.
            info!("locking session (replacing existing dead lock)");

            // Since the session was already locked, we know that the outputs are blanked, and
            // can lock right away.
            let lock = confirmation.ext_session_lock().clone();
            confirmation.lock();
            self.lock_state = LockState::Locked(lock);

            return;
        }

        info!("locking session");

        if self.output_state.is_empty() {
            // There are no outputs, lock the session right away.
            self.close_screenshot_ui();
            self.cursor_manager
                .set_cursor_image(CursorImageStatus::default_named());

            let lock = confirmation.ext_session_lock().clone();
            confirmation.lock();
            self.lock_state = LockState::Locked(lock);
        } else {
            // There are outputs which we need to redraw before locking. But before we do that,
            // let's wait for the lock surfaces.
            //
            // Give them a second; swaylock can take its time to paint a big enough image.
            let timer = Timer::from_duration(Duration::from_millis(1000));
            let deadline_token = self
                .event_loop
                .insert_source(timer, |_, _, state| {
                    trace!("lock deadline expired, continuing");
                    state.synoik.continue_to_locking();
                    TimeoutAction::Drop
                })
                .unwrap();

            self.lock_state = LockState::WaitingForSurfaces {
                confirmation,
                deadline_token,
            };
        }
    }

    pub fn maybe_continue_to_locking(&mut self) {
        if !matches!(self.lock_state, LockState::WaitingForSurfaces { .. }) {
            // Not waiting.
            return;
        }

        // Check if there are any outputs whose lock surfaces had not had a commit yet.
        for state in self.output_state.values() {
            let Some(surface) = &state.lock_surface else {
                // Surface not created yet.
                return;
            };

            if !is_mapped(surface.wl_surface()) {
                return;
            }
        }

        // All good.
        trace!("lock surfaces are ready, continuing");
        self.continue_to_locking();
    }

    fn continue_to_locking(&mut self) {
        match mem::take(&mut self.lock_state) {
            LockState::WaitingForSurfaces {
                confirmation,
                deadline_token,
            } => {
                self.event_loop.remove(deadline_token);

                self.close_screenshot_ui();
                self.cursor_manager
                    .set_cursor_image(CursorImageStatus::default_named());
                self.switcher.cancel();

                if self.output_state.is_empty() {
                    // There are no outputs, lock the session right away.
                    let lock = confirmation.ext_session_lock().clone();
                    confirmation.lock();
                    self.lock_state = LockState::Locked(lock);
                } else {
                    // There are outputs which we need to redraw before locking.
                    self.lock_state = LockState::Locking(confirmation);
                    self.queue_redraw_all();
                }
            }
            other => {
                error!("continue_to_locking() called with wrong lock state: {other:?}",);
                self.lock_state = other;
            }
        }
    }

    pub fn unlock(&mut self) {
        info!("unlocking session");

        let prev = mem::take(&mut self.lock_state);
        if let LockState::WaitingForSurfaces { deadline_token, .. } = prev {
            self.event_loop.remove(deadline_token);
        }

        for output_state in self.output_state.values_mut() {
            output_state.lock_surface = None;
        }
        self.queue_redraw_all();
    }

    fn update_locked_hint(&mut self) {
        if !self.is_session_instance {
            return;
        }

        // One session-path resolution for the whole process, and it is not `XDG_SESSION_ID`: a
        // GNOME session runs the shell as a user service, which need not carry that variable at
        // all. See `freedesktop_login1::resolve_session_path`.
        let Some(session_path) = crate::dbus::freedesktop_login1::session_path() else {
            warn!("our logind session is unknown; LockedHint won't be set");
            return;
        };

        fn call(session_path: &zbus::zvariant::ObjectPath<'_>, locked: bool) -> anyhow::Result<()> {
            let conn = zbus::blocking::Connection::system()
                .context("error connecting to the system bus")?;

            conn.call_method(
                Some("org.freedesktop.login1"),
                session_path,
                Some("org.freedesktop.login1.Session"),
                "SetLockedHint",
                &(locked),
            )
            .context("failed to call SetLockedHint")?;

            Ok(())
        }

        // Consider only the fully locked state here. When using the locked hint with sleep
        // inhibitor tools, we want to allow sleep only after the screens are fully cleared with
        // the lock screen, which corresponds to the Locked state.
        //
        // Two sources, one hint: `ext-session-lock` (an external locker, niri's inherited path)
        // and the screen shield (GNOME's own, `_setLocked` → `SetLockedHint`,
        // `screenShield.js:173-174`). They must not each write it or they would fight over the
        // property; whichever says locked wins.
        let locked =
            matches!(self.lock_state, LockState::Locked(_)) || self.screen_shield.is_locked();

        if self.locked_hint.is_some_and(|h| h == locked) {
            return;
        }

        self.locked_hint = Some(locked);

        let res = thread::Builder::new()
            .name("Logind LockedHint Updater".to_owned())
            .spawn(move || {
                let _span = tracy_client::span!("LockedHint");

                if let Err(err) = call(session_path, locked) {
                    warn!("failed to set logind LockedHint: {err:?}");
                }
            });

        if let Err(err) = res {
            warn!("error spawning a thread to set logind LockedHint: {err:?}");
        }
    }

    pub fn new_lock_surface(&mut self, surface: LockSurface, output: &Output) {
        let lock = match &self.lock_state {
            LockState::Unlocked => {
                error!("tried to add a lock surface on an unlocked session");
                return;
            }
            LockState::WaitingForSurfaces { confirmation, .. } => confirmation.ext_session_lock(),
            LockState::Locking(confirmation) => confirmation.ext_session_lock(),
            LockState::Locked(lock) => lock,
        };

        if lock.client() != surface.wl_surface().client() {
            debug!("ignoring lock surface from an unrelated client");
            return;
        }

        let Some(output_state) = self.output_state.get_mut(output) else {
            error!("missing output state");
            return;
        };

        output_state.lock_surface = Some(surface);
    }

    /// Activates the pointer constraint if necessary according to the current pointer contents.
    ///
    /// Make sure the pointer location and contents are up to date before calling this.
    pub fn maybe_activate_pointer_constraint(&self) {
        let Some((surface, surface_loc)) = &self.pointer_contents.surface else {
            return;
        };

        let pointer = self.seat.get_pointer().unwrap();
        if Some(surface) != pointer.current_focus().as_ref() {
            return;
        }

        with_pointer_constraint(surface, &pointer, |constraint| {
            let Some(constraint) = constraint else { return };

            if constraint.is_active() {
                return;
            }

            // Constraint does not apply if not within region.
            if let Some(region) = constraint.region() {
                let pointer_pos = pointer.current_location();
                let pos_within_surface = pointer_pos - *surface_loc;
                if !region.contains(pos_within_surface.to_i32_round()) {
                    return;
                }
            }

            constraint.activate();
        });
    }

    pub fn focus_layer_surface_if_on_demand(&mut self, surface: Option<LayerSurface>) {
        if let Some(surface) = surface {
            if surface.cached_state().keyboard_interactivity
                == wlr_layer::KeyboardInteractivity::OnDemand
            {
                if self.layer_shell_on_demand_focus.as_ref() != Some(&surface) {
                    self.layer_shell_on_demand_focus = Some(surface);

                    // FIXME: granular.
                    self.queue_redraw_all();
                }

                return;
            }
        }

        // Something else got clicked, clear on-demand layer-shell focus.
        if self.layer_shell_on_demand_focus.is_some() {
            self.layer_shell_on_demand_focus = None;

            // FIXME: granular.
            self.queue_redraw_all();
        }
    }

    /// Tries to find and return the root shell surface for a given surface.
    ///
    /// I.e. for popups, this function will try to find the parent toplevel or layer surface. For
    /// regular subsurfaces, it will find the root surface.
    pub fn find_root_shell_surface(&self, surface: &WlSurface) -> WlSurface {
        let Some(root) = self.root_surface.get(surface) else {
            return surface.clone();
        };

        if let Some(popup) = self.popups.find_popup(root) {
            return find_popup_root_surface(&popup).unwrap_or_else(|_| root.clone());
        }

        root.clone()
    }

    pub fn on_ipc_outputs_changed(&self) {
        let _span = tracy_client::span!("Synoik::on_ipc_outputs_changed");

        let Some(dbus) = &self.dbus else { return };
        let Some(conn_display_config) = dbus.conn_display_config.clone() else {
            return;
        };

        let res = thread::Builder::new()
            .name("DisplayConfig MonitorsChanged Emitter".to_owned())
            .spawn(move || {
                use crate::dbus::mutter_display_config::DisplayConfig;
                let _span = tracy_client::span!("MonitorsChanged");
                let iface = match conn_display_config
                    .object_server()
                    .interface::<_, DisplayConfig>("/org/gnome/Mutter/DisplayConfig")
                {
                    Ok(iface) => iface,
                    Err(err) => {
                        warn!("error getting DisplayConfig interface: {err:?}");
                        return;
                    }
                };

                async_io::block_on(async move {
                    if let Err(err) = DisplayConfig::monitors_changed(iface.signal_emitter()).await
                    {
                        warn!("error emitting MonitorsChanged: {err:?}");
                    }
                });
            });

        if let Err(err) = res {
            warn!("error spawning a thread to send MonitorsChanged: {err:?}");
        }
    }

    /// Send `AcceleratorActivated`/`AcceleratorDeactivated` for a grabbed
    /// accelerator, unicast to the grabbing client like gnome-shell does. The
    /// parameters dict carries `timestamp` and `action-mode`.
    pub fn emit_accelerator_signal(&self, action: u32, activated: bool) {
        use std::collections::HashMap;

        use zbus::names::BusName;
        use zbus::zvariant::Value;

        let Some(grab) = self.accel_grabs.iter().find(|g| g.action == action) else {
            return;
        };
        let Some(conn) = self.dbus.as_ref().and_then(|d| d.conn_gnome_shell.clone()) else {
            return;
        };

        let mut parameters = HashMap::new();
        let timestamp = get_monotonic_time().as_millis() as u32;
        parameters.insert("timestamp", Value::from(timestamp));
        // Shell.ActionMode: NORMAL or LOCK_SCREEN is all we distinguish.
        let action_mode: u32 = if self.is_locked() { 1 << 2 } else { 1 };
        parameters.insert("action-mode", Value::from(action_mode));

        let name = if activated {
            "AcceleratorActivated"
        } else {
            "AcceleratorDeactivated"
        };
        let destination = match BusName::try_from(grab.owner.clone()) {
            Ok(destination) => destination,
            Err(err) => {
                warn!("invalid grab owner name {:?}: {err:?}", grab.owner);
                return;
            }
        };
        let res = async_io::block_on(conn.inner().emit_signal(
            Some(destination),
            "/org/gnome/Shell",
            "org.gnome.Shell",
            name,
            &(action, parameters),
        ));
        if let Err(err) = res {
            warn!("error emitting {name}: {err:?}");
        }
    }

    /// Tell the portal the window/app list moved.
    ///
    /// GNOME emits `WindowsChanged` off `tracked-windows-changed` and
    /// `RunningApplicationsChanged` off `app-state-changed`/`notify::focus-app`
    /// (`introspect.js:36-47`, `:100-108`). `sync_running_apps` is the one place that already
    /// re-snapshots both, so it is the seam — the alternative is invalidation hooks on every path
    /// that can map, unmap or focus a window, and the ones that get forgotten are silent.
    pub fn emit_introspect_changed(&self) {
        let Some(conn) = self.dbus.as_ref().and_then(|d| d.conn_introspect.as_ref()) else {
            return;
        };
        for name in ["WindowsChanged", "RunningApplicationsChanged"] {
            let res = async_io::block_on(conn.inner().emit_signal(
                None::<zbus::names::BusName<'_>>,
                "/org/gnome/Shell/Introspect",
                "org.gnome.Shell.Introspect",
                name,
                &(),
            ));
            if let Err(err) = res {
                warn!("error emitting {name}: {err:?}");
            }
        }
    }

    pub fn handle_focus_follows_mouse(&mut self, new_focus: &PointContents) {
        let Some(ffm) = self.config.borrow().input.focus_follows_mouse else {
            return;
        };

        let pointer = &self.seat.get_pointer().unwrap();
        if pointer.is_grabbed() {
            return;
        }

        if self.switcher.is_open() {
            return;
        }

        // Recompute the current pointer focus because we don't update it during animations.
        let current_focus = self.contents_under(pointer.current_location());

        if let Some(output) = &new_focus.output {
            if current_focus.output.as_ref() != Some(output) {
                self.layout.focus_output(output);
            }
        }

        if let Some(window) = &new_focus.window {
            if !self.layout.is_overview_open() && current_focus.window.as_ref() != Some(window) {
                let (window, hit) = window;

                // Don't trigger focus-follows-mouse over the tab indicator.
                if matches!(
                    hit,
                    HitType::Activate {
                        is_tab_indicator: true
                    }
                ) {
                    return;
                }

                if !self.layout.should_trigger_focus_follows_mouse_on(window) {
                    return;
                }

                if let Some(threshold) = ffm.max_scroll_amount {
                    if self.layout.scroll_amount_to_activate(window) > threshold.0 {
                        return;
                    }
                }

                self.layout.activate_window_without_raising(window);
                self.layer_shell_on_demand_focus = None;
            }
        }

        if let Some(layer) = &new_focus.layer {
            if current_focus.layer.as_ref() != Some(layer) {
                self.layer_shell_on_demand_focus = Some(layer.clone());
            }
        }
    }

    /// Renders an output's `Output`-target contents into a renderer-neutral CPU buffer through the
    /// given renderer. On a Vulkan session the screen-transition crossfade composites the `Output`
    /// target through the owned renderer (which can't sample a GLES texture), so it uploads this
    /// buffer to a `VkTexture`.
    fn capture_output_neutral(
        &self,
        renderer: &mut VulkanRenderer,
        output: &Output,
        target: RenderTarget,
    ) -> Option<MemoryBuffer> {
        let size = output.current_mode().unwrap().size;
        let transform = output.current_transform();
        let scale = Scale::from(output.current_scale().fractional_scale());

        let ctx = RenderCtx {
            renderer,
            target,
            xray: None,
            appearance: Some(self.appearance()),
        };
        let elements = self.render_to_vec(ctx, output, false);
        let elements = elements.iter().rev();
        match render_to_vec(renderer, size, scale, transform, Fourcc::Abgr8888, elements) {
            Ok(data) => {
                let buffer_size = size.to_logical(1).to_buffer(1, Transform::Normal);
                Some(MemoryBuffer::new(
                    data,
                    Fourcc::Abgr8888,
                    buffer_size,
                    scale,
                    transform,
                ))
            }
            Err(err) => {
                warn!(
                    "error capturing screen transition neutral buffer for {}: {err:?}",
                    output.name()
                );
                None
            }
        }
    }

    /// Vulkan pass for [`Self::do_screen_transition`]: capture every output's frozen screen through
    /// the owned Vulkan renderer (so the crossfade needs no GLES), keyed by output.
    ///
    /// One neutral per render target: block-out rules key off the target, so the buffer the
    /// crossfade shows a screencast must not be the one it shows the output. An output is only
    /// entered if all of its targets captured; a partial entry would silently crossfade from
    /// nothing on the missing targets.
    pub fn capture_screen_transition_neutrals(
        &mut self,
        renderer: &mut crate::render_helpers::vulkan::VulkanRenderer,
    ) -> std::collections::HashMap<Output, [MemoryBuffer; RenderTarget::COUNT]> {
        self.update_render_elements(None);
        let outputs: Vec<Output> = self.output_state.keys().cloned().collect();
        outputs
            .into_iter()
            .filter_map(|output| {
                let neutrals = ALL_RENDER_TARGETS
                    .map(|target| self.capture_output_neutral(renderer, &output, target));
                // Only keep the output if every target captured.
                if neutrals.iter().any(Option::is_none) {
                    return None;
                }
                Some((output, neutrals.map(Option::unwrap)))
            })
            .collect()
    }

    /// The frozen screen was captured through the owned renderer up front
    /// (`capture_screen_transition_neutrals`); this just hands the buffers to each output's
    /// transition.
    pub fn do_screen_transition(
        &mut self,
        mut neutrals: std::collections::HashMap<Output, [MemoryBuffer; RenderTarget::COUNT]>,
        delay_ms: Option<u16>,
    ) {
        let _span = tracy_client::span!("Synoik::do_screen_transition");

        self.update_render_elements(None);

        let textures: Vec<_> = self
            .output_state
            .keys()
            .cloned()
            .filter_map(|output| {
                let transform = output.current_transform();

                let scale = Scale::from(output.current_scale().fractional_scale());

                // An output whose capture failed is dropped: it gets no crossfade (the failure
                // warned there).
                let neutrals = neutrals.remove(&output)?;
                Some((output, neutrals, scale, transform))
            })
            .collect();

        let delay = delay_ms.map_or(screen_transition::DELAY, |d| {
            Duration::from_millis(u64::from(d))
        });

        for (output, buffers, scale, transform) in textures {
            let state = self.output_state.get_mut(&output).unwrap();
            let clock = self.clock.clone();
            state.screen_transition = Some(ScreenTransition::from_neutrals(
                buffers, scale, transform, delay, clock,
            ));
        }

        // We don't actually need to queue a redraw because the point is to freeze the screen for a
        // bit, and even if the delay was zero, we're drawing the same contents anyway.
    }

    pub fn recompute_window_rules(&mut self) {
        let _span = tracy_client::span!("Synoik::recompute_window_rules");

        let changed = {
            let window_rules = &self.config.borrow().window_rules;

            for unmapped in self.unmapped_windows.values_mut() {
                let new_rules = ResolvedWindowRules::compute(
                    window_rules,
                    WindowRef::Unmapped(unmapped),
                    self.is_at_startup,
                );
                if let InitialConfigureState::Configured { rules, restore, .. } =
                    &mut unmapped.state
                {
                    *rules = new_rules;
                    // Same as in `update_window_rules`: a config reload must not cost an
                    // in-flight restore its seeded position, size and state.
                    if let Some(restore) = restore {
                        restore.rule_seeds.apply(rules);
                    }
                }
            }

            let mut windows = vec![];
            self.layout.with_windows_mut(|mapped, _| {
                if mapped.recompute_window_rules(window_rules, self.is_at_startup) {
                    windows.push(mapped.window.clone());
                }
            });
            let changed = !windows.is_empty();
            for win in windows {
                self.layout.update_window(&win, None);
            }
            changed
        };

        if changed {
            // FIXME: granular.
            self.queue_redraw_all();
        }
    }

    pub fn recompute_layer_rules(&mut self) {
        let _span = tracy_client::span!("Synoik::recompute_layer_rules");

        let mut changed = false;
        {
            let config = self.config.borrow();
            let rules = &config.layer_rules;

            for mapped in self.mapped_layer_surfaces.values_mut() {
                if mapped.recompute_layer_rules(rules, self.is_at_startup) {
                    changed = true;
                    mapped.update_config(&config);
                }
            }
        }

        if changed {
            // FIXME: granular.
            self.queue_redraw_all();
        }
    }

    pub fn reset_pointer_inactivity_timer(&mut self) {
        if self.pointer_inactivity_timer_got_reset {
            return;
        }

        let _span = tracy_client::span!("Synoik::reset_pointer_inactivity_timer");

        if let Some(token) = self.pointer_inactivity_timer.take() {
            self.event_loop.remove(token);
        }

        let Some(timeout_ms) = self.config.borrow().cursor.hide_after_inactive_ms else {
            return;
        };

        let duration = Duration::from_millis(timeout_ms as u64);
        let timer = Timer::from_duration(duration);
        let token = self
            .event_loop
            .insert_source(timer, move |_, _, state| {
                state.synoik.pointer_inactivity_timer = None;

                // If the pointer is already invisible, don't reset it back to Hidden causing one
                // frame of hover.
                if state.synoik.pointer_visibility.is_visible() {
                    state.synoik.pointer_visibility = PointerVisibility::Hidden;
                    state.synoik.queue_redraw_all();
                }

                TimeoutAction::Drop
            })
            .unwrap();
        self.pointer_inactivity_timer = Some(token);

        self.pointer_inactivity_timer_got_reset = true;
    }

    pub fn notify_activity(&mut self) {
        if self.notified_activity_this_iteration {
            return;
        }

        let _span = tracy_client::span!("Synoik::notify_activity");

        self.idle_notifier_state.notify_activity(&self.seat);

        // Feed the same activity to the D-Bus idle monitor: fire any user-active watches, re-arm
        // idle watches, and reschedule their timer. Runs once per event-loop iteration (guarded
        // above), so continuous input doesn't churn the timer per event.
        let now = self.clock.now_unadjusted();
        let fired = self.idle_monitor.on_activity(now);
        self.emit_idle_watch_fired(&fired);
        self.reschedule_idle_monitor_timer();

        // A banner shown while the user was idle arms its (shorter) expiry on
        // their first activity (`js/ui/messageTray.js:1118-1122`).
        if self.notification_banner.on_activity() {
            self.reschedule_notification_banner_timer();
        }

        self.notified_activity_this_iteration = true;
    }

    /// Apply a store mutation's [`Effects`](crate::notifications::Effects): hand the
    /// owed signal emissions to the server task (which owns the connection and emits
    /// unicast), and drive the banner surface.
    pub fn apply_notification_effects(&mut self, effects: crate::notifications::Effects) {
        use crate::notifications::{BannerEffect, SynoikToNotifications};
        use crate::ui::notification_card::content_for;

        if let Some(tx) = &self.notifications_emit {
            for closed in &effects.closed {
                // `NotificationClosed` is an fdo signal, unicast to the posting
                // client; a notification with no sender has nothing to emit to
                // (an untracked fdo notification, or any `org.gtk.Notifications`
                // one — that interface has no closed signal at all).
                let Some(sender) = closed.sender.clone() else {
                    continue;
                };
                let _ = tx.send_blocking(SynoikToNotifications::Closed {
                    id: closed.id,
                    reason: closed.reason,
                    sender: Some(sender),
                });
            }
        }

        match effects.banner {
            Some(BannerEffect::RefreshCurrent) => {
                // The shown notification was replaced in place: refresh content
                // and re-arm the timeout (`js/ui/messageTray.js:938-943`).
                if let Some(id) = self.notification_banner.content_id() {
                    let now = self.clock.now_unadjusted();
                    if let Some(content) = content_for(&self.notifications, id, now) {
                        let idle = self.user_is_idle();
                        self.notification_banner.refresh(content, idle);
                        self.reschedule_notification_banner_timer();
                    }
                }
                self.queue_redraw_all();
            }
            Some(BannerEffect::HideCurrent) => {
                // The model already destroyed the shown notification: hide
                // without animation and never double-destroy a transient
                // (`js/ui/messageTray.js:909-917,1282`), then drain the queue.
                self.notification_banner.hide_removed();
                self.reschedule_notification_banner_timer();
                self.maybe_show_banner();
                self.queue_redraw_all();
            }
            Some(BannerEffect::QueueChanged) => {
                self.maybe_show_banner();
                self.queue_redraw_all();
            }
            None => {
                // Mutations that don't re-enter banner admission (e.g. a
                // replace to LOW urgency) must still update the shown content —
                // GNOME's banner widget live-binds the notification properties.
                if let Some(id) = self.notification_banner.content_id() {
                    let now = self.clock.now_unadjusted();
                    if let Some(content) = content_for(&self.notifications, id, now) {
                        self.notification_banner.sync_content(content);
                        self.queue_redraw_all();
                    }
                }
            }
        }

        // Every store mutation ends here, so this keeps an open calendar
        // popover's message list live — without re-acknowledging (arrivals
        // while open stay unseen, `js/ui/messageList.js:1193-1199`).
        self.refresh_popover_notifications();
        // ...and the panel's unread-messages dot in sync with the store.
        self.update_messages_indicator();
    }

    /// Recompute the panel's `MessagesIndicator` dot from the store: shown when
    /// banners are enabled and there are unseen notifications not still queued
    /// for a banner (`js/ui/dateMenu.js:787-798`). Called from every store
    /// mutation (via [`Self::apply_notification_effects`]) and whenever the
    /// `show-banners`/DND setting flips.
    pub fn update_messages_indicator(&mut self) {
        let show_banners = !self.gnome_settings.quick_toggles.do_not_disturb;
        let visible = show_banners && self.notifications.indicator_count() > 0;
        if self.panel.set_messages_indicator(visible) {
            self.queue_redraw_all();
        }
    }

    /// Snapshot the app catalog into the dash (GNOME's `Dash._redisplay`,
    /// `dash.js:677-699`): every favorite, then every running app that is not
    /// already a favorite, each flagged with whether it is running. Returns whether
    /// the dash changed.
    pub fn sync_dash_favorites(&mut self) -> bool {
        let favorites = self.app_system.favorites();
        let n_favorites = favorites.len();

        let mut items: Vec<DashEntry> = favorites
            .into_iter()
            .map(|e| DashEntry {
                // The dot is "not stopped", not "has windows": a favorite that is
                // still launching shows one (`_updateRunningStyle`,
                // `appDisplay.js:3007-3012`).
                running: self.app_system.shows_running_dot(&e.id),
                urgent: self.app_system.has_urgent_window(&e.id),
                id: e.id,
                name: e.name,
                icon: e.icon,
            })
            .collect();

        // Running non-favorites follow, in `get_running()` order.
        for app in self.app_system.running() {
            if items.iter().any(|item| item.id == app.id) {
                continue;
            }
            // NOT YET dashed when it resolves to nothing, though GNOME dashes such a
            // window-backed app like any other (`application-x-executable` fallback).
            //
            // Adding a tile re-lays-out the row, so the tiles ALREADY in it re-bake, and one
            // existing icon's element `Id` changes on the first frame after the change — measured:
            // one `External` id, on frame 0 only, element list constant at 53. A churned `Id`
            // costs the output's backdrop blur. It is a one-time cost, not per-frame, but the
            // re-bake happens inside the frame path and a content sync alone does not pre-empt it
            // (verified: `sync_dash_favorites` before the overview does NOT clear it; only an
            // extra render does). Doing it properly means baking the dash off the frame path when
            // its content changes, which is its own change.
            match self.app_system.lookup(&app.id) {
                Some(entry) => items.push(DashEntry {
                    urgent: self.app_system.has_urgent_window(&entry.id),
                    id: entry.id,
                    name: entry.name,
                    icon: entry.icon,
                    running: true,
                }),
                None => items.push(DashEntry {
                    urgent: self.app_system.has_urgent_window(&app.id),
                    id: app.id.clone(),
                    name: app.fallback_label().to_owned(),
                    icon: crate::app_system::AppIconRef::Fallback,
                    running: true,
                }),
            }
        }

        self.dash.set_items(items, n_favorites)
    }

    /// Snapshot the app catalog into the app grid (GNOME's `AppDisplay._redisplay`,
    /// `appDisplay.js:1086,1492-1504`): every installed app that should show, minus
    /// the favorites (they live in the dash), sorted by name (`_compareItems`,
    /// `appDisplay.js:1122`). Returns whether the grid changed. (Parental controls are
    /// not modeled yet, so that half of the filter is a no-op for now.)
    pub fn sync_app_grid(&mut self) -> bool {
        // Folders first: each takes a grid slot of its own and its members stop
        // appearing at the top level (`_redisplay` collects `appsInsideFolders` and
        // filters the app list against it, `appDisplay.js:1508-1533`). A folder that
        // resolves to nothing is destroyed rather than displayed.
        let mut inside_folders: HashSet<String> = HashSet::new();
        let mut folders: Vec<AppGridEntry> = Vec::new();
        for folder in &self.gnome_settings.app_folders {
            let members = self.app_system.folder_members(folder);
            if members.is_empty() {
                continue;
            }
            inside_folders.extend(members.iter().map(|e| e.id.clone()));
            // A folder tile draws its members, not this — it is only what a *drag* of
            // the folder carries, and a drag proxy is a single icon. GNOME drags the
            // composed 2×2; the first member is the closest single icon to it.
            let proxy = members[0].icon.clone();
            folders.push(AppGridEntry {
                id: folder.id.clone(),
                name: folder.name.clone(),
                icon: proxy,
                folder: Some(
                    members
                        .into_iter()
                        .map(|e| AppGridEntry {
                            id: e.id,
                            name: e.name,
                            icon: e.icon,
                            folder: None,
                        })
                        .collect(),
                ),
            });
        }

        // Resolve the favorites *once*. `is_favorite` re-resolves every stored id through GIO on
        // each call, so asking it per installed app is O(installed x favorites) desktop-file
        // parses — 752 of them on a 94-app catalog, which is 25 of this function's 28 ms.
        let favorites: HashSet<String> = self
            .app_system
            .favorites()
            .into_iter()
            .map(|e| e.id)
            .collect();
        let mut entries: Vec<AppGridEntry> = self
            .app_system
            .installed()
            .filter(|e| !favorites.contains(&e.id) && !inside_folders.contains(&e.id))
            .map(|e| AppGridEntry {
                id: e.id.clone(),
                name: e.name.clone(),
                icon: e.icon.clone(),
                folder: None,
            })
            .collect();
        entries.append(&mut folders);
        // gnome-shell's `AppDisplay._compareItems` (`appDisplay.js:1475-1490`): apps the
        // user has placed sort by their saved `(page, position)`; everything else falls
        // in *after* them, by name. The arrangement lives in `org.gnome.shell
        // app-picker-layout`, not in any state of ours — and gnome-shell writes that key
        // itself on every layout pass, so a real profile's grid reflects install order
        // plus manual moves, never plain alphabetical.
        let layout = &self.gnome_settings.app_picker_layout;
        // The name fallback is `localeCompare`, i.e. a real collation: "Écran" belongs
        // with the E's, not after Z. GLib's collation key is the same one GTK sorts with
        // and reads the process locale, so it is the closest thing to gnome-shell's
        // without pulling in an ICU of our own. Built once per entry rather than per
        // comparison — the key is what is cheap to compare, not to make.
        let mut keyed: Vec<(AppGridEntry, gio::glib::CollationKey)> = entries
            .drain(..)
            .map(|e| {
                let key = gio::glib::CollationKey::from(e.name.as_str());
                (e, key)
            })
            .collect();
        keyed.sort_by(|(a, a_key), (b, b_key)| {
            match (layout.get(&a.id), layout.get(&b.id)) {
                (Some(a), Some(b)) => a.cmp(b),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                // Ties broken by the raw name, so the order is total and stable across
                // runs even for two apps whose names collate equal.
                (None, None) => a_key.cmp(b_key).then_with(|| a.name.cmp(&b.name)),
            }
        });
        entries = keyed.into_iter().map(|(e, _)| e).collect();
        // An open folder follows the model it was opened from: its members are re-resolved,
        // and a folder that no longer resolves to anything takes its dialog down with it
        // (GNOME destroys the `FolderIcon`, and the dialog is destroyed with its source,
        // `appDisplay.js:2320-2325`).
        let mut changed = if let Some(id) = self.folder_dialog.folder_id() {
            let members = entries
                .iter()
                .find(|e| e.id == id)
                .and_then(|e| Some((e.name.clone(), e.folder.clone()?)));
            self.folder_dialog.resync(members)
        } else {
            false
        };
        changed |= self.app_grid.set_entries(entries);
        changed
    }

    /// Note that the app catalog changed, and reload it once the pings stop.
    ///
    /// A single `installed-changed` is rarely a single change: installing a package
    /// writes many `.desktop` files, and glib's monitors fire per directory. Reloading on
    /// each one re-enumerates every desktop file on disk, drops four icon caches,
    /// re-syncs three surfaces and forces a full redraw — all on the compositor thread,
    /// and all of it thrown away by the next ping a few milliseconds later.
    ///
    /// So the ping only moves a deadline. gnome-shell does exactly this, with the same
    /// restart-on-each-ping shape and the same interval
    /// (`shell_app_cache_queue_update`, `src/shell-app-cache.c:219-230`,
    /// `DEFAULT_TIMEOUT_SECONDS 5`); it also rate-limits its directory monitors to the
    /// same period. A pending deadline means a timer is already armed, so a burst arms
    /// exactly one.
    pub fn queue_app_catalog_reload(&mut self) {
        let deadline = std::time::Instant::now() + APP_CATALOG_RELOAD_DEBOUNCE;
        if self.app_catalog_reload_at.replace(deadline).is_some() {
            // A timer is already running; it will see the moved deadline and wait again.
            return;
        }
        let armed = self.event_loop.insert_source(
            Timer::from_duration(APP_CATALOG_RELOAD_DEBOUNCE),
            |_, _, state| match app_catalog_reload_wait(
                state.synoik.app_catalog_reload_at,
                std::time::Instant::now(),
            ) {
                Some(at) => TimeoutAction::ToInstant(at),
                None => {
                    state.synoik.app_catalog_reload_at = None;
                    state.synoik.reload_app_catalog();
                    TimeoutAction::Drop
                }
            },
        );
        if let Err(err) = armed {
            // No timer means no reload, so do it now rather than lose the change.
            tracing::warn!("could not arm the app catalog reload: {err}");
            self.app_catalog_reload_at = None;
            self.reload_app_catalog();
        }
    }

    /// Re-read the app catalog and everything derived from it.
    ///
    /// Runs the downstream **unconditionally**, so a `touch` on any watched `.desktop` is a usable
    /// "reload now" trigger even when the enumeration is unchanged. This used to early-return on an
    /// identical enumeration, for a reason that has since stopped being true: dropping the icon
    /// caches once blanked every dash and grid tile until the off-thread decodes landed, and
    /// `AppIconCache::invalidate` now *demotes* buffers to `stale` and keeps serving the old pixels
    /// until each replacement arrives (`render_helpers/icon.rs`), with the uploads dropped per icon
    /// as it lands. `a_ping_on_an_unchanged_catalog_keeps_the_dash_icons` pins that.
    ///
    /// What is left is cost, and it is small: `perf_probe_what_does_an_app_catalog_reload_cost`
    /// prices the downstream at ~1.2 ms on a 94-app catalog with the overview and app grid open,
    /// against the ~7 ms `refresh()` enumeration that both the old and new shapes pay anyway. The
    /// spurious glib ping that lands a few seconds into every session therefore costs ~1 ms, not a
    /// visible flicker. Note that ~1.2 ms is *after* hoisting the favorites resolve out of
    /// `sync_app_grid`'s per-app filter; before that, this function took 33 ms.
    pub(crate) fn reload_app_catalog(&mut self) {
        self.app_system.refresh();
        // A newly installed app's icon (or a cached negative) may now resolve. The
        // cache keeps serving the old pixels until each replacement decode lands, and
        // the uploads are dropped per icon at that point — so nothing blanks here.
        self.app_icon_cache.clear();
        // A refreshed catalog may change what the current query resolves to, and
        // which apps populate the grid.
        self.sync_overview_search();
        self.sync_dash_favorites();
        self.sync_app_grid();
        // The catalog (and its icons) just changed and the caches were cleared above
        // — re-warm the decodes off-thread for the next open.
        self.prewarm_app_icons();
        // Unconditional: dropping the icon uploads and reshuffling search results both
        // invalidate what is on screen, and an idle overview produces no frames on its
        // own — a stale frame would let a click land on a tile that has since changed app.
        self.queue_redraw_all();
    }

    /// Warm the app-icon decode cache for the always-visible launch surfaces (the
    /// dash + the app grid) at each connected output's scale, so the first overview
    /// open finds the icons already decoded instead of rasterizing ~24 of them on the
    /// opening frame. This mirrors GNOME, which keeps its `AppDisplay` resident and
    /// populates it off the idle deferred-work queue at startup (`appDisplay.js:1339`)
    /// into a shell-wide cache held `POLICY_FOREVER` (`st-texture-cache.c:998`).
    ///
    /// The decode runs on the worker thread and [`AppIconCache::buffer`] dedups keys
    /// that are already cached or in flight, so this is idempotent and cheap to call
    /// again whenever the scale set or the app content changes.
    /// Build a fresh symbolic-icon cache for `theme`, keeping it pointed at the worker.
    ///
    /// An icon-theme change replaces the cache wholesale (that is how its textures, which are not
    /// keyed by theme, get dropped). The worker thread outlives it, so the new cache has to be
    /// re-handed the sink — miss that and symbolic rasterization silently falls back to running
    /// inline on the frame thread, which is exactly what this machinery exists to prevent.
    pub fn replace_icon_cache(&mut self, theme: &str) {
        let mut replacement = IconCache::new(theme);
        // Keep the outgoing cache's icons drawable until each is re-rasterized in the new
        // theme; re-rasterizing goes through the worker, so a bare replacement blanks every
        // symbolic icon on screen until it catches up.
        replacement.adopt_textures_from(&self.icon_cache);
        if let Some(tx) = self.symbolic_icon_tx.clone() {
            replacement.set_worker(tx);
        }
        self.icon_cache = replacement;
    }

    /// Drop one icon's uploaded textures everywhere it can appear, because its pixels
    /// just changed underneath them.
    ///
    /// The upload key is (scale, descriptor, size) with no notion of *which* decode
    /// produced it, so a texture uploaded while the cache was serving stale pixels
    /// would otherwise be served forever. Dropping per-icon as each decode lands is
    /// what lets an invalidation keep drawing the old icons instead of nothing:
    /// nothing has to clear the whole map up front.
    /// A window preview's app icon and caption — `Shell.WindowTracker.get_window_app`
    /// plus `_getCaption` (`windowPreview.js:133-135,259-266`): the window's title,
    /// falling back to the app's name.
    ///
    /// The icon is `None` when the window resolves to no installed app. Such a window is now a
    /// window-backed app in the running set, but there is still no icon behind it — the dash
    /// draws `AppIconRef::Fallback` for one; a preview draws none rather than stamping every
    /// unresolvable window with `application-x-executable`.
    fn preview_app_chrome(
        &self,
        window: &smithay::desktop::Window,
    ) -> (Option<crate::app_system::AppIconRef>, String) {
        let Some((_, mapped)) = self.layout.windows().find(|(_, m)| &m.window == window) else {
            return (None, String::new());
        };
        let (app_id, title) = crate::utils::with_toplevel_role(mapped.toplevel(), |role| {
            (role.app_id.clone(), role.title.clone())
        });
        let sandbox = self
            .app_system
            .sandbox_id_cached(mapped.credentials().map(|c| c.pid));
        let entry = app_id
            .as_deref()
            .and_then(|app_id| self.app_system.app_for_window(app_id, sandbox));
        let caption = title
            .filter(|t| !t.is_empty())
            .or_else(|| entry.as_ref().map(|e| e.name.clone()))
            .unwrap_or_default();
        (entry.map(|e| e.icon), caption)
    }

    pub fn drop_app_icon_uploads(&self, icon: &crate::app_system::AppIconRef, logical_px: u16) {
        self.dash.drop_icon_upload(icon, logical_px);
        self.app_grid.drop_icon_upload(icon, logical_px);
        self.folder_dialog.drop_icon_upload(icon, logical_px);
        self.overview_search.drop_icon_upload(icon, logical_px);
        self.preview_chrome.drop_icon_upload(icon, logical_px);
        crate::ui::widget::drop_app_icon_upload(
            &mut self.app_icon_uploads.borrow_mut(),
            icon,
            logical_px,
        );
    }

    /// The `(output scale, grid icon px, open-folder icon px)` combinations the app
    /// surfaces will draw at — the decode-cache keys a prewarm has to hit to be worth
    /// anything. Separate from [`Self::prewarm_app_icons`] so it can be asserted without
    /// a decode worker (the prewarm itself no-ops without one).
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn prewarm_variants(&self) -> Vec<(f64, f64, Option<f64>, f64)> {
        let mut variants: Vec<(f64, f64, Option<f64>, f64)> = Vec::new();
        for output in self.global_space.outputs() {
            let scale = output.current_scale().fractional_scale();
            let controls = self.layout.controls_layout_for_output(output);
            let metrics = controls
                .map(|c| self.app_grid.metrics_for(c.app_display))
                .unwrap_or(crate::ui::widget::TileMetrics::overview());
            // The dash icon is chosen from its band too, for the same reason (the
            // adaptive chrome ramp), so warming a flat 64 warms an entry a small canvas
            // never asks for.
            let dash_px = controls
                .map(|c| crate::ui::dash::Dash::metrics(c.dash).icon_px)
                .unwrap_or(crate::ui::dash::ICON_PX);
            let view = Rectangle::from_size(output_size(output));
            let variant = (
                scale,
                metrics.icon_px,
                self.folder_dialog.icon_px(view),
                dash_px,
            );
            if !variants.contains(&variant) {
                variants.push(variant);
            }
        }
        variants
    }

    pub fn prewarm_app_icons(&self) {
        // Before the worker is wired, `buffer()` would decode inline on the main
        // thread — the exact startup stall this exists to avoid.
        if !self.app_icon_cache.has_worker() {
            return;
        }
        // The distinct (scale, grid icon size) pairs the surfaces will draw at — each
        // combination is its own decode-cache key. The grid's icon size is **chosen from
        // the band it is given** (`AppGrid::metrics_for`), not fixed at
        // `TileMetrics::overview()`'s 96: a 1280×800 screen renders the grid at 48. Warming
        // 96 there warms an entry nothing ever asks for, and every icon then decodes
        // lazily the first time its page comes up — which is exactly what a one-time blink
        // on first reaching a page looks like. A handful of outputs at most, so a linear
        // dedup is fine.
        for (scale, grid_px, folder_px, dash_px) in self.prewarm_variants() {
            for icon in self.dash.icon_refs() {
                let _ = self.app_icon_cache.buffer(icon, dash_px, scale);
            }
            for icon in self.app_grid.icon_refs() {
                let _ = self.app_icon_cache.buffer(icon, grid_px, scale);
            }
            // A folder tile draws its members at the smaller sub-icon size, which is its
            // own decode-cache key.
            let subicon_px = crate::ui::widget::TileMetrics {
                icon_px: grid_px,
                ..crate::ui::widget::TileMetrics::overview()
            }
            .folder_subicon_px();
            for icon in self.app_grid.folder_icon_refs() {
                let _ = self.app_icon_cache.buffer(icon, subicon_px, scale);
            }
            // An open folder's own view draws its members at *its* tile icon size.
            if let Some(folder_px) = folder_px {
                for icon in self.folder_dialog.icon_refs() {
                    let _ = self.app_icon_cache.buffer(icon, folder_px, scale);
                }
            }
        }
    }

    /// Snapshot every mapped window into the app model's running-app tracker —
    /// our `ShellWindowTracker` bookkeeping (`shell-window-tracker.c`), which
    /// GNOME drives from `window-created`/`unmanaged`/`notify::wm-class` and the
    /// focus-order updates. We instead re-snapshot whenever the window set or the
    /// focus order could have moved, which is cheap (a walk of the window list)
    /// and immune to a missed edge.
    ///
    /// Returns whether anything the dash reads changed — the running list, or an
    /// app's *state* (our `app-state-changed`, `dash.js:383`). The second half
    /// matters because a launch moves an app to STARTING, which shows a running dot,
    /// without touching a single window.
    /// Enforce the app-level urgency rule: **an app with a focused window is not urgent.**
    ///
    /// Urgency itself stays per window, exactly as mutter keeps it — focusing a window unsets
    /// its own `wm_state_demands_attention` and nothing else's (`window.c:5090-5091`), and
    /// [`Mapped::set_urgent`] refuses to mark the focused window for the same reason. But the
    /// dash and the dock are per *app*: a second window demanding attention from another
    /// workspace kept the dock poking for an app whose window the user was already working in,
    /// and only walking over to focus *that* window shut it up.
    ///
    /// Written as an invariant re-stated every refresh rather than as a hook on focus changes,
    /// because the case that motivated it is a **map**, not a focus change: an app launches,
    /// focuses its first window, and only then maps a second one on another workspace. A focus
    /// hook would have run before the urgent window existed, and would have worked or not
    /// depending on the order the client happened to map in.
    ///
    /// **Interim** (decided 2026-08-08): urgency wants a redesign around a less distracting
    /// per-window affordance; until there is one, an app you are looking at does not shout.
    fn clear_urgency_of_focused_app(&mut self) {
        use crate::utils::with_toplevel_role;

        let focused_app = self.layout.focus().and_then(|focused| {
            let app_id = with_toplevel_role(focused.toplevel(), |role| role.app_id.clone())?;
            let sandbox = self
                .app_system
                .sandbox_id_cached(focused.credentials().map(|c| c.pid));
            self.app_system
                .app_for_window(&app_id, sandbox)
                .map(|entry| entry.id)
        });
        let Some(focused_app) = focused_app else {
            return;
        };

        // Resolved through `app_for_window`, not by comparing `app_id` strings: that is the
        // identity the dash aggregates on, and a window resolving differently would leave a
        // poke that focusing could never clear.
        let app_system = &self.app_system;
        self.layout.with_windows_mut(|mapped, _| {
            if !mapped.is_urgent() {
                return;
            }
            let sandbox = app_system.sandbox_id_cached(mapped.credentials().map(|c| c.pid));
            let same_app = with_toplevel_role(mapped.toplevel(), |role| role.app_id.clone())
                .and_then(|id| app_system.app_for_window(&id, sandbox))
                .is_some_and(|entry| entry.id == focused_app);
            if same_app {
                mapped.set_urgent(false);
            }
        });
    }

    pub fn sync_running_apps(&mut self) -> bool {
        use crate::app_system::RunningWindow;
        use crate::utils::with_toplevel_role;

        // Before the snapshot, so a window cleared here never reaches the dash as urgent.
        self.clear_urgency_of_focused_app();

        // Sweep timed-out startup sequences here too — mutter runs a timeout source
        // for the same purpose (`startup_sequence_timeout`,
        // `startup-notification.c:483`). A launch that never produced a window would
        // otherwise keep its running dot forever. The sweep marks the state changed
        // itself, so its result is picked up below with everything else.
        self.app_system.expire_startups(get_monotonic_time());

        let mut windows = Vec::new();
        self.layout.with_windows(|mapped, _, _, _| {
            let (app_id, title) = with_toplevel_role(mapped.toplevel(), |role| {
                (role.app_id.clone(), role.title.clone())
            });
            windows.push(RunningWindow {
                id: mapped.id(),
                app_id,
                pid: mapped.credentials().map(|c| c.pid),
                title,
                urgent: mapped.is_urgent(),
                last_focus: mapped.get_focus_timestamp(),
            });
        });
        // Both sides evaluated before the `||`: short-circuiting would leave
        // `state_changed` set and fire it at some unrelated later window change.
        let windows_changed = self.app_system.set_windows(windows);
        let state_changed = self.app_system.take_state_changed();

        // Launch feedback: mutter drives a compositor-wide `wait` cursor off exactly
        // this predicate — `meta_startup_notification_has_pending_sequences`
        // (`startup-notification.c:120-132`) → `MetaCompositor:global_cursor`
        // (`compositor.c:1103-1117`). Its `application_id` requirement is vacuous for
        // us: every sequence is keyed by a desktop id.
        //
        // Recomputed here rather than at the launch/complete sites so it cannot be
        // left stuck on by a path that forgets to clear it — the same reason this
        // whole function re-snapshots instead of taking invalidation hooks.
        let starting = self.app_system.starting_apps().next().is_some();
        let icon = starting.then_some(CursorIcon::Wait);
        if self.cursor_manager.set_global_override(icon) {
            // The `&&` below this call can swallow a `true`, and the cursor is drawn
            // per frame, so it queues its own redraw.
            self.queue_redraw_all();
        }

        windows_changed || state_changed
    }

    /// Whether the overview chrome (dash, search) is actually on screen: the GNOME
    /// overview open, not hidden behind a lock or screenshot surface. The single gate
    /// for every overview-UI pointer/hover intercept — an intercept firing while the
    /// UI is invisible would eat clicks (and the dash one could launch into a locked
    /// session; the S3 blocker). `is_overview_open` alone is also true in niri's own
    /// scrolling-mode overview, where the GNOME chrome never draws — hence `is_gnome_mode`.
    pub fn overview_ui_visible(&self) -> bool {
        self.layout.is_gnome_mode()
            && self.layout.is_overview_open()
            && !self.is_locked()
            && !self.screenshot_ui.is_open()
    }

    /// Where the dash is on `output` right now, in that output's logical coordinates.
    ///
    /// One dash, two homes: the overview's slot while the overview is up, otherwise the dock's
    /// (see [`crate::ui::dock`]) while it is out. Every dash hit-test, hover and drop-target
    /// asks this rather than reaching for the overview layout, which is what lets the dock reuse
    /// the dash's interaction wholesale. `None` when the dash is not on screen at all.
    pub fn dash_area(&self, output: &Output) -> Option<Rectangle<f64, Logical>> {
        if self.overview_ui_visible() {
            return self
                .layout
                .controls_layout_for_output(output)
                .map(|c| c.dash);
        }
        // Locked or behind the screenshot UI the dash is not on screen, and a dock summoned
        // there would be an invisible click-eater in front of the shield — the same trap the
        // overview's own click intercept documents. The render path asks *this* function rather
        // than the dock directly, so the gate cannot come apart between what is drawn and what
        // is clickable. (No test: `LockState` carries a `SessionLocker` a fixture can't build,
        // so the guarantee is structural instead — one gate, both callers.)
        if self.is_locked() || self.screenshot_ui.is_open() {
            return None;
        }
        self.dock.area(output)
    }

    /// Point the dock at whichever output has an app demanding attention, so it can poke that
    /// app's icon above the bottom edge.
    ///
    /// Suppressed while a fullscreen window is focused: poking into a fullscreen video is the one
    /// place where "louder than GNOME" turns into "worse than GNOME". (A setting may follow.)
    pub fn sync_dock_urgency(&mut self) {
        let urgent = self.dash.items().iter().any(|item| item.urgent);

        let fullscreen = self
            .layout
            .focus()
            .is_some_and(|focus| focus.sizing_mode().is_fullscreen());

        let output = self
            .layout
            .active_output()
            .or_else(|| self.dock.output())
            .cloned();
        self.dock
            .set_poking(output.as_ref(), urgent && !fullscreen && output.is_some());
    }

    /// Whether the dock — not the overview — currently owns the dash on `output`.
    pub fn dock_owns_dash(&self, output: &Output) -> bool {
        !self.overview_ui_visible() && self.dash_area(output).is_some()
    }

    /// Reset the overview search on a fresh overview *enter* (GNOME resets search on
    /// overview enter/unmap). Keyed on the rising edge of "the GNOME overview is open"
    /// — deliberately NOT on [`overview_ui_visible`](Self::overview_ui_visible), which
    /// also dips when a screenshot/lock surface covers a still-open overview: GNOME
    /// keeps the query across such an independent modal, so keying on visibility would
    /// wipe an in-progress search on a Print-screen round-trip. (Clearing is not
    /// load-bearing for the lock bypass — `overview_ui_visible` gates the intercepts.)
    /// Nothing clears on close, so the query stays visible through the close fade, as
    /// GNOME's does. Called each cycle from `State::refresh`.
    /// Composite `elements` as one group at `alpha`, or push them straight through
    /// at full opacity.
    ///
    /// The partial-alpha branch is pinned by
    /// `vulkan_search_fade_blends_the_picker_at_partial_alpha`, which measures the
    /// blend against both settled ends; every other test settles the fade, where
    /// this is a pass-through.
    ///
    /// The group composite is what makes the search cross-fade correct: applying a
    /// per-element alpha would double-darken wherever two window previews overlap.
    /// Falls back to a plain push if the offscreen fails, so a fade problem can
    /// never blank the overview.
    fn push_group_at_alpha(
        renderer: &mut VulkanRenderer,
        buffer: &OffscreenBuffer,
        scale: f64,
        alpha: f32,
        elements: Vec<OutputRenderElements>,
        push: &mut dyn FnMut(OutputRenderElements),
    ) {
        if alpha <= 0.001 {
            return;
        }
        if alpha >= 0.999 {
            for elem in elements {
                push(elem);
            }
            return;
        }

        match buffer.render(renderer, Scale::from(scale), &elements) {
            // The element carries the encompassing box's own offset already, so it
            // composites where the group was.
            Ok((elem, _sync, _data)) => push(elem.with_alpha(alpha).into()),
            Err(err) => {
                warn!("error compositing the overview search cross-fade: {err:?}");
                for elem in elements {
                    push(elem);
                }
            }
        }
    }

    /// How far the search has covered the window picker: 0 = picker fully shown,
    /// 1 = fully searching. gnome-shell cross-fades the two over
    /// `SIDE_CONTROLS_ANIMATION_TIME` (`overviewControls.js:609-643`).
    pub fn overview_search_fade(&self) -> f64 {
        match &self.overview_search_fade {
            Some(anim) => anim.clamped_value().clamp(0., 1.),
            None => {
                if self.overview_search_fade_target {
                    1.
                } else {
                    0.
                }
            }
        }
    }

    /// How far the entry has grown from its resting puck to GNOME's pill: 0 = puck, 1 = pill.
    pub fn overview_search_expand(&self) -> f64 {
        match &self.overview_search_expand {
            Some(anim) => anim.clamped_value().clamp(0., 1.),
            None => {
                if self.overview_search_expand_target {
                    1.
                } else {
                    0.
                }
            }
        }
    }

    /// Arms (or retires) the grow/shrink, and pushes the current progress into the search
    /// model so hit-testing follows the animating pill rather than snapping to its target.
    fn update_overview_search_expand(&mut self) {
        let target = self.overview_search.is_expanded();
        if target != self.overview_search_expand_target {
            let from = self.overview_search_expand();
            self.overview_search_expand_target = target;
            // The same fixed `SIDE_CONTROLS_ANIMATION_TIME` ease the cross-fade uses; only
            // whether animations run at all comes from the config.
            let config = synoik_config::Animation {
                off: self.config.borrow().animations.off,
                kind: synoik_config::animations::Kind::Easing(
                    synoik_config::animations::EasingParams {
                        duration_ms: 250,
                        curve: synoik_config::animations::Curve::EaseOutQuad,
                    },
                ),
            };
            self.overview_search_expand = Some(Animation::new(
                self.clock.clone(),
                from,
                if target { 1. } else { 0. },
                0.,
                config,
            ));
        }

        if self
            .overview_search_expand
            .as_ref()
            .is_some_and(|a| a.is_done())
        {
            self.overview_search_expand = None;
        }
        let progress = self.overview_search_expand();
        self.overview_search.set_expand(progress);
    }

    /// Arms (or retires) the cross-fade when the search engages or clears.
    fn update_overview_search_fade(&mut self) {
        let target = self.overview_search.is_active();
        if target != self.overview_search_fade_target {
            let from = self.overview_search_fade();
            self.overview_search_fade_target = target;
            // gnome-shell uses a fixed `SIDE_CONTROLS_ANIMATION_TIME` ease here,
            // not a configurable animation; only whether animations run at all is
            // taken from the config.
            let config = synoik_config::Animation {
                off: self.config.borrow().animations.off,
                kind: synoik_config::animations::Kind::Easing(
                    synoik_config::animations::EasingParams {
                        duration_ms: 250,
                        curve: synoik_config::animations::Curve::EaseOutQuad,
                    },
                ),
            };
            self.overview_search_fade = Some(Animation::new(
                self.clock.clone(),
                from,
                if target { 1. } else { 0. },
                0.,
                config,
            ));
        }

        if self
            .overview_search_fade
            .as_ref()
            .is_some_and(|a| a.is_done())
        {
            self.overview_search_fade = None;
        }
    }

    pub fn refresh_overview_search_state(&mut self) {
        let open = self.layout.is_gnome_mode() && self.layout.is_overview_open();
        if open && !self.overview_search_was_visible {
            self.overview_search.clear();
            // A fresh overview open starts the app grid on page 0
            // (`Main.overview 'hidden'` → `goToPage(0)`, `appDisplay.js:1342`).
            self.app_grid.reset_page();
        }
        // …and a search never outlives the overview that hosted it: gnome-shell drops it
        // on the way out (`prepareToLeaveOverview`'s `_setSearchActive(false)`, and the
        // full `reset()` on unmap — `searchController.js:117-131`). Clearing it here
        // retargets the cross-fade, so the window picker fades back in under the closing
        // overview rather than popping. Leaving it active kept `picker_alpha` at 0 with
        // no overview left to explain it: every window vanished behind the shade and the
        // only way back was to open the overview again.
        if !open && self.overview_search_was_visible {
            self.overview_search.clear();
        }
        // Leaving the app grid takes any open folder with it — hiding the whole overview
        // and returning to the window picker alike. It *animates* out rather than being
        // dropped, so the shrink runs alongside the grid's own fade; the dialog stops
        // being modal the moment the close starts, and retires itself when it ends.
        if !self.layout.is_app_grid_open() {
            self.folder_dialog.hide();
            // A swipe cannot outlive the grid it was dragging.
            self.app_grid_scroll_swipe.reset();
            self.app_grid_pan = None;
            // A grid that is not on screen holds no key focus — a re-opened grid starts
            // over from its first page (`goToPage(0)`, `appDisplay.js:1342`) with nothing
            // lit, exactly as a fresh `AppDisplay` does.
            self.app_grid.set_focused(None);
        }
        self.folder_dialog.advance();
        // The grid's tile eases (a reorder's reflow, the dragged tile's scale-fade) are
        // per-tile animations with their own stagger delays, so they need a tick of their
        // own rather than riding a single timeline.
        self.app_grid.advance_animations();
        self.folder_dialog.advance_grid_animations();
        // The source tile fades out under the opening dialog and back in as it shrinks
        // home, so the panel appears to *become* the icon (`appDisplay.js:2441-2451`).
        let fade = self
            .folder_dialog
            .source_fade()
            .map(|(id, a)| (id.to_owned(), a));
        if self.app_grid.set_tile_fade(fade) {
            self.queue_redraw_all();
        }
        self.overview_search_was_visible = open;
    }

    /// Run the app search for the current query and feed the results into the overview
    /// search model — GNOME's `SearchResultsView.setTerms` → `_doSearch` → the built-in
    /// `AppSearchProvider` (`appDisplay.js:1801-1831`): `AppSystem.search(terms.join(' '))`
    /// → relevance-tier groups → filter `should_show` → concat → cap at
    /// [`MAX_RESULTS`](crate::ui::overview_search::MAX_RESULTS). (No `Shell.AppUsage`
    /// within-tier sort yet — S9+.)
    pub fn sync_overview_search(&mut self) {
        let terms = crate::ui::overview_search::tokenize(self.overview_search.query());
        if terms.is_empty() {
            self.overview_search.set_results(Vec::new());
            return;
        }
        let query = terms.join(" ");
        let mut results = Vec::new();
        'outer: for group in self.app_system.search(&query) {
            for id in group {
                if let Some(entry) = self.app_system.lookup(&id) {
                    if entry.should_show {
                        results.push(SearchResultEntry {
                            id: entry.id,
                            name: entry.name,
                            icon: entry.icon,
                        });
                        if results.len() >= crate::ui::overview_search::MAX_RESULTS {
                            break 'outer;
                        }
                    }
                }
            }
        }
        self.overview_search.set_results(results);
    }

    /// Push a fresh store snapshot into an open calendar popover's message
    /// list, if any.
    pub fn refresh_popover_notifications(&mut self) {
        if self.panel_popover.open_role() != Some(crate::ui::panel::ROLE_DATE_MENU) {
            return;
        }
        let now = self.clock.now_unadjusted();
        let cards = crate::ui::notification_card::message_list_groups(&self.notifications, now);
        if self.panel_popover.set_notifications(cards) {
            self.queue_redraw_all();
        }
    }

    /// Push a fresh MPRIS snapshot into an open calendar popover's message list, if any.
    /// Push the shown app indicators into the panel. The store holds every registered item,
    /// including passive and not-yet-ready ones; the panel only ever sees the drawable set.
    pub fn refresh_panel_indicators(&mut self) {
        let indicators: Vec<_> = self
            .status_notifier
            .shown()
            .map(|indicator| crate::ui::panel::PanelIndicator {
                id: indicator.item.id.clone(),
                icon: indicator.props.effective_icon().clone(),
            })
            .collect();
        if self.panel.set_app_indicators(indicators) {
            self.queue_redraw_all();
        }
    }

    pub fn refresh_popover_media(&mut self) {
        if self.panel_popover.open_role() != Some(crate::ui::panel::ROLE_DATE_MENU) {
            return;
        }
        let players = crate::ui::media_card::media_card_contents(&self.mpris);
        if self.panel_popover.set_media_players(players) {
            self.queue_redraw_all();
        }
    }

    /// Start loading every visible player's cover, and drop the ones no player claims any more.
    ///
    /// Called on any MPRIS change, **not** when the popover opens: gnome-shell builds a
    /// `MediaMessage` — and so resolves its icon — as the player appears
    /// (`js/ui/messageList.js:1780-1784`). Waiting until the popover opens would show the themed
    /// fallback for a whole round trip on a slow link.
    ///
    /// The retain is the cache's only bound: one entry per cover *played* is the only open-ended
    /// key space the image caches have.
    pub fn refresh_media_art(&mut self) {
        let mut live: std::collections::HashSet<crate::image_source::ImageSource> = self
            .mpris
            .visible()
            .filter_map(|player| player.state.art.clone())
            .collect();
        // The avatar shares this cache and so shares its only bound. Leaving it out of the live
        // set would have any MPRIS change at all evict the lock screen's picture, which is a decode
        // that only comes back on a frame that has already drawn the fallback in its place.
        live.extend(self.avatar_source());
        self.image_cache.retain(|source| live.contains(source));

        // Warm at the art slot's size for every scale a card could be drawn at, which is what the
        // cache is keyed on — warming the wrong scale is a decode nobody reads.
        for source in &live {
            for output in self.global_space.outputs() {
                let scale = output.current_scale().fractional_scale();
                self.image_cache.warm(
                    source,
                    crate::render_helpers::icon::ImageFit::Contain,
                    crate::ui::notification_card::BODY_ICON,
                    scale,
                );
            }
        }
    }

    /// Whether the unlock dialog shows the "Log in as another user" button.
    ///
    /// All four of GNOME's conditions (`unlockDialog.js:921-926`), and every one of them can veto:
    /// the seat must be able to host another session, the machine must have somebody else to log in
    /// as, the user must not have turned it off, and the administrator must not have locked it
    /// down. A button that appears when any of these is false is a button that does nothing, on the
    /// one screen where a control that does nothing is alarming.
    pub fn switch_user_visible(&self) -> bool {
        // Through the shield's own copy, not `gnome_settings`: that is the one kept current at
        // runtime (`set_settings` on every settings change), and reading the other would make the
        // button ignore a lockdown that took effect after startup.
        let settings = self.screen_shield.settings();
        self.can_switch_user
            && self.multiple_users
            && settings.user_switch_enabled
            && !settings.disable_user_switching
    }

    /// Whether the switch-user button is reactive *right now* — visible, and the prompt page has
    /// any presence at all.
    ///
    /// GNOME gates the button's `reactive`/`can_focus` on `progress > 0`, the same number that
    /// drives its opacity (`unlockDialog.js:811-821`), so it is clickable exactly while it is
    /// drawn. Gating on the model's *page* instead splits the two in both directions: through the
    /// 300 ms fade-out the page already reads `Clock` while the button is still on screen, and for
    /// the frame after the prompt is raised the page reads `Prompt` while the button is still at
    /// alpha 0 — a click on something invisible.
    pub fn switch_user_reactive(&self, now: Duration) -> bool {
        self.switch_user_visible() && self.lock_screen.page_progress(now) > 0.
    }

    /// The account picture as an image source, if AccountsService gave us one that is on disk.
    pub fn avatar_source(&self) -> Option<crate::image_source::ImageSource> {
        self.user_account
            .icon_file
            .as_ref()
            .map(|icon| crate::image_source::ImageSource::File(icon.path.clone()))
    }

    /// Decode the account picture at every output's scale, at the size the unlock prompt draws it.
    ///
    /// Keyed on the scale like every other image, so warming the wrong one is a decode nobody
    /// reads — hence per output rather than once.
    pub fn warm_avatar(&self) {
        let Some(source) = self.avatar_source() else {
            return;
        };
        for output in self.global_space.outputs() {
            let scale = output.current_scale().fractional_scale();
            self.image_cache.warm(
                &source,
                crate::render_helpers::icon::ImageFit::Cover,
                crate::ui::lock_screen::AVATAR_PX,
                scale,
            );
        }
    }

    /// Ask the CalendarServer watcher to load the day range the calendar shows —
    /// the open popover's grid, or (closed) today's month, matching gnome-shell's
    /// per-rebuild `requestRange` (`js/ui/calendar.js:748`). The watcher dedups,
    /// so calling this liberally (startup, open, month paging) is free.
    pub fn sync_calendar_range(&self) {
        let Some(tx) = &self.calendar_range_emit else {
            return;
        };
        let (since, until) = match self.panel_popover.date_menu() {
            Some(dm) => dm.calendar.grid_range(),
            None => crate::ui::calendar::today_grid_range(self.gnome_settings.calendar.week_start),
        };
        let _ =
            tx.send_blocking(crate::calendar_events::SynoikToCalendar::SetRange { since, until });
    }

    /// Rebuild the open Events section for the calendar's selected day and push
    /// it, after a store change, popover open, or month/day navigation
    /// (`EventsSection.setDate` on `selected-date-changed` / open,
    /// `js/ui/dateMenu.js:900-915`). Gated on the dateMenu popover being open.
    pub fn refresh_popover_calendar_events(&mut self) {
        let Some(dm) = self.panel_popover.date_menu() else {
            return;
        };
        let selected = dm.calendar.selected();
        // SAFETY: `time(NULL)` reads the wall clock (the events section shows
        // real dates, like the calendar grid's own `today()`).
        let now_secs = unsafe { libc::time(std::ptr::null_mut()) } as i64;
        let is_24h = self.gnome_settings.clock.hour24;
        let model = crate::ui::calendar::events_section_model(
            &self.calendar_events,
            selected,
            now_secs,
            is_24h,
        );
        if self.panel_popover.set_calendar_events(model) {
            self.queue_redraw_all();
        }
    }

    /// Rebuild the open World Clocks section at the current instant and push it,
    /// on popover open, a settings change, or the panel's per-minute clock tick
    /// (gnome-shell refreshes the labels on `WallClock notify::clock`,
    /// `js/ui/dateMenu.js:508-521`). Gated on the dateMenu popover being open.
    pub fn refresh_popover_world_clocks(&mut self) {
        if self.panel_popover.date_menu().is_none() {
            return;
        }
        // SAFETY: `time(NULL)` reads the wall clock (world clocks show live times).
        let now_secs = unsafe { libc::time(std::ptr::null_mut()) } as i64;
        let is_24h = self.gnome_settings.clock.hour24;
        let model = crate::world_clocks::world_clocks_model(
            &self.gnome_settings.world_clocks.locations,
            self.gnome_settings.world_clocks.clocks_installed,
            now_secs,
            is_24h,
        );
        if self.panel_popover.set_world_clocks(model) {
            self.queue_redraw_all();
        }
    }

    fn user_is_idle(&mut self) -> bool {
        let now = self.clock.now_unadjusted();
        self.idle_monitor.idletime_ms(now) > crate::ui::notification_banner::IDLE_TIME_MS
    }

    /// A clicked action (button, or a body click resolving to the default
    /// action) routed to the notification's D-Bus origin. The token is a real
    /// XDG activation token so the app can activate itself with it.
    ///
    /// - fdo (`js/ui/notificationDaemon.js:224-236,310-316`): `ActivationToken` then
    ///   `ActionInvoked`, unicast to the notification's sender.
    /// - `org.gtk.Notifications` (`js/ui/notificationDaemon.js:453-465`): the body-click pseudo-key
    ///   `"default"` resolves to the payload's stored default-action string; the resulting key
    ///   routes to the app (`app.` prefix, slice 2) or broadcasts `ActionInvoked` — the server
    ///   decides.
    ///
    /// Returns whether the shell itself activated the app, so the caller can leave the
    /// overview: gnome-shell hides it from `GtkNotificationDaemonAppSource.activateAction`
    /// (`:512-519`) but *not* from the fdo path, which only emits a signal and leaves
    /// raising to the app.
    #[must_use]
    pub fn emit_notification_action(&mut self, id: u32, action: String) -> bool {
        use crate::notifications::{GtkToNotifications, NotifKind, SynoikToNotifications};
        let Some(notification) = self.notifications.find(id) else {
            return false;
        };
        let kind = notification.kind.clone();
        let sender = notification.sender.clone();
        let (token, _) = self.activation_state.create_external_token(None);
        let token = token.as_str().to_owned();

        match kind {
            NotifKind::Fdo => {
                if let Some(tx) = &self.notifications_emit {
                    let _ = tx.send_blocking(SynoikToNotifications::ActionInvoked {
                        id,
                        action,
                        token,
                        sender,
                    });
                }
                false
            }
            // Ours: there is no bus name to signal at, so the action runs here.
            kind @ NotifKind::Shell { .. } => {
                let Some(resolved) = crate::notifications::shell_action_for(&kind, &action) else {
                    return false;
                };
                crate::utils::run_shell_notification_action(&resolved);
                // The shell just raised an app, which is the case gnome-shell leaves the overview
                // for (`GtkNotificationDaemonAppSource.activateAction`,
                // `js/ui/notificationDaemon.js:512-519`) — the fdo path, which only signals, does
                // not.
                true
            }
            NotifKind::Gtk {
                app_id,
                gtk_id,
                default_action,
            } => {
                // Resolve the body-click pseudo-key to the real default action.
                let action = if action == "default" {
                    match default_action {
                        Some(action) => action,
                        None => return false,
                    }
                } else {
                    action
                };
                if let Some(tx) = &self.gtk_notifications_emit {
                    let _ = tx.send_blocking(GtkToNotifications::ActionInvoked {
                        app_id,
                        gtk_id,
                        action,
                        token,
                    });
                    return true;
                }
                false
            }
        }
    }

    /// A body click on a notification with NO default action: gnome-shell's
    /// `source.open()`. For an `org.gtk.Notifications` source this D-Bus-activates
    /// the app (`js/ui/notificationDaemon.js:539`); fdo app-focus is deferred (no
    /// window tracker). Call BEFORE `activate_source`, which destroys the card.
    ///
    /// Returns whether the app was actually activated, so the caller can leave the
    /// overview. gnome-shell hides it inside `openApp`, *after* the `app == null`
    /// early return (`:375-381`) — no app to raise, no reason to leave.
    #[must_use]
    pub fn open_notification_app(&mut self, id: u32) -> bool {
        use crate::notifications::{GtkToNotifications, NotifKind};
        let Some(NotifKind::Gtk { app_id, .. }) =
            self.notifications.find(id).map(|n| n.kind.clone())
        else {
            return false;
        };
        let (token, _) = self.activation_state.create_external_token(None);
        let token = token.as_str().to_owned();
        if let Some(tx) = &self.gtk_notifications_emit {
            let _ = tx.send_blocking(GtkToNotifications::Activate { app_id, token });
            return true;
        }
        false
    }

    /// Ask Settings to show `panel` — gnome-shell's `launchSettingsPanel`
    /// (`js/ui/status/network.js:66-76`), which activates the app's own `launch-panel`
    /// action rather than spawning `gnome-control-center <panel>`.
    ///
    /// This only *selects the panel* (and D-Bus-activates Settings if it is stopped). It does
    /// **not** raise an already-open Settings, so the caller does the raise itself; see
    /// [`Self::launch_settings_panel`]'s caller in `apply_popover_action`.
    pub fn launch_settings_panel(&mut self, panel: &str, args: Vec<String>) {
        use crate::notifications::GtkToNotifications;
        let (token, _) = self.activation_state.create_external_token(None);
        let token = token.as_str().to_owned();
        if let Some(tx) = &self.gtk_notifications_emit {
            let _ = tx.send_blocking(GtkToNotifications::LaunchSettingsPanel {
                panel: panel.to_owned(),
                args,
                token,
            });
        }
    }

    /// Pop and show the next queued banner if the surface is free (hidden, no
    /// popover open, GNOME mode, an output to show on).
    pub fn maybe_show_banner(&mut self) {
        if !self.layout.is_gnome_mode() || !self.notification_banner.can_show() {
            return;
        }
        // The banner's `blocked` flag is only synced to the popover once per
        // frame (`advance_animations`), but this runs eagerly from the D-Bus
        // path — a Notify landing in the same calloop cycle as a popover open
        // must not banner (and self-acknowledge) over the open menu, so check
        // the popover directly.
        if self.panel_popover.is_open() {
            return;
        }
        // The active output stands in for GNOME's primary monitor
        // (`js/ui/messageTray.js:709-729`), fixed for the banner's lifetime.
        let Some(output) = self.layout.active_output().cloned() else {
            return;
        };
        let Some(id) = self.notifications.pop_next_banner() else {
            return;
        };
        let now = self.clock.now_unadjusted();
        let Some(content) = crate::ui::notification_card::content_for(&self.notifications, id, now)
        else {
            return;
        };
        let idle = self.user_is_idle();
        // Output-local pointer position, for the popped-up-under-the-pointer
        // hover-expand guard (`js/ui/messageTray.js:1149-1156`).
        let pointer = self
            .seat
            .get_pointer()
            .map(|p| p.current_location())
            .and_then(|loc| {
                let geo = self.global_space.output_geometry(&output)?;
                geo.contains(Point::from((loc.x as i32, loc.y as i32)))
                    .then(|| loc - geo.loc.to_f64())
            });
        self.notification_banner
            .show(content, output, idle, pointer);
        self.reschedule_notification_banner_timer();
        self.queue_redraw_all();
    }

    /// Re-arm the banner's expiry wake-up to its current deadline (or cancel it).
    /// The deadline itself is checked against the (pinnable) clock in
    /// `advance_animations`; this timer only wakes an otherwise idle loop.
    pub fn reschedule_notification_banner_timer(&mut self) {
        if let Some(token) = self.notification_banner_timer.take() {
            self.event_loop.remove(token);
        }
        let Some(deadline) = self.notification_banner.next_wakeup() else {
            return;
        };
        let now = self.clock.now_unadjusted();
        let timer = Timer::from_duration(deadline.saturating_sub(now));
        let token = self
            .event_loop
            .insert_source(timer, move |_, _, state| {
                state.synoik.notification_banner_timer = None;
                // The frame's advance_animations re-checks the deadline.
                state.synoik.queue_redraw_all();
                TimeoutAction::Drop
            })
            .unwrap();
        self.notification_banner_timer = Some(token);
    }

    /// The OSD's equivalent of
    /// [`reschedule_notification_banner_timer`](Self::reschedule_notification_banner_timer):
    /// wake the loop at the earliest armed 1500 ms hide deadline.
    pub fn reschedule_osd_timer(&mut self) {
        if let Some(token) = self.osd_timer.take() {
            self.event_loop.remove(token);
        }
        self.osd_timer_at = self.osd.next_wakeup();
        let Some(deadline) = self.osd_timer_at else {
            return;
        };
        let now = self.clock.now_unadjusted();
        let timer = Timer::from_duration(deadline.saturating_sub(now));
        let token = self
            .event_loop
            .insert_source(timer, move |_, _, state| {
                state.synoik.osd_timer = None;
                state.synoik.osd_timer_at = None;
                // The frame's advance_animations re-checks the deadline.
                state.synoik.queue_redraw_all();
                TimeoutAction::Drop
            })
            .unwrap();
        self.osd_timer = Some(token);
    }

    /// Wake the loop at the dock's auto-hide deadline: a dock resting on screen produces no
    /// frames of its own for the grace period to expire on.
    pub fn reschedule_dock_timer(&mut self) {
        if let Some(token) = self.dock_timer.take() {
            self.event_loop.remove(token);
        }
        self.dock_timer_at = self.dock.next_wakeup();
        let Some(deadline) = self.dock_timer_at else {
            return;
        };
        let now = self.clock.now_unadjusted();
        let timer = Timer::from_duration(deadline.saturating_sub(now));
        let token = self
            .event_loop
            .insert_source(timer, move |_, _, state| {
                state.synoik.dock_timer = None;
                state.synoik.dock_timer_at = None;
                // The frame's advance_animations re-checks the deadline.
                state.synoik.queue_redraw_all();
                TimeoutAction::Drop
            })
            .unwrap();
        self.dock_timer = Some(token);
    }

    /// Wake the loop at the switcher's next deadline — the open delay, the no-modifier commit,
    /// or hover coming back on.
    ///
    /// Without this the popup would sit invisible for its whole session: nothing else redraws
    /// while a modifier is merely being *held*, so the 150 ms reveal has no other event to ride.
    pub fn reschedule_switcher_timer(&mut self) {
        if let Some(token) = self.switcher_timer.take() {
            self.event_loop.remove(token);
        }
        self.switcher_timer_at = self.switcher.next_deadline();
        let Some(deadline) = self.switcher_timer_at else {
            return;
        };
        let now = self.clock.now_unadjusted();
        let timer = Timer::from_duration(deadline.saturating_sub(now));
        let token = self
            .event_loop
            .insert_source(timer, move |_, _, state| {
                state.synoik.switcher_timer = None;
                state.synoik.switcher_timer_at = None;

                let now = state.synoik.clock.now_unadjusted();
                let outcome = state.synoik.switcher.advance(now);
                state.finish_switcher(outcome);
                state.hide_osd_for_switcher();
                state.synoik.queue_redraw_all();
                TimeoutAction::Drop
            })
            .unwrap();
        self.switcher_timer = Some(token);
    }

    /// Start the idle clock over: stamp the activity time, fire the user-active watches, re-arm
    /// the timed ones (`meta_idle_monitor_reset_idletime`, `meta-idle-monitor.c:454-490`).
    ///
    /// Not the same thing as [`notify_activity`](Self::notify_activity), which is what real input
    /// does: this speaks for the *D-Bus* idle monitor alone, because its callers are not claiming
    /// the user touched anything. mutter draws the same line — its resume path resets the core
    /// monitor and nothing else (`meta-backend-native.c:1027-1028`).
    pub fn reset_idletime(&mut self, now: Duration) {
        let fired = self.idle_monitor.on_activity(now);
        self.emit_idle_watch_fired(&fired);
        self.reschedule_idle_monitor_timer();
    }

    /// Re-arm the single idle-watch timer to the earliest pending deadline (or cancel it if none).
    /// Idempotent; call after anything that changes the watch set or the last-activity time.
    pub fn reschedule_idle_monitor_timer(&mut self) {
        if let Some(token) = self.idle_monitor_timer.take() {
            self.event_loop.remove(token);
        }
        let Some(deadline) = self.idle_monitor.next_wakeup() else {
            return;
        };
        // A deadline already in the past (e.g. a watch added while long idle) yields a zero delay,
        // which fires on the next loop iteration — mutter fires such a watch at its next dispatch.
        let delay = deadline.saturating_sub(self.clock.now_unadjusted());
        let timer = Timer::from_duration(delay);
        let token = self
            .event_loop
            .insert_source(timer, move |_, _, state| {
                state.synoik.on_idle_monitor_timer();
                TimeoutAction::Drop
            })
            .unwrap();
        self.idle_monitor_timer = Some(token);
    }

    fn on_idle_monitor_timer(&mut self) {
        self.idle_monitor_timer = None;
        let now = self.clock.now_unadjusted();
        let fired = self.idle_monitor.refresh(now);
        self.emit_idle_watch_fired(&fired);
        // An idle watch that fired but stays registered has a *later* deadline only after the next
        // activity, so nothing new is pending now; but a second watch with a longer interval might
        // be, so always recompute.
        self.reschedule_idle_monitor_timer();
    }

    pub fn emit_idle_watch_fired(&self, fired: &[crate::idle_monitor::Fired]) {
        use zbus::names::BusName;

        if fired.is_empty() {
            return;
        }
        let Some(conn) = self.dbus.as_ref().and_then(|d| d.conn_idle_monitor.clone()) else {
            return;
        };
        for f in fired {
            let destination = match BusName::try_from(f.owner.clone()) {
                Ok(destination) => destination,
                Err(err) => {
                    warn!("invalid idle watch owner {:?}: {err:?}", f.owner);
                    continue;
                }
            };
            let res = async_io::block_on(conn.inner().emit_signal(
                Some(destination),
                "/org/gnome/Mutter/IdleMonitor/Core",
                "org.gnome.Mutter.IdleMonitor",
                "WatchFired",
                &(f.id,),
            ));
            if let Err(err) = res {
                warn!("error emitting WatchFired: {err:?}");
            }
        }
    }

    /// The user confirmed the end-session dialog (clicked the action button, pressed Enter on it,
    /// or the countdown expired): tell gnome-session to proceed and close the dialog.
    pub fn confirm_end_session(&mut self) {
        if let Some(confirmation) = self.end_session.confirm() {
            self.emit_confirmation(confirmation);
        }
        self.end_session_dialog.hide();
        self.reschedule_end_session_timer();
        self.queue_redraw_all();
    }

    /// Act on a [`Confirmation`]: settle the offline update with gnome-software first, *then* emit
    /// the signal, because what we settled decides which signal it is.
    ///
    /// Ordering is gnome-shell's (`js/ui/endSessionDialog.js:469-500`): it awaits `SetAction`
    /// before emitting, for the same reason — a `shutdown` that gnome-software accepted must
    /// become a reboot on the wire, and one it refused must not.
    fn emit_confirmation(&self, confirmation: crate::end_session::Confirmation) {
        use crate::end_session::UpdateDecision;

        let accepted = match (confirmation.updates, self.gnome_software_conn()) {
            (UpdateDecision::Install(action), Some(conn)) => {
                crate::dbus::gnome_software::set_action(&conn, action)
            }
            (UpdateDecision::Discard, Some(conn)) => {
                crate::dbus::gnome_software::cancel(&conn);
                false
            }
            // No connection to say it to, or nothing to say.
            _ => false,
        };
        self.emit_end_session_signal(confirmation.signal(accepted));
    }

    /// The session-bus connection the offline-update calls ride on. The same one the dialog's own
    /// signals use — gnome-software is addressed by name, so any session connection reaches it.
    fn gnome_software_conn(&self) -> Option<zbus::blocking::Connection> {
        self.dbus.as_ref().and_then(|d| d.conn_end_session.clone())
    }

    /// The update checkbox's state for the dialog to draw: `None` when it isn't offered at all,
    /// `Some(checked)` otherwise.
    pub fn update_checkbox(&self) -> Option<bool> {
        self.end_session
            .offers_updates()
            .then(|| self.end_session.install_updates())
    }

    /// gnome-software answered. If it says there is something pending, the checkbox appears on the
    /// dialog that is already on screen — which is also why the box is taller from here on.
    pub fn on_offline_update_state(&mut self, state: crate::end_session::OfflineUpdateState) {
        self.end_session.set_offline_update_state(state);
        self.refresh_end_session_content();
    }

    /// The user toggled the update checkbox.
    pub fn toggle_install_updates(&mut self) {
        self.end_session.toggle_install_updates();
        self.refresh_end_session_content();
    }

    /// Push the state machine's current content at the dialog widget and redraw.
    fn refresh_end_session_content(&mut self) {
        let Some(presentation) = self.end_session.presentation() else {
            return;
        };
        let now = self.clock.now_unadjusted();
        let seconds_left = self.end_session.seconds_left(now);
        let checkbox = self.update_checkbox();
        self.end_session_dialog
            .set_content(presentation, seconds_left, checkbox);
        self.queue_redraw_all();
    }

    /// Ask gnome-software what is pending, for a dialog that just opened. The answer arrives
    /// asynchronously via [`Self::on_offline_update_state`].
    fn query_offline_updates(&self) {
        let Some(conn) = self.gnome_software_conn() else {
            return;
        };
        let Some(tx) = self.offline_update_tx.clone() else {
            return;
        };
        crate::dbus::gnome_software::query_state(&conn, tx);
    }

    /// The user cancelled the end-session dialog (Cancel button or Esc): tell gnome-session to
    /// abort (`Canceled` then `Closed`, like gnome-shell) and close the dialog.
    pub fn cancel_end_session(&mut self) {
        if self.end_session.cancel() {
            self.emit_end_session_signal("Canceled");
            self.emit_end_session_signal("Closed");
        }
        self.end_session_dialog.hide();
        self.reschedule_end_session_timer();
        self.queue_redraw_all();
    }

    /// Re-arm the countdown timer: fire once a second to refresh the displayed seconds and, at the
    /// deadline, auto-confirm. Cancels the timer when no dialog is counting down.
    pub fn reschedule_end_session_timer(&mut self) {
        if let Some(token) = self.end_session_timer.take() {
            self.event_loop.remove(token);
        }
        if self.end_session.deadline().is_none() {
            return;
        }
        let timer = Timer::from_duration(Duration::from_secs(1));
        let token = self
            .event_loop
            .insert_source(timer, move |_, _, state| {
                state.synoik.on_end_session_timer();
                TimeoutAction::Drop
            })
            .unwrap();
        self.end_session_timer = Some(token);
    }

    fn on_end_session_timer(&mut self) {
        self.end_session_timer = None;
        let now = self.clock.now_unadjusted();

        if let Some(confirmation) = self.end_session.tick(now) {
            // Countdown expired: auto-confirm the default action, exactly as clicking it would —
            // including whatever the update checkbox was left saying.
            self.emit_confirmation(confirmation);
            self.end_session_dialog.hide();
            self.queue_redraw_all();
            return;
        }

        // Still counting down: update the displayed seconds and tick again in a second.
        if let Some(presentation) = self.end_session.presentation() {
            self.end_session_dialog.set_content(
                presentation,
                self.end_session.seconds_left(now),
                self.update_checkbox(),
            );
        }
        self.queue_redraw_all();
        self.reschedule_end_session_timer();
    }

    pub fn emit_end_session_signal(&self, signal: &str) {
        let Some(conn) = self.dbus.as_ref().and_then(|d| d.conn_end_session.clone()) else {
            return;
        };
        // Broadcast (no destination) — gnome-session listens on the object, like gnome-shell's
        // `this._dbusImpl.emit_signal(...)`.
        let res = async_io::block_on(conn.inner().emit_signal(
            None::<zbus::names::BusName>,
            "/org/gnome/SessionManager/EndSessionDialog",
            "org.gnome.SessionManager.EndSessionDialog",
            signal,
            &(),
        ));
        if let Err(err) = res {
            warn!("error emitting EndSessionDialog.{signal}: {err:?}");
        }
    }

    /// Ask gnome-session to start a logout/power-off/restart (the `Logout`/`PowerOff`/`Reboot`
    /// actions). gnome-session then calls `EndSessionDialog.Open` back on us.
    pub fn request_session_action(&self, request: crate::end_session::SessionRequest) {
        let Some(conn) = self.dbus.as_ref().and_then(|d| d.conn_end_session.clone()) else {
            warn!("cannot request {request:?}: no session bus connection");
            return;
        };
        crate::dbus::gnome_session::request_session_action(&conn, request);
    }

    /// The switcher's window list — `getWindows` (`altTab.js:51-61`) over our layout.
    ///
    /// DIVERGENCE: GNOME filters by the active *workspace* on the workspace manager, and places
    /// the popup on the **primary** monitor. We have no primary-monitor notion, so "the active
    /// workspace" here means the active workspace of the active output, and the popup follows
    /// that output. On a single head the two are the same; on several, GNOME would put the popup
    /// on the primary head and we put it where you are working — which is arguably better but is
    /// a divergence either way, and is recorded as one in `docs/fork/alt-tab-port.md`.
    pub fn switcher_tab_list(&self, current_workspace_only: bool) -> Vec<MappedId> {
        let active_output = self.layout.active_output();

        let mut windows = Vec::new();
        for (mon, ws_idx, ws) in self.layout.workspaces() {
            let on_active_workspace = mon.is_some_and(|mon| {
                Some(mon.output()) == active_output && mon.active_workspace_idx() == ws_idx
            });

            for mapped in ws.windows() {
                windows.push(crate::ui::switcher::window_list::SwitcherWindow {
                    id: mapped.id(),
                    focus_timestamp: mapped.get_focus_timestamp(),
                    on_active_workspace,
                    demands_attention: mapped.is_urgent(),
                    // Attached modal dialogs need mutter's `attach-modal-dialogs`, which is off by
                    // default and which we do not model — see `SwitcherWindow::attached_to`.
                    attached_to: None,
                });
            }
        }

        crate::ui::switcher::window_list::tab_list(&windows, current_workspace_only)
    }

    /// Resolve each app item's icon and name — `AppIcon` (`altTab.js:670-692`).
    ///
    /// An app the catalog cannot resolve still gets a row: it has windows, so it is switchable,
    /// and dropping it would make those windows unreachable. It just shows its id and no icon.
    pub fn switcher_app_art(
        &self,
        items: &[crate::ui::switcher::app_switcher::AppItem],
    ) -> Vec<crate::ui::switcher::ui::ItemArt> {
        items
            .iter()
            .map(|item| {
                let entry = self.app_system.lookup(&item.app_id);
                crate::ui::switcher::ui::ItemArt {
                    icon: entry.as_ref().map(|e| e.icon.clone()),
                    // A window-backed app has no entry, and its id is a synthetic `window:<n>`:
                    // show what the window calls itself instead of that.
                    label: entry.map_or_else(|| item.fallback_label.clone(), |e| e.name),
                    arrow: item.has_arrow(),
                    // The sub-list's captions, resolved now for the same reason the icons are:
                    // it is built half a second later, from the UI, with no way back here.
                    window_titles: item
                        .windows
                        .iter()
                        .map(|&id| self.window_title(id))
                        .collect(),
                }
            })
            .collect()
    }

    /// One window's title, or its app's name when it has none — the fallback `appMenu.js:283`
    /// uses for the equivalent row.
    fn window_title(&self, id: MappedId) -> String {
        use crate::utils::with_toplevel_role;

        let Some((_, mapped)) = self.layout.windows().find(|(_, m)| m.id() == id) else {
            return String::new();
        };
        let (app_id, title) = with_toplevel_role(mapped.toplevel(), |role| {
            (role.app_id.clone(), role.title.clone())
        });
        title
            .filter(|t| !t.is_empty())
            .or_else(|| {
                let sandbox = self
                    .app_system
                    .sandbox_id_cached(mapped.credentials().map(|c| c.pid));
                app_id
                    .as_deref()
                    .and_then(|id| self.app_system.app_for_window(id, sandbox))
                    .map(|e| e.name)
            })
            .unwrap_or_default()
    }

    /// Resolve each window item's badge icon and title — `WindowIcon` (`altTab.js:1002-1057`).
    ///
    /// The label is the *window title*, not the app name: two windows of one app are told apart
    /// by their titles, which is the whole reason this switcher exists alongside the app one. A
    /// window with no title falls back to its app's name, as `appMenu.js:283` does for the
    /// equivalent row.
    pub fn switcher_window_art(&self, ids: &[MappedId]) -> Vec<crate::ui::switcher::ui::ItemArt> {
        use crate::utils::with_toplevel_role;

        ids.iter()
            .map(|&id| {
                let mapped = self.layout.windows().find(|(_, m)| m.id() == id);
                let (app_id, title) = mapped.map_or((None, None), |(_, m)| {
                    with_toplevel_role(m.toplevel(), |role| {
                        (role.app_id.clone(), role.title.clone())
                    })
                });
                let sandbox = self
                    .app_system
                    .sandbox_id_cached(mapped.and_then(|(_, m)| m.credentials()).map(|c| c.pid));
                let entry = app_id
                    .as_deref()
                    .and_then(|id| self.app_system.app_for_window(id, sandbox));

                crate::ui::switcher::ui::ItemArt {
                    icon: entry.as_ref().map(|e| e.icon.clone()),
                    label: title
                        .filter(|t| !t.is_empty())
                        .or_else(|| entry.map(|e| e.name))
                        .unwrap_or_default(),
                    // The chevron belongs to the app switcher: a window switcher item *is* one
                    // window, so there is never anything to descend into — and with it the
                    // sub-list, so there are no captions to resolve either.
                    arrow: false,
                    window_titles: Vec::new(),
                }
            })
            .collect()
    }

    /// The cycler's `.cycler-highlight`, at the top of the window layer: `CyclerHighlight` is a
    /// child of `global.window_group` (`altTab.js:498`), so it frames the window it raised and
    /// stays *under* the panel and the layer-shell surfaces above it.
    fn render_cycler_highlight(&self, output: &Output, push: &mut dyn FnMut(OutputRenderElements)) {
        if self.switcher.output() != Some(output) {
            return;
        }
        let Some(rect) = self.cycler_highlight else {
            return;
        };
        self.switcher
            .render_cycler(rect, self.gnome_settings.accent_color, &mut |elem| {
                push(crate::ui::switcher::ui::SwitcherRenderElement::Solid(elem).into())
            });
    }

    /// Keep the layout's preview raise and the cycler's highlight rect in step with the popup —
    /// see [`SwitcherUi::preview_windows`](crate::ui::switcher::ui::SwitcherUi::preview_windows).
    ///
    /// Always runs the clearing half, even with nothing open: that is what un-pins the windows
    /// when the session ends.
    pub fn sync_switcher_preview(&mut self) {
        let windows: Vec<Window> = self
            .switcher
            .preview_windows()
            .into_iter()
            .filter_map(|id| self.find_window_by_id(id))
            .collect();
        self.layout.set_preview_raised(&windows);

        self.cycler_highlight = self
            .switcher
            .cycler_window()
            .and_then(|id| self.find_window_by_id(id))
            .zip(self.switcher.output().cloned())
            .and_then(|(id, output)| self.layout.window_render_rect(&id, &output));

        let Some(target) = self
            .switcher
            .workspace_preview()
            .and_then(|id| self.find_window_by_id(id))
        else {
            // No settled target — either nothing is open, or a session ended without going
            // through `finish_switcher` (a modal or the lock screen took the grab). Either way
            // the workspaces it borrowed are owed back.
            for origin in std::mem::take(&mut self.switcher_ws_preview).iter().rev() {
                self.layout.undo_workspace_preview(origin);
            }
            return;
        };

        if let Some(origin) = self.layout.preview_workspace_of(&target) {
            if !self
                .switcher_ws_preview
                .iter()
                .any(|seen| seen.output() == origin.output())
            {
                self.switcher_ws_preview.push(origin);
            }
        }
    }

    pub fn queue_redraw_switcher_output(&mut self) {
        if let Some(output) = self.switcher.output().cloned() {
            self.queue_redraw(&output);
        }
    }
}

pub struct NewClient {
    pub client: UnixStream,
    pub restricted: bool,
    pub credentials_unknown: bool,
}

pub struct ClientState {
    pub compositor_state: CompositorClientState,
    pub can_view_decoration_globals: bool,
    pub primary_selection_disabled: bool,
    /// Whether this client is denied from the restricted protocols such as security-context.
    pub restricted: bool,
    /// We cannot retrieve this client's socket credentials.
    pub credentials_unknown: bool,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

/// Bake the in-compositor GNOME wallpaper into an xray
/// [`EffectBuffer`](crate::render_helpers::effect_buffer::EffectBuffer)'s element list.
///
/// In GNOME (Floating) mode the wallpaper is drawn directly by `render_inner`, not as a layer-shell
/// background surface, so `render_layer_normal(Layer::Background)` — which fills the xray buffers —
/// never sees it. Without this the xray/blur samples an empty buffer and shows only the flat
/// `bg_color`. We fill the output-sized buffer at its origin with `zoom = 1` (the `XrayElement`
/// applies the per-window mapping and rounded clip when it samples), pushed last so the wallpaper
/// sits below the background-layer surfaces, matching `render_inner`'s draw order.
/// Renderer-agnostic via [`Wallpaper::render_dual`].
fn push_gnome_wallpaper_into_xray(
    gnome_mode: bool,
    wallpaper: &Wallpaper,
    renderer: &mut VulkanRenderer,
    buf_logical: Size<f64, Logical>,
    buf_scale: Scale<f64>,
    elements: &mut Vec<OutputRenderElements>,
) {
    if !gnome_mode {
        return;
    }

    // Radius 0: the buffer holds the raw wallpaper; the sampling `XrayElement` does the rounded
    // clip itself.
    let Some(elem) = wallpaper.render(renderer, Default::default(), buf_logical, 0., buf_scale)
    else {
        return;
    };
    // Wrap into the same `CropRenderElement<Relocate<Rescale<…>>>` the on-screen path builds, but
    // as a no-op transform (zoom 1, origin) that only crops to the buffer bounds.
    if let Some(elem) = scale_relocate_crop(elem, buf_scale, 1., Rectangle::from_size(buf_logical))
    {
        elements.push(elem.into());
    }
}

fn scale_relocate_crop<E: Element>(
    elem: E,
    output_scale: Scale<f64>,
    zoom: f64,
    ws_geo: Rectangle<f64, Logical>,
) -> Option<CropRenderElement<RelocateRenderElement<RescaleRenderElement<E>>>> {
    let ws_geo = ws_geo.to_physical_precise_round(output_scale);
    let elem = RescaleRenderElement::from_element(elem, Point::from((0, 0)), zoom);
    let elem = RelocateRenderElement::from_element(elem, ws_geo.loc, Relocate::Relative);
    CropRenderElement::from_element(elem, output_scale, ws_geo)
}

synoik_render_elements! {
    PointerRenderElements => {
        Wayland = WaylandSurfaceRenderElement<VulkanRenderer>,
        NamedPointer = MemoryRenderBufferRenderElement<VulkanRenderer>,
    }
}

synoik_render_elements! {
    WindowScreenshotRenderElement => {
        Layout = LayoutElementRenderElement,
        Pointer = RelocateRenderElement<PointerRenderElements>,
    }
}

synoik_render_elements! {
    OutputRenderElements => {
        Monitor = MonitorRenderElement,
        RescaledTile = RescaleRenderElement<TileRenderElement>,
        LayerSurface = LayerSurfaceRenderElement,
        RelocatedLayerSurface = CropRenderElement<RelocateRenderElement<RescaleRenderElement<
            LayerSurfaceRenderElement
        >>>,
        RelocatedColor = CropRenderElement<RelocateRenderElement<RescaleRenderElement<
            SolidColorRenderElement
        >>>,
        RelocatedRoundedTexture = CropRenderElement<RelocateRenderElement<RescaleRenderElement<
            RoundedTextureRenderElement<crate::render_helpers::vulkan::VkTexture>
        >>>,
        Pointer = PointerRenderElements,
        Wayland = WaylandSurfaceRenderElement<VulkanRenderer>,
        SolidColor = SolidColorRenderElement,
        ScreenshotUi = ScreenshotUiRenderElement,
        CapturedTexture = CapturedTextureRenderElement,
        Switcher = crate::ui::switcher::ui::SwitcherRenderElement,
        ExitConfirmDialog = ExitConfirmDialogRenderElement,
        RunDialog = RunDialogRenderElement,
        EndSessionDialog = EndSessionDialogRenderElement,
        PolkitDialog = crate::ui::polkit_dialog::PolkitDialogRenderElement,
        FolderDialog = crate::ui::folder_dialog::FolderDialogRenderElement,
        // CPU-rendered UI (panel, notifications) uploaded through the active renderer, so it draws
        // on GLES and the owned Vulkan renderer alike (the M1 escape hatch: `TextureRenderElement`
        // impls `RenderElement<R>` for any `R: Renderer<TextureId = T>`).
        UiTexture = TextureRenderElement<VkTexture>,
        // The panel: baked chrome, plus its background as its own solid layer.
        Panel = PanelElement,
        // Used for the CPU-rendered panels.
        RelocatedMemoryBuffer = RelocateRenderElement<MemoryRenderBufferRenderElement<VulkanRenderer>>,
        // The wallpaper drawn straight onto an output rather than into a workspace — the lock
        // screen's curtain, which has no workspace geometry to be relocated into.
        RoundedTexture = RoundedTextureRenderElement<crate::render_helpers::vulkan::VkTexture>,
        // The raised background under a dragged folder's composed icon.
        RoundedSolid = crate::render_helpers::rounded_solid::RoundedSolidRenderElement,
        // A group of elements composited at one alpha — the overview's search cross-fade.
        Offscreen = OffscreenRenderElement,
        // The window picker's per-preview chrome: close button, caption, app icon.
        PreviewChrome = crate::ui::window_preview::PreviewChromeRenderElement,
        ThumbnailChrome = crate::ui::thumbnail_chrome::ThumbnailChromeRenderElement,
        // The dash: baked chrome, plus the dock's backdrop blur under its pill.
        Dash = crate::ui::dash::DashElement,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    /// The reload timer's two branches, which are otherwise only reachable by sleeping out
    /// a real five seconds. Both failure directions are silent: a timer that always
    /// reloads makes the coalescing a no-op, and one that always re-waits never reloads,
    /// so a newly installed app simply never appears.
    #[test]
    fn the_reload_timer_waits_out_a_moved_deadline() {
        let now = Instant::now();

        // Nothing pending (the reload already ran, or was never queued): run, don't wait.
        assert_eq!(app_catalog_reload_wait(None, now), None);

        // A ping arrived mid-wait and pushed the deadline out: wait for the rest of it.
        let later = now + APP_CATALOG_RELOAD_DEBOUNCE;
        assert_eq!(app_catalog_reload_wait(Some(later), now), Some(later));

        // The deadline has passed: reload.
        assert_eq!(
            app_catalog_reload_wait(Some(now - Duration::from_millis(1)), now),
            None
        );
        // And exactly at the deadline, too — else a deadline that lands on the timer's own
        // instant would re-arm for zero and spin.
        assert_eq!(app_catalog_reload_wait(Some(now), now), None);
    }
}
